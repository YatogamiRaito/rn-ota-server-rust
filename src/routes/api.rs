use axum::{
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashSet;
use subtle::ConstantTimeEq;
use tracing::error;

use crate::cohort;
use crate::models::{Bundle, BundlePatch};
use crate::AppState;

/// `bundles.id` and `bundle_patches.base_bundle_id` are `CHAR(36) ascii_bin` (UUIDs).
const MAX_ID_LEN: usize = 36;

/// Every free-form bundle column is MySQL `TEXT`, which holds at most 65535 *bytes*.
/// A longer value is either rejected by the server (strict mode → an opaque 500) or
/// silently truncated (non-strict mode → corrupted bundle metadata, e.g. a chopped-off
/// `sig:` signature). Checking it here turns both outcomes into a clear 400.
const MAX_TEXT_BYTES: usize = 65_535;

/// Upper bound on `targetCohorts`. `check.rs` parses this array on *every* device
/// update check, so an unbounded list is a read-path amplification lever. 1000 is the
/// number of numeric cohorts that exist at all, so no real deployment can exceed it.
const MAX_TARGET_COHORTS: usize = 1000;

/// Upper bound on the number of bundles a single `POST /bundles` may carry. They all
/// go into one transaction; the stock CLI publishes one at a time.
const MAX_BUNDLES_PER_REQUEST: usize = 1000;

/// Upper bound on the number of values in a comma-separated `idIn` / `targetAppVersionIn`
/// filter. Each value becomes a bind parameter and MySQL caps a statement at 65535 of
/// those, so without a limit a long enough query string turns into a 500.
const MAX_IN_LIST_VALUES: usize = 1000;

const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 100;

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClientPatch {
    pub baseBundleId: String,
    pub baseFileHash: String,
    pub patchFileHash: String,
    pub patchStorageUri: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClientBundle {
    pub id: String,
    pub platform: String,
    pub should_force_update: bool,
    pub enabled: bool,
    pub file_hash: String,
    pub git_commit_hash: Option<String>,
    pub message: Option<String>,
    pub channel: String,
    pub storage_uri: String,
    pub target_app_version: Option<String>,
    pub fingerprint_hash: Option<String>,
    /// **The one key this response OMITS rather than nulls.** Upstream's `rowToBundle` sets
    /// `metadata: parseBundleMetadata(record.metadata)`, and `parseBundleMetadata` returns
    /// `undefined` — not `null` and not `{}` — whenever the column is NULL, holds text that
    /// is not JSON, or holds something that is not a JSON object. `JSON.stringify` then drops
    /// the key entirely. Every other field here is always present, carrying `null` when it
    /// has no value. See [`parse_bundle_metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub rollout_cohort_count: i32,
    pub target_cohorts: Option<Vec<String>>,
    pub manifest_storage_uri: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub asset_base_storage_uri: Option<String>,
    pub patches: Vec<ClientPatch>,
    pub patch_base_bundle_id: Option<String>,
    pub patch_base_file_hash: Option<String>,
    pub patch_file_hash: Option<String>,
    pub patch_storage_uri: Option<String>,
}

/// Upstream's `parseTargetCohorts` (`@hot-updater/server` `dist/db/bundleRows.mjs`):
///
/// ```js
/// const parseTargetCohorts = (value) => {
///   if (!value) return null;
///   if (Array.isArray(value)) return value.filter((item) => typeof item === "string");
///   if (typeof value !== "string") return null;
///   try {
///     const parsed = JSON.parse(value);
///     return Array.isArray(parsed) ? parsed.filter((item) => typeof item === "string") : null;
///   } catch { return null; }
/// };
/// ```
///
/// This used to be `serde_json::from_value::<Vec<String>>(v).ok()`, which is a stricter rule
/// in two ways that both cost real cohorts:
///
/// * A column holding the array as a **JSON string** (`"[\"alpha\"]"` rather than
///   `["alpha"]`) failed outright and yielded `None`. Upstream parses that second layer.
/// * A single non-string element discarded the **whole** list. Upstream filters the
///   offending entries and keeps the rest.
///
/// Either way the bundle looked untargeted, so a device that an explicit cohort list should
/// have let in fell back to the rollout percentage. Note an empty JSON array is `Some(vec![])`
/// — `[]` is truthy in JS — while an empty *string* column is `None`.
pub fn parse_target_cohorts(val: &Option<serde_json::Value>) -> Option<Vec<String>> {
    let value = val.as_ref()?;

    let only_strings = |items: &Vec<serde_json::Value>| -> Vec<String> {
        items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect()
    };

    match value {
        // `!value` in JS: null and the empty string are both as good as absent. A non-empty
        // string falls through to the JSON.parse branch below.
        serde_json::Value::Null => None,
        serde_json::Value::Array(items) => Some(only_strings(items)),
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Array(items)) => Some(only_strings(&items)),
            _ => None,
        },
        // A number, a boolean or an object is neither an array nor a string: `null`.
        _ => None,
    }
}

/// Upstream's `parseBundleMetadata` (`@hot-updater/server` `dist/db/updateArtifacts.mjs`),
/// which every one of upstream's four SQL adapters runs the `metadata` column through on the
/// way out via `rowToBundle`:
///
/// ```js
/// const parseBundleMetadata = (value) => {
///   if (!value) return;                                    // JS falsy -> undefined
///   let parsedValue = value;
///   if (typeof parsedValue === "string")
///     try { parsedValue = JSON.parse(parsedValue); } catch { return; }
///   if (!parsedValue || typeof parsedValue !== "object" || Array.isArray(parsedValue)) return;
///   return stripBundleArtifactMetadata(parsedValue);       // identity at 0.35.12
/// };
/// ```
///
/// `None` here means the key is **omitted** from the response, not serialized as `null` —
/// that is the whole point of the function, and `ClientBundle::metadata` carries the
/// `skip_serializing_if` that honours it.
///
/// The `!value` guard is JS truthiness, so an empty string, `0` and `false` are all as good
/// as absent; only a non-empty string is worth trying to parse.
pub fn parse_bundle_metadata(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let value = value?;

    // JS falsy: null, "", 0, false. (`undefined` is the `None` already returned above.)
    let falsy = match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Bool(b) => !*b,
        serde_json::Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    };
    if falsy {
        return None;
    }

    // A MySQL `JSON` column normally decodes straight to a `Value`, but a bundle written by
    // an older client (or by a `TEXT` column that was later migrated) can still hold the
    // JSON as a *string*. Upstream parses that second layer; so does this.
    let parsed = match value {
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s).ok()?,
        other => other.clone(),
    };

    // Only a plain object survives. An array, a scalar or a nested `null` is dropped.
    if parsed.is_object() {
        Some(parsed)
    } else {
        None
    }
}

// Reference: node_modules/@hot-updater/server/src/db/schemaEnhancements.ts (assertBundlePersistenceConstraints).
// Shared constraint check for both create_bundles (INSERT) AND update_bundle (PATCH, merged result row).
fn assert_bundle_persistence_constraints(
    target_app_version: &Option<String>,
    fingerprint_hash: &Option<String>,
    rollout_cohort_count: i32,
    target_cohorts: &Option<Vec<String>>,
) -> Result<(), String> {
    let normalize = |v: &Option<String>| -> Option<String> {
        v.as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if normalize(target_app_version).is_none() && normalize(fingerprint_hash).is_none() {
        return Err("Bundle must define either targetAppVersion or fingerprintHash.".to_string());
    }

    if !(0..=cohort::DEFAULT_ROLLOUT_COHORT_COUNT).contains(&rollout_cohort_count) {
        return Err(format!(
            "rolloutCohortCount must be an integer between 0 and {}.",
            cohort::DEFAULT_ROLLOUT_COHORT_COUNT
        ));
    }

    if let Some(cohorts) = target_cohorts {
        for c in cohorts {
            if !cohort::is_valid_cohort(c) {
                return Err(format!(
                    "Invalid target cohort \"{}\". {}",
                    c,
                    cohort::INVALID_COHORT_ERROR_MESSAGE
                ));
            }
        }
    }

    Ok(())
}

/// The database row → response body mapping, mirroring upstream's `rowToBundle`.
///
/// `pub` so `tests/cli_api_parity_tests.rs` can replay the recorded upstream bodies from the
/// recorded database ROWS rather than only from upstream's finished JSON — the same reason
/// `calculate_pagination` and `build_cursor_page_query` are exported. It is a pure function
/// of its two arguments and touches no state.
pub fn map_to_client_bundle(b: Bundle, patches: Vec<BundlePatch>) -> ClientBundle {
    let client_patches: Vec<ClientPatch> = patches
        .into_iter()
        .map(|p| ClientPatch {
            baseBundleId: p.base_bundle_id,
            baseFileHash: p.base_file_hash,
            patchFileHash: p.patch_file_hash,
            patchStorageUri: p.patch_storage_uri,
        })
        .collect();

    let primary_patch = client_patches.first();

    ClientBundle {
        id: b.id,
        platform: b.platform,
        should_force_update: b.should_force_update != 0,
        enabled: b.enabled != 0,
        file_hash: b.file_hash,
        git_commit_hash: b.git_commit_hash,
        message: b.message,
        channel: b.channel,
        storage_uri: b.storage_uri,
        target_app_version: b.target_app_version,
        fingerprint_hash: b.fingerprint_hash,
        metadata: parse_bundle_metadata(b.metadata.as_ref()),
        rollout_cohort_count: b.rollout_cohort_count,
        target_cohorts: parse_target_cohorts(&b.target_cohorts),
        manifest_storage_uri: b.manifest_storage_uri,
        manifest_file_hash: b.manifest_file_hash,
        asset_base_storage_uri: b.asset_base_storage_uri,
        patch_base_bundle_id: primary_patch.map(|p| p.baseBundleId.clone()),
        patch_base_file_hash: primary_patch.map(|p| p.baseFileHash.clone()),
        patch_file_hash: primary_patch.map(|p| p.patchFileHash.clone()),
        patch_storage_uri: primary_patch.map(|p| p.patchStorageUri.clone()),
        patches: client_patches,
    }
}

/// Every error this API emits is a JSON object with a single `error` key, because that is
/// what upstream emits and what the CLI parses.
///
/// `createHandler` has exactly two error shapes and both are JSON with
/// `Content-Type: application/json`:
///
/// ```js
/// // HandlerBadRequestError, and the 404 in handleGetBundle
/// new Response(JSON.stringify({ error: error.message }), { status: 400, headers: … })
/// // anything else
/// new Response(JSON.stringify({ error: "Internal server error", message: … }), { status: 500, … })
/// ```
///
/// This used to be `(StatusCode::BAD_REQUEST, "some message")`, which axum renders as a
/// bare `text/plain` body. A client doing `await res.json()` on a failure — which the
/// hot-updater CLI does — got a JSON parse error instead of the message, so every 400 this
/// server produced was unreadable to it.
fn error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

const BEARER_PREFIX: &[u8] = b"Bearer ";

/// Compare a presented bearer token with the configured one without leaking anything
/// through timing.
///
/// **Do not replace this with `==`, and do not replace it with a bare
/// `presented.ct_eq(expected)` either.** Both are wrong, for different reasons:
///
/// * `==` on `str`/`[u8]` short-circuits at the first differing byte, so the time to
///   reject a token grows with the length of the shared prefix. That is a byte-at-a-time
///   oracle for a token that can publish bundles to production mobile apps.
/// * `subtle`'s `ConstantTimeEq for [T]` returns `Choice::from(0)` immediately when the
///   two slices differ in length, which leaks the length of the *secret*.
///
/// So the loop below always runs exactly `presented.len()` iterations — a count the
/// attacker chose and therefore already knows — and indexes `expected` modulo its length
/// so that no iteration is skipped and no branch depends on secret bytes. Length equality
/// is folded into the result with `ct_eq` rather than an `if`.
fn constant_time_token_eq(presented: &[u8], expected: &[u8]) -> bool {
    if expected.is_empty() {
        // A server configured with an empty token accepts nothing. This branch depends
        // only on the configuration, never on the presented value, so it leaks nothing;
        // it exists to make the `% expected.len()` below well-defined.
        return false;
    }

    let mut diff: u8 = 0;
    for (i, byte) in presented.iter().enumerate() {
        diff |= byte ^ expected[i % expected.len()];
    }

    bool::from(diff.ct_eq(&0u8) & presented.len().ct_eq(&expected.len()))
}

/// Authorization helper.
///
/// The app lookup happens first and answers 404 for an app that is not configured. That
/// is deliberately *not* a timing/enumeration concern: app names are not secrets — they
/// sit in the path of the unauthenticated device-facing endpoints — and the lookup never
/// touches a token, so it cannot short-circuit the comparison below. The per-app token is
/// the only secret here, and it is only ever compared by `constant_time_token_eq`.
fn authorize(
    headers: &HeaderMap,
    state: &AppState,
    app_name: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let app_config = state
        .config
        .get_app_config(app_name)
        .ok_or((StatusCode::NOT_FOUND, "Application not found"))?;

    // Raw bytes, not `to_str()`: a token is an opaque byte string and a non-UTF-8 header
    // should fail the comparison, not be reported as a missing header.
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .map(|v| v.as_bytes())
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header"))?;

    // The scheme prefix is public, so matching it with an ordinary comparison leaks
    // nothing; only what follows it is secret.
    let presented = auth_header
        .strip_prefix(BEARER_PREFIX)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid authorization token"))?;

    if constant_time_token_eq(presented, app_config.auth_token.as_bytes()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid authorization token"))
    }
}

/// Validate an id that is written to a `CHAR(36) ascii_bin` column.
///
/// Without this an over-long id is truncated to 36 characters by a non-strict MySQL,
/// which can make two distinct ids collide on the primary key — and the primary key is
/// what the cross-app ownership check in `create_bundles` relies on.
fn validate_id(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty."));
    }
    if value.len() > MAX_ID_LEN {
        return Err(format!(
            "{field} must be at most {MAX_ID_LEN} characters long."
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "{field} may only contain ASCII letters, digits, '-' and '_'."
        ));
    }
    Ok(())
}

fn check_text_len(field: &str, value: Option<&String>) -> Result<(), String> {
    match value {
        Some(v) if v.len() > MAX_TEXT_BYTES => Err(format!(
            "{field} must be at most {MAX_TEXT_BYTES} bytes long."
        )),
        _ => Ok(()),
    }
}

fn check_target_cohorts_len(value: Option<&Vec<String>>) -> Result<(), String> {
    match value {
        Some(v) if v.len() > MAX_TARGET_COHORTS => Err(format!(
            "targetCohorts must contain at most {MAX_TARGET_COHORTS} entries."
        )),
        _ => Ok(()),
    }
}

/// `serde_json::to_value` on a `Vec<String>` genuinely cannot fail, but rather than leave
/// an `unwrap()` that a reader has to re-derive that from, the value is built directly.
fn cohorts_to_json(cohorts: &[String]) -> serde_json::Value {
    serde_json::Value::Array(
        cohorts
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    )
}

// 1. GET /:app/hot-updater/api/bundles/channels
pub async fn list_channels(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    let rows_result = sqlx::query("SELECT DISTINCT channel FROM bundles WHERE app_name = ?")
        .bind(&app_name)
        .fetch_all(&state.db)
        .await;

    match rows_result {
        Ok(rows) => {
            let channels: Vec<String> = rows
                .into_iter()
                .map(|r| r.get::<String, _>("channel"))
                .collect();
            Json(serde_json::json!({
                "data": {
                    "channels": channels
                }
            }))
            .into_response()
        }
        Err(err) => {
            error!("Failed to fetch channels: {}", err);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error")
        }
    }
}

// 2. GET /:app/hot-updater/api/bundles/:id
pub async fn get_bundle(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, bundle_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    let bundle_result =
        sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE app_name = ? AND id = ?")
            .bind(&app_name)
            .bind(&bundle_id)
            .fetch_optional(&state.db)
            .await;

    match bundle_result {
        Ok(Some(b)) => {
            // `app_name` is REQUIRED, not defensive. A bundle id is unique only within an
            // app now that the primary key is `(app_name, id)`, so two tenants may hold
            // the same id and an unscoped `WHERE bundle_id = ?` would return both their
            // patch sets. This predicate used to be genuinely redundant; the composite key
            // is exactly what made it load-bearing.
            let patches_result = sqlx::query_as::<_, BundlePatch>(
                // `base_bundle_id` is the tie-break, not decoration: upstream's `rowToBundle` sorts
                // with `(left.order_index ?? 0) - (right.order_index ?? 0) || left.base_bundle_id.localeCompare(right.base_bundle_id)`,
                // and `ORDER BY order_index ASC` alone leaves rows that share an index in an
                // order MySQL does not promise. patches[0] is what fills the deprecated
                // `patchBaseBundleId`/`patch*` mirror fields, so an unstable order there is an
                // unstable response body.
                "SELECT * FROM bundle_patches WHERE app_name = ? AND bundle_id = ? ORDER BY order_index ASC, base_bundle_id ASC",
            )
            .bind(&app_name)
            .bind(&bundle_id)
            .fetch_all(&state.db)
            .await;

            // See `list_bundles`: silently dropping the patch list is worse than failing.
            let patches = match patches_result {
                Ok(p) => p,
                Err(err) => {
                    error!("Failed to fetch patches for bundle {}: {}", bundle_id, err);
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
                }
            };
            Json(map_to_client_bundle(b, patches)).into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "Bundle not found"),
        Err(err) => {
            error!("Failed to fetch bundle: {}", err);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error")
        }
    }
}

// Query parameters for GET /api/bundles
//
// `limit`, `page` and `offset` are deliberately received as raw strings: upstream
// (`handler.mjs` `parsePositiveIntegerSearchParam`) answers 400 for anything that is not
// a positive integer in range, and it must be *this* 400 rather than axum's generic
// query-deserialization rejection. See `parse_positive_integer_param`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ListBundlesParams {
    pub channel: Option<String>,
    pub platform: Option<String>,
    /// Raw value; upstream accepts only `"true"`/`"false"` and 400s otherwise, so it is
    /// parsed by [`parse_boolean_param`] rather than by serde.
    #[serde(rename = "enabled")]
    pub enabled_raw: Option<String>,
    #[serde(skip)]
    pub enabled: Option<bool>,
    pub limit: Option<String>,
    pub page: Option<String>,
    /// Only ever inspected for presence — upstream removed it and 400s if it appears.
    pub offset: Option<String>,
    pub target_app_version: Option<String>,
    /// Repeated query parameter (`?targetAppVersionIn=a&targetAppVersionIn=b`), filled from
    /// the raw query string rather than by serde — see [`query_get_all`].
    #[serde(skip)]
    pub target_app_version_in: Vec<String>,
    /// Raw value; see [`parse_boolean_param`].
    #[serde(rename = "targetAppVersionNotNull")]
    pub target_app_version_not_null_raw: Option<String>,
    #[serde(skip)]
    pub target_app_version_not_null: Option<bool>,
    pub fingerprint_hash: Option<String>,
    pub id_eq: Option<String>,
    pub id_gt: Option<String>,
    pub id_gte: Option<String>,
    pub id_lt: Option<String>,
    pub id_lte: Option<String>,
    /// Repeated query parameter (`?idIn=a&idIn=b`), filled from the raw query string
    /// rather than by serde — see [`query_get_all`].
    #[serde(skip)]
    pub id_in: Vec<String>,
    pub after: Option<String>,
    pub before: Option<String>,
}

/// All values of a repeated query parameter, matching WHATWG `searchParams.getAll(key)`.
///
/// Upstream reads `idIn` / `targetAppVersionIn` with `getAll`
/// (`handler.mjs` `parseStringArraySearchParam`), so the wire form is `?idIn=a&idIn=b`.
/// There is deliberately **no** comma-splitting fallback: upstream treats `?idIn=a,b` as a
/// single id that happens to contain a comma, and accepting both forms would silently
/// mangle such an id.
///
/// Zero occurrences yield an empty vector, which upstream maps to `undefined` — the filter
/// is omitted entirely. A single empty occurrence (`?idIn=`) is *not* the same thing: it is
/// a one-element list containing the empty string, and it filters on it.
fn query_get_all(query: Option<&str>, key: &str) -> Vec<String> {
    let Some(query) = query else {
        return Vec::new();
    };
    url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .collect()
}

/// Upstream `isPlatform` (`handler.mjs:40-42`). No case folding, no trimming.
fn is_platform(value: &str) -> bool {
    value == "ios" || value == "android"
}

/// Mirror of upstream's `platform` handling in `handleGetBundles` (`handler.mjs:158`).
/// `Err` carries the 400 message verbatim.
///
/// Only an absent parameter, `ios` and `android` are accepted. The empty string is *not*
/// treated as absent here — it produces the slightly odd but faithful
/// `Invalid platform: . Expected 'ios' or 'android'.`
pub fn parse_platform_param(raw: Option<&str>) -> Result<Option<String>, String> {
    match raw {
        None => Ok(None),
        Some(value) if is_platform(value) => Ok(Some(value.to_string())),
        Some(value) => Err(format!(
            "Invalid platform: {value}. Expected 'ios' or 'android'."
        )),
    }
}

/// Mirror of upstream `parseStringArraySearchParam` (`handler.mjs:60-63`): the result of
/// `searchParams.getAll(key)`, or `undefined` when the parameter never appeared.
///
/// The distinction that matters: zero occurrences give `None` (no filter at all), while a
/// single empty occurrence (`?idIn=`) gives `Some([""])` — a filter on the empty string
/// that matches nothing. Duplicates and ordering are preserved verbatim; the values are
/// opaque strings, never a delimited list.
pub fn parse_string_array_param(values: &[String]) -> Option<Vec<String>> {
    if values.is_empty() {
        None
    } else {
        Some(values.to_vec())
    }
}

/// Mirror of upstream `parseBooleanSearchParam` (`handler.mjs:48-54`). `Err` carries the
/// 400 message verbatim.
///
/// Only the exact strings `"true"` and `"false"` are accepted — no `1`/`0`, no `yes`/`no`,
/// no case folding, and the empty string is a 400 rather than "absent". Without this the
/// rejection came from axum's generic query deserializer, which answers 400 too but with a
/// different body.
pub fn parse_boolean_param(key: &str, raw: Option<&str>) -> Result<Option<bool>, String> {
    match raw {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(format!(
            "The '{key}' query parameter must be 'true' or 'false'."
        )),
    }
}

/// The three states a `parseNullableStringSearchParam` parameter can be in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NullableStringParam {
    /// The parameter never appeared; upstream returns `undefined` and the `!== undefined`
    /// guard drops the filter entirely.
    Absent,
    /// The literal lowercase text `null`, which upstream turns into JS `null` — an
    /// `IS NULL` filter, not a match on the four characters.
    Null,
    /// Any other value, including the empty string.
    Value(String),
}

/// Mirror of upstream `parseNullableStringSearchParam` (`handler.mjs:55-59`):
/// ```js
/// if (value === null) return;            // absent
/// return value === "null" ? null : value;
/// ```
///
/// It governs **both** `targetAppVersion` and `fingerprintHash`, which is why they share
/// this helper rather than each carrying their own inline check — that is exactly how they
/// drifted apart before (`?targetAppVersion=null` meant `IS NULL` while
/// `?fingerprintHash=null` bound the literal string `'null'`).
///
/// Two neighbours worth being explicit about, because an "obvious" implementation gets
/// them wrong: the comparison is **case-sensitive**, so `?fingerprintHash=NULL` is a filter
/// on the four-character string `NULL` and not `IS NULL`; and `?fingerprintHash=` is a
/// filter on the empty string, not an absent parameter, because the guard on these two
/// fields is `!== undefined` rather than truthiness.
pub fn parse_nullable_string_param(raw: Option<&str>) -> NullableStringParam {
    match raw {
        None => NullableStringParam::Absent,
        Some("null") => NullableStringParam::Null,
        Some(value) => NullableStringParam::Value(value.to_string()),
    }
}

/// Mirror of the JS-truthiness fold upstream applies to `channel`, `platform` and the
/// scalar id filters (`...channel && { channel }`, `handler.mjs:161-169`): an absent **or**
/// empty value drops the filter entirely, and every other non-empty string is kept —
/// including `"0"` and `" "`, which are truthy in JS because only `""` is falsy among
/// strings.
///
/// **This is not the same rule as [`parse_nullable_string_param`], and the two must not be
/// merged.** The same input means different things in the same handler:
/// `?targetAppVersion=null` is an `IS NULL` filter, while `?channel=null` is a filter on
/// the literal four-character string `null`. `targetAppVersion` / `fingerprintHash` are
/// guarded with `!== undefined` and go through the nullable helper; everything else is
/// folded through truthiness and goes through this one.
pub fn parse_truthy_string_param(raw: Option<&str>) -> Option<String> {
    raw.filter(|s| !s.is_empty()).map(str::to_string)
}

/// Reject filter lists long enough to blow past MySQL's bind-parameter ceiling.
/// Checked before any query is built so the caller gets a 400 instead of a 500.
fn validate_list_params(params: &ListBundlesParams) -> Result<(), String> {
    for (field, values) in [
        ("idIn", &params.id_in),
        ("targetAppVersionIn", &params.target_app_version_in),
    ] {
        if values.len() > MAX_IN_LIST_VALUES {
            return Err(format!(
                "{field} must contain at most {MAX_IN_LIST_VALUES} values."
            ));
        }
    }
    Ok(())
}

fn apply_filters<'a>(qb: &mut sqlx::QueryBuilder<'a, sqlx::MySql>, params: &'a ListBundlesParams) {
    // `channel`, `platform` and the scalar id filters go through JS truthiness upstream, so
    // an empty value is dropped rather than matched on — see `truthy`.
    if let Some(chan) = parse_truthy_string_param(params.channel.as_deref()) {
        qb.push(" AND channel = ");
        qb.push_bind(chan);
    }
    if let Some(plat) = parse_truthy_string_param(params.platform.as_deref()) {
        qb.push(" AND platform = ");
        qb.push_bind(plat);
    }
    if let Some(enabled) = params.enabled {
        qb.push(" AND enabled = ");
        qb.push_bind(if enabled { 1 } else { 0 });
    }
    // `fingerprintHash` and `targetAppVersion` are guarded with `!== undefined` upstream,
    // not truthiness, and both go through `parseNullableStringSearchParam` — so they share
    // one helper here and cannot drift apart again.
    for (column, raw) in [
        ("fingerprint_hash", &params.fingerprint_hash),
        ("target_app_version", &params.target_app_version),
    ] {
        match parse_nullable_string_param(raw.as_deref()) {
            NullableStringParam::Absent => {}
            NullableStringParam::Null => {
                qb.push(" AND ");
                qb.push(column);
                qb.push(" IS NULL");
            }
            NullableStringParam::Value(value) => {
                qb.push(" AND ");
                qb.push(column);
                qb.push(" = ");
                qb.push_bind(value);
            }
        }
    }
    if let Some(versions) = parse_string_array_param(&params.target_app_version_in) {
        qb.push(" AND target_app_version IN (");
        let mut separated = qb.separated(", ");
        for v in versions {
            separated.push_bind(v);
        }
        qb.push(")");
    }
    if let Some(not_null) = params.target_app_version_not_null {
        if not_null {
            qb.push(" AND target_app_version IS NOT NULL");
        } else {
            qb.push(" AND target_app_version IS NULL");
        }
    }

    // ID Filters
    if let Some(eq) = parse_truthy_string_param(params.id_eq.as_deref()) {
        qb.push(" AND id = ");
        qb.push_bind(eq);
    }
    if let Some(gt) = parse_truthy_string_param(params.id_gt.as_deref()) {
        qb.push(" AND id > ");
        qb.push_bind(gt);
    }
    if let Some(gte) = parse_truthy_string_param(params.id_gte.as_deref()) {
        qb.push(" AND id >= ");
        qb.push_bind(gte);
    }
    if let Some(lt) = parse_truthy_string_param(params.id_lt.as_deref()) {
        qb.push(" AND id < ");
        qb.push_bind(lt);
    }
    if let Some(lte) = parse_truthy_string_param(params.id_lte.as_deref()) {
        qb.push(" AND id <= ");
        qb.push_bind(lte);
    }
    // `Some([""])` (from `?idIn=`) is a real filter on the empty string that matches
    // nothing; only a parameter that never appeared drops the clause.
    if let Some(ids) = parse_string_array_param(&params.id_in) {
        qb.push(" AND id IN (");
        let mut separated = qb.separated(", ");
        for id in ids {
            separated.push_bind(id);
        }
        qb.push(")");
    }

    // NOTE: the `after` / `before` cursor predicates are deliberately NOT applied here.
    // Upstream (`createDatabasePlugin.mjs:141-146`) computes `total` from the base WHERE
    // with the cursor excluded, and applies the cursor only to the page query — so a
    // cursor must never narrow `pagination.total`. `list_bundles` pushes the cursor
    // predicate itself, from the plan returned by `build_cursor_page_query`.
}

// ---------------------------------------------------------------------------------------
// Upstream pagination semantics.
//
// These three functions are direct ports of `@hot-updater/plugin-core@0.35.8`
// (`dist/calculatePagination.mjs`, `dist/createDatabasePlugin.mjs`) and
// `@hot-updater/server@0.35.8` (`dist/handler.mjs`). They are `pub` and free of any
// database or HTTP dependency so the recorded-upstream fixtures can be replayed against
// them directly, the way `decide_update` is. Behaviour that looks wrong here is upstream
// behaviour and is reproduced on purpose; see the individual notes.
// ---------------------------------------------------------------------------------------

/// The `pagination` object upstream returns, minus the two cursor keys (those are omitted
/// rather than nulled, so they are built separately at serialization time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaginationMeta {
    pub total: i64,
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub current_page: i64,
    pub total_pages: i64,
}

/// Mirror of upstream `calculatePagination(total, { limit, offset })`
/// (`plugin-core/dist/calculatePagination.mjs`).
///
/// `current_page` is `floor(offset / limit) + 1`, which is *incoherent* for a cursor page
/// that is not limit-aligned — `before=b4, limit=2` over b1..b7 reports `currentPage: 1`
/// together with `hasPreviousPage: true`. That is what upstream returns and it is
/// reproduced deliberately; do not "fix" it.
pub fn calculate_pagination(total: i64, limit: i64, offset: i64) -> PaginationMeta {
    if total == 0 {
        return PaginationMeta {
            total: 0,
            has_next_page: false,
            has_previous_page: false,
            current_page: 1,
            total_pages: 0,
        };
    }

    // JS divides by a float and never traps; Rust's integer division panics on zero.
    // A non-positive limit cannot arrive here (the parser rejects it with a 400), so this
    // is purely a guard against a panic, not a behavioural choice.
    let limit = limit.max(1);

    PaginationMeta {
        total,
        has_next_page: offset.saturating_add(limit) < total,
        has_previous_page: offset > 0,
        current_page: offset / limit + 1,
        total_pages: total / limit + i64::from(total % limit != 0),
    }
}

/// Which `id` predicate the cursor page query carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdFilter {
    Gt(String),
    Lt(String),
    None,
}

/// The rewritten page query upstream derives from a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPlan {
    /// `before` fetches the page in the *opposite* order and reverses the rows afterwards.
    pub reverse_data: bool,
    pub id_filter: IdFilter,
    /// Ordering of the page query itself (not of the result).
    pub ascending: bool,
}

/// Mirror of upstream `buildCursorPageQuery(where, cursor, orderBy)`
/// (`plugin-core/dist/createDatabasePlugin.mjs:32-52`).
///
/// `after` keeps the requested direction; `before` flips it, walks backwards from the
/// cursor and reverses the rows, which is what makes `before` return the page immediately
/// preceding the cursor instead of the first page. When both are given, `after` wins —
/// upstream tests `cursor.after` first.
pub fn build_cursor_page_query(
    after: Option<&str>,
    before: Option<&str>,
    descending: bool,
) -> CursorPlan {
    // JS truthiness: `if (cursor.after)` is false for an empty string, so `?after=` is not
    // a cursor at all — it falls through to the no-cursor branch. `handler.mjs` agrees
    // (`after || before ? { after, before } : void 0` builds no cursor object either).
    let after = after.filter(|s| !s.is_empty());
    let before = before.filter(|s| !s.is_empty());

    if let Some(after) = after {
        return CursorPlan {
            reverse_data: false,
            id_filter: if descending {
                IdFilter::Lt(after.to_string())
            } else {
                IdFilter::Gt(after.to_string())
            },
            ascending: !descending,
        };
    }

    if let Some(before) = before {
        return CursorPlan {
            reverse_data: true,
            id_filter: if descending {
                IdFilter::Gt(before.to_string())
            } else {
                IdFilter::Lt(before.to_string())
            },
            // Direction flipped relative to the requested ordering.
            ascending: descending,
        };
    }

    CursorPlan {
        reverse_data: false,
        id_filter: IdFilter::None,
        ascending: !descending,
    }
}

/// Mirror of upstream `parsePositiveIntegerSearchParam(url, key, defaultValue, maxValue)`
/// (`server/dist/handler.mjs:64-70`). `Err` carries the 400 message verbatim.
///
/// The parse emulates JS `Number(value)` rather than Rust's `i64::from_str`, because the
/// upstream contract is defined in terms of it: whitespace is trimmed, `""` is `0` (and so
/// rejected), `0x`/`0o`/`0b` prefixes are honoured, and exponent/fraction forms such as
/// `1e2` or `5.0` are accepted when they land on a whole number. `Infinity` and `NaN`
/// parse but fail the integer test, exactly as upstream.
pub fn parse_positive_integer_param(
    key: &str,
    raw: Option<&str>,
    default_value: i64,
    max_value: i64,
) -> Result<i64, String> {
    let Some(raw) = raw else {
        return Ok(default_value);
    };

    let rejected = || {
        Err(format!(
            "The '{key}' query parameter must be a positive integer between 1 and {max_value}."
        ))
    };

    let Some(parsed) = js_number(raw) else {
        return rejected();
    };
    if !(parsed.is_finite() && parsed.fract() == 0.0) {
        return rejected();
    }
    if parsed < 1.0 || parsed > max_value as f64 {
        return rejected();
    }

    Ok(parsed as i64)
}

/// JS `Number(string)` for the subset that can appear in a query parameter. `None` stands
/// for `NaN`.
fn js_number(raw: &str) -> Option<f64> {
    // JS trims whitespace and maps the empty string to 0.
    let s = raw.trim_matches(|c: char| c.is_whitespace());
    if s.is_empty() {
        return Some(0.0);
    }

    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    if let Some(oct) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8).ok().map(|v| v as f64);
    }
    if let Some(bin) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2).ok().map(|v| v as f64);
    }

    // Rust's f64 parser accepts `inf`/`infinity`/`nan` in any case; JS `Number` does not
    // (only the exact `Infinity` spelling), and every one of them fails the integer test
    // below anyway, so they are all rejected here.
    let lower = s.to_ascii_lowercase();
    let lower = lower.trim_start_matches(['+', '-']);
    if lower.starts_with("inf") || lower.starts_with("nan") {
        return None;
    }

    s.parse::<f64>().ok()
}

/// Upstream parses `page` separately from `limit` and with **no** upper bound
/// (`handler.mjs:155,157`): any non-positive or non-integer value is a 400, and the
/// message differs from the `limit` one.
///
/// Not part of the three-function contract agreed with the fixtures agent, but exported
/// alongside them because `page` 400s are recorded from the same handler.
pub fn parse_page_param(raw: Option<&str>) -> Result<Option<i64>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let invalid = || Err("The 'page' query parameter must be a positive integer.".to_string());

    let Some(parsed) = js_number(raw) else {
        return invalid();
    };
    if !(parsed.is_finite() && parsed.fract() == 0.0) || parsed <= 0.0 {
        return invalid();
    }

    // A page far beyond i64 (upstream would carry it as a float) is saturated: the offset
    // it produces is clamped to the last page either way, so the response is identical.
    Ok(Some(if parsed >= i64::MAX as f64 {
        i64::MAX
    } else {
        parsed as i64
    }))
}

/// The two optional cursor keys of the `pagination` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorKeys {
    pub next_cursor: Option<String>,
    pub previous_cursor: Option<String>,
}

/// Mirror of upstream's cursor-key rule, covering both branches that produce one.
///
/// Normal page (`createPaginatedResult`, `createDatabasePlugin.mjs:61-62`):
/// ```js
/// const nextCursor     = data.length > 0 && startIndex + data.length < total ? data.at(-1)?.id : void 0;
/// const previousCursor = data.length > 0 && startIndex > 0 ? data[0]?.id : void 0;
/// ```
/// Note `startIndex + data.length`, the number of rows *actually* returned — not
/// `offset + limit`. The two differ on a short final page: `after=b2, limit=2` over b1..b7
/// leaves one row at `startIndex 6`, and `6 + 1 < 7` is false, so `nextCursor` is
/// suppressed where `6 + 2 < 7` would also have been false but `offset + limit` reasoning
/// on a full page would not.
///
/// Exhausted cursor (`createDatabasePlugin.mjs:164-176`): the page is empty and the
/// supplied cursor is echoed back on the opposite side — `after` as `previousCursor`,
/// `before` as `nextCursor`. Both are echoed if both were supplied.
///
/// `after` / `before` must be passed **only when the request actually took the cursor
/// branch**: a `page=` query ignores the cursor entirely upstream and goes through
/// `createPaginatedResult`, which has no echo. Empty strings are treated as absent, since
/// they are falsy in JS.
pub fn derive_cursor_keys(
    total: i64,
    start_index: i64,
    page_ids: &[String],
    after: Option<&str>,
    before: Option<&str>,
) -> CursorKeys {
    let after = after.filter(|s| !s.is_empty());
    let before = before.filter(|s| !s.is_empty());

    if page_ids.is_empty() {
        return CursorKeys {
            next_cursor: before.map(String::from),
            previous_cursor: after.map(String::from),
        };
    }

    let row_count = page_ids.len() as i64;
    CursorKeys {
        next_cursor: if start_index.saturating_add(row_count) < total {
            page_ids.last().cloned()
        } else {
            None
        },
        previous_cursor: if start_index > 0 {
            page_ids.first().cloned()
        } else {
            None
        },
    }
}

const OFFSET_REMOVED_MESSAGE: &str =
    "The 'offset' query parameter has been removed. Use 'after' or 'before' cursor pagination instead.";

// 3. GET /:app/hot-updater/api/bundles
pub async fn list_bundles(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
    RawQuery(raw_query): RawQuery,
    Query(mut params): Query<ListBundlesParams>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    // The two list filters are repeated parameters, which serde_urlencoded cannot express;
    // they are read straight off the raw query string instead.
    params.id_in = query_get_all(raw_query.as_deref(), "idIn");
    params.target_app_version_in = query_get_all(raw_query.as_deref(), "targetAppVersionIn");

    if let Err(message) = validate_list_params(&params) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    // Upstream rejects an unknown platform here rather than returning an empty list
    // (`handler.mjs:158`).
    if let Err(message) = parse_platform_param(params.platform.as_deref()) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    params.enabled = match parse_boolean_param("enabled", params.enabled_raw.as_deref()) {
        Ok(v) => v,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    params.target_app_version_not_null = match parse_boolean_param(
        "targetAppVersionNotNull",
        params.target_app_version_not_null_raw.as_deref(),
    ) {
        Ok(v) => v,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    // Upstream removed offset pagination outright and rejects the parameter even when it
    // would have been harmless (`handler.mjs:156`).
    if params.offset.is_some() {
        return error_response(StatusCode::BAD_REQUEST, OFFSET_REMOVED_MESSAGE);
    }

    // Upstream validates rather than clamps: `limit=0`, `limit=-1` and `limit=101` are all
    // 400s, not silently corrected values.
    let limit = match parse_positive_integer_param(
        "limit",
        params.limit.as_deref(),
        DEFAULT_PAGE_SIZE,
        MAX_PAGE_SIZE,
    ) {
        Ok(v) => v,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let page = match parse_page_param(params.page.as_deref()) {
        Ok(v) => v,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    // `total` is computed from the base filters with the cursor deliberately excluded
    // (`createDatabasePlugin.mjs:141-146`), so paging never shrinks it.
    let mut count_builder =
        sqlx::QueryBuilder::new("SELECT COUNT(*) FROM bundles WHERE app_name = ");
    count_builder.push_bind(&app_name);
    apply_filters(&mut count_builder, &params);

    let total = match count_builder
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await
    {
        Ok(t) => t,
        Err(err) => {
            // Used to be `unwrap_or(0)`, which reported `total: 0` next to a non-empty
            // `data` array and swallowed the error entirely.
            error!("Failed to count bundles: {}", err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    // An empty cursor string is falsy in JS and is therefore not a cursor at all; every
    // branch below has to agree with `build_cursor_page_query` about that.
    let after = params.after.as_deref().filter(|s| !s.is_empty());
    let before = params.before.as_deref().filter(|s| !s.is_empty());

    // A page query and a cursor query are mutually exclusive upstream: whenever `page` is
    // defined the cursor is ignored entirely (`createDatabasePlugin.mjs:225-236`).
    let cursor_plan = if page.is_some() || (after.is_none() && before.is_none()) {
        None
    } else {
        Some(build_cursor_page_query(after, before, true))
    };

    let offset = match page {
        Some(page) => {
            // Upstream clamps a too-large page to the last one and re-queries, rather than
            // returning an empty page (`createDatabasePlugin.mjs:228-234`).
            let total_pages = if total == 0 {
                0
            } else {
                total / limit + i64::from(total % limit != 0)
            };
            let max_offset = if total_pages == 0 {
                0
            } else {
                (total_pages.max(1) - 1).saturating_mul(limit)
            };
            page.saturating_sub(1).saturating_mul(limit).min(max_offset)
        }
        None => 0,
    };

    let mut query_builder = sqlx::QueryBuilder::new("SELECT * FROM bundles WHERE app_name = ");
    query_builder.push_bind(&app_name);
    apply_filters(&mut query_builder, &params);

    let ascending = match &cursor_plan {
        Some(plan) => {
            match &plan.id_filter {
                IdFilter::Gt(id) => {
                    query_builder.push(" AND id > ");
                    query_builder.push_bind(id.clone());
                }
                IdFilter::Lt(id) => {
                    query_builder.push(" AND id < ");
                    query_builder.push_bind(id.clone());
                }
                IdFilter::None => {}
            }
            plan.ascending
        }
        None => false,
    };

    query_builder.push(if ascending {
        " ORDER BY id ASC LIMIT "
    } else {
        " ORDER BY id DESC LIMIT "
    });
    query_builder.push_bind(limit);
    query_builder.push(" OFFSET ");
    query_builder.push_bind(offset);

    let query = query_builder.build_query_as::<Bundle>();
    let bundles_result = query.fetch_all(&state.db).await;

    let mut bundles = match bundles_result {
        Ok(b) => b,
        Err(err) => {
            error!("Failed to fetch bundles list: {}", err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    // `before` walked the rows backwards, so put them back into the requested order.
    if cursor_plan.as_ref().is_some_and(|p| p.reverse_data) {
        bundles.reverse();
    }

    // An exhausted cursor is not an error upstream: the cursor is echoed back on the
    // opposite side and `startIndex` jumps to `total` for `after` / `0` for `before`
    // (`createDatabasePlugin.mjs:164-178`).
    // The index of the page's first row in the full ordering. Upstream gets it from a
    // dedicated count query (`createDatabasePlugin.mjs:178-185`); for a page query it is
    // simply the resolved offset, and for an exhausted cursor it jumps to `total` (for
    // `after`) or back to 0 (for `before`) — `createDatabasePlugin.mjs:165`.
    let start_index = match (cursor_plan.as_ref(), bundles.first()) {
        (Some(_), None) => {
            if after.is_some() {
                total
            } else {
                0
            }
        }
        (Some(_), Some(first)) => {
            let mut before_builder =
                sqlx::QueryBuilder::new("SELECT COUNT(*) FROM bundles WHERE app_name = ");
            before_builder.push_bind(&app_name);
            apply_filters(&mut before_builder, &params);
            // DESC ordering: the rows "before" the first one are those with a greater id.
            before_builder.push(" AND id > ");
            before_builder.push_bind(first.id.clone());

            match before_builder
                .build_query_scalar::<i64>()
                .fetch_one(&state.db)
                .await
            {
                Ok(v) => v,
                Err(err) => {
                    error!("Failed to count bundles before the cursor page: {}", err);
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
                }
            }
        }
        _ => offset,
    };

    // Load patches for all returned bundles
    let mut client_bundles = Vec::new();
    if !bundles.is_empty() {
        let bundle_ids: Vec<String> = bundles.iter().map(|b| b.id.clone()).collect();
        // Scoped by `app_name` for the same reason as in `get_bundle`: bundle ids repeat
        // across apps under the `(app_name, id)` primary key, so an `IN` list of ids alone
        // would pull in another tenant's patch rows.
        let mut patches_builder =
            sqlx::QueryBuilder::new("SELECT * FROM bundle_patches WHERE app_name = ");
        patches_builder.push_bind(&app_name);
        patches_builder.push(" AND bundle_id IN (");
        let mut separated = patches_builder.separated(", ");
        for id in &bundle_ids {
            separated.push_bind(id);
        }
        // Same tie-break as `get_bundle` -- see the comment there.
        patches_builder.push(") ORDER BY order_index ASC, base_bundle_id ASC");

        let patches_query = patches_builder.build_query_as::<BundlePatch>();
        // A failure here used to fall back to "no patches", which is not a harmless
        // default: a bundle that silently loses its patch list makes the device download
        // a full bundle (or resolve the wrong base). Report the failure instead.
        let patches = match patches_query.fetch_all(&state.db).await {
            Ok(p) => p,
            Err(err) => {
                error!("Failed to fetch patches for bundle list: {}", err);
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
            }
        };

        for b in bundles {
            // Case-insensitive to match the `ascii_general_ci` collation on these columns:
            // the `IN` query above already matched case-insensitively, so a byte-wise
            // grouping here would silently drop patch rows whose stored `bundle_id` differs
            // in case from the bundle's own `id` (possible after an upsert, which never
            // rewrites `bundles.id`). Revisit if those columns move to a `_bin` collation.
            let b_patches: Vec<BundlePatch> = patches
                .iter()
                .filter(|p| p.bundle_id.eq_ignore_ascii_case(&b.id))
                .cloned()
                .collect();
            client_bundles.push(map_to_client_bundle(b, b_patches));
        }
    }

    let meta = calculate_pagination(total, limit, start_index);
    // The cursors are only handed to `derive_cursor_keys` when the request actually took
    // the cursor branch: a `page=` query ignores them upstream and gets no echo.
    let (echo_after, echo_before) = if cursor_plan.is_some() {
        (after, before)
    } else {
        (None, None)
    };
    let page_ids: Vec<String> = client_bundles.iter().map(|cb| cb.id.clone()).collect();
    let keys = derive_cursor_keys(total, start_index, &page_ids, echo_after, echo_before);

    Json(serde_json::json!({
        "data": client_bundles,
        "pagination": pagination_json(&meta, &keys),
    }))
    .into_response()
}

/// Serialize a [`PaginationMeta`] plus its [`CursorKeys`] the way upstream does: the two
/// cursor keys are **omitted** when absent (`...nextCursor ? { nextCursor } : {}`), never
/// emitted as `null`.
fn pagination_json(meta: &PaginationMeta, keys: &CursorKeys) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("total".into(), meta.total.into());
    obj.insert("hasNextPage".into(), meta.has_next_page.into());
    obj.insert("hasPreviousPage".into(), meta.has_previous_page.into());
    obj.insert("currentPage".into(), meta.current_page.into());
    obj.insert("totalPages".into(), meta.total_pages.into());
    if let Some(next) = &keys.next_cursor {
        obj.insert("nextCursor".into(), next.clone().into());
    }
    if let Some(previous) = &keys.previous_cursor {
        obj.insert("previousCursor".into(), previous.clone().into());
    }
    serde_json::Value::Object(obj)
}

// 4. POST /:app/hot-updater/api/bundles
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CLIPatch {
    pub base_bundle_id: String,
    pub base_file_hash: String,
    pub patch_file_hash: String,
    pub patch_storage_uri: String,
}

/// Upstream's `readBundlePatchArray` + `getBundlePatches` (`@hot-updater/core`
/// `dist/index.mjs`), which is what `bundleToPatchRows` feeds off when a bundle is written:
///
/// ```js
/// const isBundlePatchArtifact = (value) =>
///   !!value && typeof value === "object" && !Array.isArray(value) &&
///   typeof value.baseBundleId === "string" && typeof value.baseFileHash === "string" &&
///   typeof value.patchFileHash === "string" && typeof value.patchStorageUri === "string";
/// const readBundlePatchArray = (patches) =>
///   Array.isArray(patches) ? patches.filter(isBundlePatchArtifact) : [];
/// const getBundlePatches = (bundle) => { /* drop repeats of a baseBundleId, keep the first */ };
/// ```
///
/// **Malformed entries are DROPPED, not rejected**, and a `patches` value that is not an
/// array at all is simply no patches. A strict deserialize here would answer 400 for a
/// payload upstream publishes happily, so `CLIBundle::patches` is held as a raw
/// [`serde_json::Value`] and filtered through this function instead.
///
/// The de-duplication is not cosmetic: the patch primary key is `"{bundle_id}:{base_bundle_id}"`,
/// so two patches sharing a base collide on it. Upstream drops the repeat; keeping both would
/// be a duplicate-key 500.
///
/// One deliberate widening of upstream's rule: the "seen" comparison is ASCII-case-insensitive,
/// because the id columns carry `ascii_general_ci` (see `docs/upstream-parity.md` §3.3) and
/// MySQL therefore considers `…AAA` and `…aaa` the *same* primary key. Upstream, comparing
/// case-sensitively, would keep both and hand the database a duplicate key. Revert this to a
/// plain comparison if those columns ever move to a `_bin` collation.
pub fn get_bundle_patches(patches: Option<&serde_json::Value>) -> Vec<CLIPatch> {
    let Some(serde_json::Value::Array(items)) = patches else {
        return Vec::new();
    };

    let string_field = |item: &serde_json::Value, key: &str| -> Option<String> {
        item.get(key).and_then(|v| v.as_str()).map(str::to_string)
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();

    for item in items {
        // Every one of the four fields must be a string, exactly as `isBundlePatchArtifact`
        // requires. A missing field, a null, or a number drops the whole entry.
        let (
            Some(base_bundle_id),
            Some(base_file_hash),
            Some(patch_file_hash),
            Some(patch_storage_uri),
        ) = (
            string_field(item, "baseBundleId"),
            string_field(item, "baseFileHash"),
            string_field(item, "patchFileHash"),
            string_field(item, "patchStorageUri"),
        )
        else {
            continue;
        };

        if !seen.insert(base_bundle_id.to_ascii_lowercase()) {
            continue;
        }

        out.push(CLIPatch {
            base_bundle_id,
            base_file_hash,
            patch_file_hash,
            patch_storage_uri,
        });
    }

    out
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CLIBundle {
    pub id: String,
    pub platform: String,
    pub should_force_update: Option<bool>,
    pub enabled: Option<bool>,
    pub file_hash: String,
    pub git_commit_hash: Option<String>,
    pub message: Option<String>,
    pub channel: Option<String>,
    pub storage_uri: String,
    pub target_app_version: Option<String>,
    pub fingerprint_hash: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub rollout_cohort_count: Option<i32>,
    pub target_cohorts: Option<Vec<String>>,
    pub manifest_storage_uri: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub asset_base_storage_uri: Option<String>,
    /// Held raw and filtered through [`get_bundle_patches`]: upstream drops malformed
    /// entries and tolerates a non-array value rather than rejecting the request, and a
    /// `Vec<CLIPatch>` here would answer 400 for payloads upstream accepts.
    pub patches: Option<serde_json::Value>,
}

/// Field-level validation for one incoming bundle, run before the transaction opens so
/// a bad payload never leaves half-written rows behind.
fn validate_cli_bundle(cb: &CLIBundle) -> Result<(), String> {
    validate_id("id", &cb.id)?;

    check_text_len("platform", Some(&cb.platform))?;
    check_text_len("fileHash", Some(&cb.file_hash))?;
    check_text_len("storageUri", Some(&cb.storage_uri))?;
    check_text_len("gitCommitHash", cb.git_commit_hash.as_ref())?;
    check_text_len("message", cb.message.as_ref())?;
    check_text_len("channel", cb.channel.as_ref())?;
    check_text_len("targetAppVersion", cb.target_app_version.as_ref())?;
    check_text_len("fingerprintHash", cb.fingerprint_hash.as_ref())?;
    check_text_len("manifestStorageUri", cb.manifest_storage_uri.as_ref())?;
    check_text_len("manifestFileHash", cb.manifest_file_hash.as_ref())?;
    check_text_len("assetBaseStorageUri", cb.asset_base_storage_uri.as_ref())?;
    check_target_cohorts_len(cb.target_cohorts.as_ref())?;

    // `get_bundle_patches` has already dropped the malformed entries and the repeated bases
    // the way upstream does, so what is left is only the column-width and id-shape check
    // that MySQL needs and upstream's schema-less path does not.
    for p in get_bundle_patches(cb.patches.as_ref()) {
        validate_id("patches[].baseBundleId", &p.base_bundle_id)?;
        check_text_len("patches[].baseFileHash", Some(&p.base_file_hash))?;
        check_text_len("patches[].patchFileHash", Some(&p.patch_file_hash))?;
        check_text_len("patches[].patchStorageUri", Some(&p.patch_storage_uri))?;
    }

    Ok(())
}

/// Does this app have a bundle with this id?
///
/// Scoped by `app_name` because a bundle id is only meaningful within an app now that the
/// primary key is `(app_name, id)`. "Belongs to another app" and "does not exist" are the
/// same answer from here, which is the point: a caller cannot distinguish them, so it
/// cannot probe for another tenant's ids.
///
/// No `FOR UPDATE`. The composite foreign key is what actually guarantees a patch cannot
/// outlive or cross to another app's bundle; this lookup exists only to turn a dangling
/// reference into a 400 instead of letting it surface as a foreign-key 500.
async fn app_owns_bundle(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    app_name: &str,
    bundle_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM bundles WHERE app_name = ? AND id = ?")
        .bind(app_name)
        .bind(bundle_id)
        .fetch_optional(&mut **tx)
        .await
        .map(|found| found.is_some())
}

pub async fn create_bundles(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    // Parse payload as either a single bundle object or an array of bundles
    let cli_bundles: Vec<CLIBundle> = if body.is_array() {
        match serde_json::from_value(body) {
            Ok(list) => list,
            Err(_) => {
                return error_response(StatusCode::BAD_REQUEST, "Invalid bundles array payload")
            }
        }
    } else {
        match serde_json::from_value(body) {
            Ok(single) => vec![single],
            Err(_) => {
                return error_response(StatusCode::BAD_REQUEST, "Invalid bundle object payload")
            }
        }
    };

    if cli_bundles.len() > MAX_BUNDLES_PER_REQUEST {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("At most {MAX_BUNDLES_PER_REQUEST} bundles may be sent in one request."),
        );
    }

    // Validate the whole payload up front: no row is written unless every bundle in the
    // request is acceptable.
    for cb in &cli_bundles {
        if let Err(message) = validate_cli_bundle(cb) {
            return error_response(StatusCode::BAD_REQUEST, message);
        }
        if let Err(message) = assert_bundle_persistence_constraints(
            &cb.target_app_version,
            &cb.fingerprint_hash,
            cb.rollout_cohort_count.unwrap_or(1000),
            &cb.target_cohorts,
        ) {
            return error_response(StatusCode::BAD_REQUEST, message);
        }
    }

    // Run transaction
    let mut tx = match state.db.begin().await {
        Ok(t) => t,
        Err(err) => {
            error!("Failed to begin transaction: {}", err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    for cb in cli_bundles {
        let should_force_update = if cb.should_force_update.unwrap_or(false) {
            1
        } else {
            0
        };
        let enabled = if cb.enabled.unwrap_or(true) { 1 } else { 0 };
        let channel = cb.channel.unwrap_or_else(|| "production".to_string());
        let rollout_cohort_count = cb.rollout_cohort_count.unwrap_or(1000);

        let target_cohorts_json = cb.target_cohorts.as_deref().map(cohorts_to_json);
        let metadata_json = cb.metadata.unwrap_or_else(|| serde_json::json!({}));

        // CROSS-APP ISOLATION is structural here, not checked. The primary key is
        // `(app_name, id)`, so `ON DUPLICATE KEY UPDATE` below can only ever match a row
        // this app already owns -- there is no statement it could write that reaches
        // another tenant's bundle. An earlier revision needed a row-locked ownership
        // lookup in front of this upsert and answered 409 when the id belonged to another
        // app; that guard is gone deliberately, and its 409 with it. Under the composite
        // key an id used by another tenant is simply not this app's business: the two are
        // separate rows, and refusing the write would leak the fact that someone else's
        // bundle carries that id.
        // Upsert bundle
        let insert_bundle_result = sqlx::query(
            r#"
            INSERT INTO bundles (
                id, app_name, platform, should_force_update, enabled, file_hash,
                git_commit_hash, message, channel, storage_uri, target_app_version,
                fingerprint_hash, metadata, rollout_cohort_count, target_cohorts,
                manifest_storage_uri, manifest_file_hash, asset_base_storage_uri
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
                platform = VALUES(platform),
                should_force_update = VALUES(should_force_update),
                enabled = VALUES(enabled),
                file_hash = VALUES(file_hash),
                git_commit_hash = VALUES(git_commit_hash),
                message = VALUES(message),
                channel = VALUES(channel),
                storage_uri = VALUES(storage_uri),
                target_app_version = VALUES(target_app_version),
                fingerprint_hash = VALUES(fingerprint_hash),
                metadata = VALUES(metadata),
                rollout_cohort_count = VALUES(rollout_cohort_count),
                target_cohorts = VALUES(target_cohorts),
                manifest_storage_uri = VALUES(manifest_storage_uri),
                manifest_file_hash = VALUES(manifest_file_hash),
                asset_base_storage_uri = VALUES(asset_base_storage_uri)
            "#,
        )
        .bind(&cb.id)
        .bind(&app_name)
        .bind(&cb.platform)
        .bind(should_force_update)
        .bind(enabled)
        .bind(&cb.file_hash)
        .bind(&cb.git_commit_hash)
        .bind(&cb.message)
        .bind(&channel)
        .bind(&cb.storage_uri)
        .bind(&cb.target_app_version)
        .bind(&cb.fingerprint_hash)
        .bind(&metadata_json)
        .bind(rollout_cohort_count)
        .bind(&target_cohorts_json)
        .bind(&cb.manifest_storage_uri)
        .bind(&cb.manifest_file_hash)
        .bind(&cb.asset_base_storage_uri)
        .execute(&mut *tx)
        .await;

        if let Err(err) = insert_bundle_result {
            error!("Failed to upsert bundle {}: {}", cb.id, err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database write error");
        }

        // Delete previous patches for the bundle. `app_name` is essential: without it this
        // statement would delete the patch rows of every app that happens to use the same
        // bundle id, which the composite primary key now permits.
        let delete_patches_result =
            sqlx::query("DELETE FROM bundle_patches WHERE app_name = ? AND bundle_id = ?")
                .bind(&app_name)
                .bind(&cb.id)
                .execute(&mut *tx)
                .await;

        if let Err(err) = delete_patches_result {
            error!("Failed to clear patches for bundle {}: {}", cb.id, err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database write error");
        }

        // Insert new patches
        {
            let patches = get_bundle_patches(cb.patches.as_ref());
            for (index, p) in patches.into_iter().enumerate() {
                let patch_id = format!("{}:{}", cb.id, p.base_bundle_id);
                let order_index = index as i32;

                // A cross-app base bundle is impossible here: the foreign key is
                // `(app_name, base_bundle_id) -> bundles(app_name, id)`, so the database
                // rejects one outright. This lookup is about the error the caller sees --
                // a 400 naming the problem rather than a foreign-key violation surfacing
                // as a 500 -- and it deliberately cannot tell "no such bundle" apart from
                // "another app's bundle".
                match app_owns_bundle(&mut tx, &app_name, &p.base_bundle_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            "Patch baseBundleId does not refer to a bundle of this application",
                        )
                            .into_response();
                    }
                    Err(err) => {
                        error!(
                            "Failed to resolve base bundle {}: {}",
                            p.base_bundle_id, err
                        );
                        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
                    }
                }

                let insert_patch_result = sqlx::query(
                    r#"
                    INSERT INTO bundle_patches (
                        id, app_name, bundle_id, base_bundle_id, base_file_hash, patch_file_hash, patch_storage_uri, order_index
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    "#
                )
                .bind(&patch_id)
                .bind(&app_name)
                .bind(&cb.id)
                .bind(&p.base_bundle_id)
                .bind(&p.base_file_hash)
                .bind(&p.patch_file_hash)
                .bind(&p.patch_storage_uri)
                .bind(order_index)
                .execute(&mut *tx)
                .await;

                if let Err(err) = insert_patch_result {
                    error!(
                        "Failed to insert patch {} for bundle {}: {}",
                        patch_id, cb.id, err
                    );
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Database write error",
                    );
                }
            }
        }
    }

    if let Err(err) = tx.commit().await {
        error!("Failed to commit bundle save transaction: {}", err);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database commit error");
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "success": true })),
    )
        .into_response()
}

// 5. PATCH /:app/hot-updater/api/bundles/:id

/// Upstream's `requireBundlePatchPayload` (`@hot-updater/server` `dist/handler.mjs`), which
/// is the entire HTTP-layer contract for a PATCH body:
///
/// ```js
/// const requireBundlePatchPayload = (payload, bundleId) => {
///   if (!payload || typeof payload !== "object" || Array.isArray(payload))
///     throw new HandlerBadRequestError("Invalid bundle payload");
///   const bundlePatch = payload;
///   if (bundlePatch.id !== void 0 && bundlePatch.id !== bundleId)
///     throw new HandlerBadRequestError("Bundle id mismatch");
///   const { id: _ignoredId, ...rest } = bundlePatch;
///   return rest;
/// };
/// ```
///
/// reached from `handleUpdateBundle` as `requireBundlePatchPayload(Array.isArray(body) ? body[0] : body, bundleId)`.
///
/// Three details are easy to get wrong and each has a recorded fixture case:
///
/// * An **array body collapses to its first element**. `PATCH [{"enabled":false}]` is a
///   valid patch, not a malformed one; an empty array yields `undefined` and is the
///   *"Invalid bundle payload"* branch.
/// * The id guard is `!== void 0`, so an explicit **`"id": null` counts as present** and is
///   therefore a mismatch. Only an absent `id`, or one equal to the route id, gets through.
/// * A matching `id` is **stripped**, so `id` never reaches the update itself.
pub fn require_bundle_patch_payload(
    body: serde_json::Value,
    bundle_id: &str,
) -> Result<serde_json::Value, &'static str> {
    let candidate = match body {
        serde_json::Value::Array(mut items) => {
            if items.is_empty() {
                serde_json::Value::Null
            } else {
                items.swap_remove(0)
            }
        }
        other => other,
    };

    let serde_json::Value::Object(mut map) = candidate else {
        return Err("Invalid bundle payload");
    };

    // `!== void 0`: a present-but-null id is a mismatch, not an absent one.
    if let Some(id) = map.get("id") {
        if id != &serde_json::Value::String(bundle_id.to_string()) {
            return Err("Bundle id mismatch");
        }
    }
    map.remove("id");

    Ok(serde_json::Value::Object(map))
}

/// Distinguish "the key was absent" from "the key was present and null".
///
/// A plain `Option<T>` collapses the two — serde folds a JSON `null` to `None`, which reads as
/// "leave unchanged". Upstream does not: `mergeBundleUpdate` skips only `undefined`, so a
/// present `null` is assigned and **clears the column**. Without this wrapper there is no way
/// to clear a nullable field through the API at all: the operator sends
/// `{"message": null}`, gets `200 {"success":true}`, and the row is unchanged.
///
/// `Option<Option<T>>`: outer `None` = absent, `Some(None)` = explicit null, `Some(Some(v))` =
/// a value. Every field needs `#[serde(default)]` as well, or an absent key is an error
/// instead of `None`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

/// The columns upstream's `v0_31_0` schema declares nullable, and therefore the only ones an
/// explicit `null` may clear. `channel`, `metadata` and `rollout_cohort_count` are NOT NULL
/// *with defaults* — the first two of those are handled separately below — and the rest are
/// required outright. `migrations/20260714000000_init.sql` matches this exactly.
const NOT_NULL_PATCH_COLUMNS: &[&str] = &[
    "platform",
    "shouldForceUpdate",
    "enabled",
    "fileHash",
    "channel",
    "storageUri",
];

/// Upstream's `mergeBundleUpdate` (`@hot-updater/plugin-core` `dist/createDatabasePlugin.mjs`)
/// is an es-toolkit `mergeWith` whose customizer only intercepts `REPLACE_ON_UPDATE_KEYS`:
///
/// ```js
/// const REPLACE_ON_UPDATE_KEYS = ["patches", "targetCohorts"];
/// function mergeBundleUpdate(baseBundle, patch) {
///   return mergeWith({ ...baseBundle }, patch, (_t, sourceValue, key) => {
///     if (REPLACE_ON_UPDATE_KEYS.includes(key)) return sourceValue;
///   });
/// }
/// ```
///
/// So `patches` and `targetCohorts` are **replaced whole**, and everything else — in practice
/// `metadata`, the only other structured column — is **deep merged**. This function is that
/// deep merge, and every rule below is a recorded case in `tests/fixtures/cli_api_fixtures.json`:
///
/// | stored | patch | result | case |
/// | --- | --- | --- | --- |
/// | `{"b":2,"nested":{"x":1}}` | `{"a":1}` | `{"b":2,"nested":{"x":1},"a":1}` | *metadata is DEEP merged, not replaced* |
/// | `{"nested":{"x":1}}` | `{"nested":{"y":2}}` | `{"nested":{"x":1,"y":2}}` | *merged key by key* |
/// | `{"b":2}` | `{}` | `{"b":2}` | *an empty object leaves the stored keys in place* |
/// | `{"a":{"deep":1}}` | `{"a":"flat"}` | `{"a":"flat"}` | *overwrites an object with a scalar* |
/// | `{"a":[1,2,3]}` | `{"a":[9]}` | **`{"a":[9,2,3]}`** | *array is merged index by index* |
/// | `{"a":1}` | `{"a":null}` | `{"a":null}` | *value set to null explicitly* |
///
/// **The array row is not a typo and this is not a bug to fix.** es-toolkit walks arrays
/// index by index like any other object, so a shorter patch array leaves the tail of the
/// stored one behind, and a metadata key can never be *removed* — only overwritten. Anyone
/// "correcting" this into a replace breaks parity; the recorded cases are the proof, and
/// `tests/cli_api_parity_tests.rs` replays them.
pub fn merge_bundle_metadata(
    current: Option<&serde_json::Value>,
    patch: &serde_json::Value,
) -> serde_json::Value {
    let Some(current) = current else {
        return patch.clone();
    };

    match (current, patch) {
        (serde_json::Value::Object(base), serde_json::Value::Object(source)) => {
            let mut merged = base.clone();
            for (key, value) in source {
                let next = match merged.get(key) {
                    Some(existing) => merge_bundle_metadata(Some(existing), value),
                    None => value.clone(),
                };
                merged.insert(key.clone(), next);
            }
            serde_json::Value::Object(merged)
        }
        // Index-by-index, keeping the stored tail. See the table above.
        (serde_json::Value::Array(base), serde_json::Value::Array(source)) => {
            let mut merged = base.clone();
            for (index, value) in source.iter().enumerate() {
                let next = match merged.get(index) {
                    Some(existing) => merge_bundle_metadata(Some(existing), value),
                    None => value.clone(),
                };
                if index < merged.len() {
                    merged[index] = next;
                } else {
                    merged.push(next);
                }
            }
            serde_json::Value::Array(merged)
        }
        // Mismatched kinds, or a leaf: the patch wins outright, `null` included.
        _ => patch.clone(),
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBundlePayload {
    // Every field is a double option so that an explicit `null` — which upstream assigns and
    // which therefore CLEARS the column — is distinguishable from an absent key.
    #[serde(default, deserialize_with = "double_option")]
    pub platform: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub should_force_update: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub enabled: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    pub file_hash: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub git_commit_hash: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub message: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub channel: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub storage_uri: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub target_app_version: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub fingerprint_hash: Option<Option<String>>,
    /// Deep merged into the stored value, never replaced — see [`merge_bundle_metadata`].
    /// An explicit `null` resets the column to `{}` rather than to SQL NULL: `bundleToRow`
    /// writes `stripBundleArtifactMetadata(bundle.metadata) ?? {}` and the column is NOT NULL.
    #[serde(default, deserialize_with = "double_option")]
    pub metadata: Option<Option<serde_json::Value>>,
    /// An explicit `null` resets this to the default 1000, not to SQL NULL: `bundleToRow`
    /// writes `bundle.rolloutCohortCount ?? DEFAULT_ROLLOUT_COHORT_COUNT`.
    #[serde(default, deserialize_with = "double_option")]
    pub rollout_cohort_count: Option<Option<i32>>,
    /// **REPLACED, never merged** — `targetCohorts` is in upstream's `REPLACE_ON_UPDATE_KEYS`
    /// alongside `patches`. Do not "helpfully" merge this with the stored list.
    #[serde(default, deserialize_with = "double_option")]
    pub target_cohorts: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    pub manifest_storage_uri: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub manifest_file_hash: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub asset_base_storage_uri: Option<Option<String>>,
    /// **REPLACED, never merged**, for the same reason as `target_cohorts`: both sit in
    /// upstream's `REPLACE_ON_UPDATE_KEYS`. Present-and-empty (or null) CLEARS the patch set;
    /// absent leaves it alone. Held raw and filtered through [`get_bundle_patches`], like
    /// `CLIBundle::patches`.
    #[serde(default, deserialize_with = "double_option")]
    pub patches: Option<Option<serde_json::Value>>,
}

/// Same column-width checks as `validate_cli_bundle` — PATCH writes to exactly the same
/// `TEXT` columns — plus the guard against nulling a NOT NULL column.
///
/// **Upstream has no equivalent of that second guard.** `bundleToRow` passes `platform`,
/// `shouldForceUpdate`, `enabled`, `fileHash`, `channel` and `storageUri` through verbatim, so
/// `PATCH {"channel": null}` hands the adapter a `NULL` for a `NOT NULL` column and the
/// *database* rejects it. The recorded cases show exactly that null reaching the row
/// (`tests/fixtures/cli_api_fixtures.json`, "channel set to null (a NOT NULL column with a
/// default)" and its four siblings) — but the fixture's in-memory store enforces no
/// constraints, so upstream's HTTP answer is genuinely **unobservable** at that boundary. Both
/// ends agree the request fails; answering 400 with the offending field named, rather than
/// letting MySQL 1048 surface as an opaque 500, is the same choice already recorded in
/// `docs/upstream-parity.md` §3.6.
fn validate_update_payload(payload: &UpdateBundlePayload) -> Result<(), String> {
    // `Option<Option<T>>` -> the inner value, if one was supplied.
    fn value<T>(field: &Option<Option<T>>) -> Option<&T> {
        field.as_ref().and_then(Option::as_ref)
    }
    /// Was the key present AND null?
    fn is_explicit_null<T>(field: &Option<Option<T>>) -> bool {
        matches!(field, Some(None))
    }

    check_text_len("platform", value(&payload.platform))?;
    check_text_len("fileHash", value(&payload.file_hash))?;
    check_text_len("storageUri", value(&payload.storage_uri))?;
    check_text_len("gitCommitHash", value(&payload.git_commit_hash))?;
    check_text_len("message", value(&payload.message))?;
    check_text_len("channel", value(&payload.channel))?;
    check_text_len("targetAppVersion", value(&payload.target_app_version))?;
    check_text_len("fingerprintHash", value(&payload.fingerprint_hash))?;
    check_text_len("manifestStorageUri", value(&payload.manifest_storage_uri))?;
    check_text_len("manifestFileHash", value(&payload.manifest_file_hash))?;
    check_text_len(
        "assetBaseStorageUri",
        value(&payload.asset_base_storage_uri),
    )?;
    check_target_cohorts_len(value(&payload.target_cohorts))?;

    let explicit_nulls = [
        ("platform", is_explicit_null(&payload.platform)),
        (
            "shouldForceUpdate",
            is_explicit_null(&payload.should_force_update),
        ),
        ("enabled", is_explicit_null(&payload.enabled)),
        ("fileHash", is_explicit_null(&payload.file_hash)),
        ("channel", is_explicit_null(&payload.channel)),
        ("storageUri", is_explicit_null(&payload.storage_uri)),
    ];
    for (field, is_null) in explicit_nulls {
        debug_assert!(NOT_NULL_PATCH_COLUMNS.contains(&field));
        if is_null {
            return Err(format!("{field} must not be null."));
        }
    }

    Ok(())
}

pub async fn update_bundle(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, bundle_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    // The body arrives as a raw `Value` rather than a typed extractor so that
    // `require_bundle_patch_payload` can reproduce upstream's contract exactly — an array
    // body collapsing to its first element, and a 400 with upstream's own message text for
    // the two rejection branches. A `Json<UpdateBundlePayload>` extractor answered axum's
    // generic 422 for both.
    let payload_value = match require_bundle_patch_payload(body, &bundle_id) {
        Ok(value) => value,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    let payload: UpdateBundlePayload = match serde_json::from_value(payload_value) {
        Ok(p) => p,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid bundle payload"),
    };

    if let Err(message) = validate_update_payload(&payload) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    let current =
        match sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE app_name = ? AND id = ?")
            .bind(&app_name)
            .bind(&bundle_id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => return error_response(StatusCode::NOT_FOUND, "Bundle not found"),
            Err(err) => {
                error!("Failed to fetch bundle {} for update: {}", bundle_id, err);
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
            }
        };

    // `metadata` is DEEP merged against the stored value, never replaced -- upstream's
    // `mergeBundleUpdate` is an es-toolkit `mergeWith` and `metadata` is not one of its two
    // `REPLACE_ON_UPDATE_KEYS`. Resolved here, where the current row is in hand, so that
    // `build_update_query` stays a pure function of its payload. An explicit null resets the
    // column to `{}` (`bundleToRow`'s `?? {}`), which is why it is not merged at all.
    let mut payload = payload;
    if let Some(Some(patch_metadata)) = &payload.metadata {
        let current_metadata = parse_bundle_metadata(current.metadata.as_ref());
        payload.metadata = Some(Some(merge_bundle_metadata(
            current_metadata.as_ref(),
            patch_metadata,
        )));
    }
    let payload = payload;

    // Validate constraints against the merged row that results after the patch is applied
    // (reference: HotUpdaterApi.updateBundleById -> assertBundlePersistenceConstraints({ ...current, ...newBundle })).
    // With the double option an explicit null is now distinguishable from an absent key, so
    // the merge below is the real post-patch row: `Some(None)` clears, `None` keeps.
    let merged = |patch: &Option<Option<String>>, stored: &Option<String>| -> Option<String> {
        match patch {
            Some(value) => value.clone(),
            None => stored.clone(),
        }
    };
    let merged_target_app_version =
        merged(&payload.target_app_version, &current.target_app_version);
    let merged_fingerprint_hash = merged(&payload.fingerprint_hash, &current.fingerprint_hash);
    let merged_rollout_cohort_count = match payload.rollout_cohort_count {
        // An explicit null is a reset to the default, not a clear.
        Some(value) => value.unwrap_or(cohort::DEFAULT_ROLLOUT_COHORT_COUNT),
        None => current.rollout_cohort_count,
    };
    let merged_target_cohorts = match &payload.target_cohorts {
        Some(value) => value.clone(),
        None => parse_target_cohorts(&current.target_cohorts),
    };

    if let Err(message) = assert_bundle_persistence_constraints(
        &merged_target_app_version,
        &merged_fingerprint_hash,
        merged_rollout_cohort_count,
        &merged_target_cohorts,
    ) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    // `patches` is the other `REPLACE_ON_UPDATE_KEYS` member: present means REPLACE the whole
    // set (empty or null clears it), absent means leave it alone. It writes rows rather than a
    // column, so it cannot ride along in the UPDATE and the two have to share a transaction --
    // otherwise a failed patch insert would leave the column update committed.
    let replacement_patches = payload
        .patches
        .as_ref()
        .map(|value| get_bundle_patches(value.as_ref()));

    if let Some(patches) = &replacement_patches {
        for p in patches {
            if let Err(message) = validate_id("patches[].baseBundleId", &p.base_bundle_id)
                .and_then(|()| check_text_len("patches[].baseFileHash", Some(&p.base_file_hash)))
                .and_then(|()| check_text_len("patches[].patchFileHash", Some(&p.patch_file_hash)))
                .and_then(|()| {
                    check_text_len("patches[].patchStorageUri", Some(&p.patch_storage_uri))
                })
            {
                return error_response(StatusCode::BAD_REQUEST, message);
            }
        }
    }

    let column_update = build_update_query(payload, app_name.clone(), bundle_id.clone());

    // Nothing to SET and no patch list supplied. `UPDATE bundles SET WHERE ...` is not valid
    // SQL, so an empty patch must not reach the database; report success without touching
    // the row.
    if column_update.is_none() && replacement_patches.is_none() {
        return Json(serde_json::json!({ "success": true })).into_response();
    }

    let mut tx = match state.db.begin().await {
        Ok(t) => t,
        Err(err) => {
            error!("Failed to begin update transaction: {}", err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    if let Some(mut qb) = column_update {
        if let Err(err) = qb.build().execute(&mut *tx).await {
            error!("Failed to update bundle {}: {}", bundle_id, err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    }

    if let Some(patches) = replacement_patches {
        // Same delete-then-insert shape as `create_bundles`, and `app_name` is just as
        // essential here: without it this would delete the patch rows of every app that
        // happens to use the same bundle id.
        if let Err(err) =
            sqlx::query("DELETE FROM bundle_patches WHERE app_name = ? AND bundle_id = ?")
                .bind(&app_name)
                .bind(&bundle_id)
                .execute(&mut *tx)
                .await
        {
            error!("Failed to clear patches for bundle {}: {}", bundle_id, err);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database write error");
        }

        for (index, p) in patches.into_iter().enumerate() {
            match app_owns_bundle(&mut tx, &app_name, &p.base_bundle_id).await {
                Ok(true) => {}
                Ok(false) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "Patch baseBundleId does not refer to a bundle of this application",
                    );
                }
                Err(err) => {
                    error!(
                        "Failed to resolve base bundle {}: {}",
                        p.base_bundle_id, err
                    );
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
                }
            }

            let insert_patch_result = sqlx::query(
                r#"
                INSERT INTO bundle_patches (
                    id, app_name, bundle_id, base_bundle_id, base_file_hash, patch_file_hash, patch_storage_uri, order_index
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(format!("{}:{}", bundle_id, p.base_bundle_id))
            .bind(&app_name)
            .bind(&bundle_id)
            .bind(&p.base_bundle_id)
            .bind(&p.base_file_hash)
            .bind(&p.patch_file_hash)
            .bind(&p.patch_storage_uri)
            .bind(index as i32)
            .execute(&mut *tx)
            .await;

            if let Err(err) = insert_patch_result {
                error!("Failed to insert patch for bundle {}: {}", bundle_id, err);
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database write error");
            }
        }
    }

    if let Err(err) = tx.commit().await {
        error!("Failed to commit bundle update transaction: {}", err);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database commit error");
    }

    Json(serde_json::json!({ "success": true })).into_response()
}

/// Build the `UPDATE bundles SET ... WHERE app_name = ? AND id = ?` statement for the
/// submitted fields, or `None` when the patch is empty.
///
/// Split out of `update_bundle` so the generated SQL can be asserted on without a
/// database. Every column name is a literal in this function — no caller-supplied string
/// is ever pushed as SQL text, only as a bind parameter.
fn build_update_query(
    payload: UpdateBundlePayload,
    app_name: String,
    bundle_id: String,
) -> Option<sqlx::QueryBuilder<'static, sqlx::MySql>> {
    let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("UPDATE bundles SET ");
    let mut has_update = false;

    macro_rules! set_field {
        ($col:expr, $val:expr) => {{
            if has_update {
                qb.push(", ");
            }
            qb.push($col);
            qb.push(" = ");
            qb.push_bind($val);
            has_update = true;
        }};
    }

    // The six NOT NULL columns. `validate_update_payload` has already rejected an explicit
    // null for each, so `Some(None)` cannot reach here; unwrapping the inner option is safe
    // and keeps the bind types non-nullable.
    if let Some(Some(v)) = payload.platform {
        set_field!("platform", v);
    }
    if let Some(Some(v)) = payload.should_force_update {
        set_field!("should_force_update", if v { 1i8 } else { 0i8 });
    }
    if let Some(Some(v)) = payload.enabled {
        set_field!("enabled", if v { 1i8 } else { 0i8 });
    }
    if let Some(Some(v)) = payload.file_hash {
        set_field!("file_hash", v);
    }
    if let Some(Some(v)) = payload.channel {
        set_field!("channel", v);
    }
    if let Some(Some(v)) = payload.storage_uri {
        set_field!("storage_uri", v);
    }

    // The nullable columns. `Some(None)` binds SQL NULL and CLEARS the column — that is the
    // whole point of the double option, and the only way to undo a `message` or a
    // `fingerprintHash` through this API.
    if let Some(v) = payload.git_commit_hash {
        set_field!("git_commit_hash", v);
    }
    if let Some(v) = payload.message {
        set_field!("message", v);
    }
    if let Some(v) = payload.target_app_version {
        set_field!("target_app_version", v);
    }
    if let Some(v) = payload.fingerprint_hash {
        set_field!("fingerprint_hash", v);
    }
    if let Some(v) = payload.manifest_storage_uri {
        set_field!("manifest_storage_uri", v);
    }
    if let Some(v) = payload.manifest_file_hash {
        set_field!("manifest_file_hash", v);
    }
    if let Some(v) = payload.asset_base_storage_uri {
        set_field!("asset_base_storage_uri", v);
    }
    // Nullable, and REPLACED rather than merged — `targetCohorts` is one of upstream's two
    // `REPLACE_ON_UPDATE_KEYS`. Contrast `metadata` directly below, which arrives here already
    // deep merged by `update_bundle`.
    if let Some(v) = payload.target_cohorts {
        set_field!(
            "target_cohorts",
            v.as_deref()
                .map(cohorts_to_json)
                .unwrap_or(serde_json::Value::Null)
        );
    }

    // NOT NULL with a default, and an explicit null means "reset to that default" rather than
    // "clear": `bundleToRow` writes `?? DEFAULT_ROLLOUT_COHORT_COUNT` and `?? {}` respectively.
    // `metadata` has already been merged against the stored value by `update_bundle`.
    if let Some(v) = payload.rollout_cohort_count {
        set_field!(
            "rollout_cohort_count",
            v.unwrap_or(cohort::DEFAULT_ROLLOUT_COHORT_COUNT)
        );
    }
    if let Some(v) = payload.metadata {
        set_field!("metadata", v.unwrap_or_else(|| serde_json::json!({})));
    }

    if !has_update {
        return None;
    }

    // CROSS-APP ISOLATION: both predicates are mandatory. `app_name` alone would let a
    // token update every bundle of its app; `id` alone would let it update another
    // tenant's bundle. The SELECT in `update_bundle` checks the same pair, but this WHERE
    // is what actually enforces it at write time.
    qb.push(" WHERE app_name = ");
    qb.push_bind(app_name);
    qb.push(" AND id = ");
    qb.push_bind(bundle_id);

    Some(qb)
}

// 6. DELETE /:app/hot-updater/api/bundles/:id
pub async fn delete_bundle(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, bundle_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return error_response(err.0, err.1);
    }

    // Since DB has ON DELETE CASCADE on foreign keys, deleting from bundles automatically deletes patches!
    let delete_result = sqlx::query("DELETE FROM bundles WHERE app_name = ? AND id = ?")
        .bind(&app_name)
        .bind(&bundle_id)
        .execute(&state.db)
        .await;

    match delete_result {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(err) => {
            error!("Failed to delete bundle {}: {}", bundle_id, err);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Database error")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- constant-time token comparison -------------------------------------------------

    #[test]
    fn token_eq_accepts_the_exact_token() {
        assert!(constant_time_token_eq(b"s3cr3t-token", b"s3cr3t-token"));
    }

    #[test]
    fn token_eq_rejects_a_wrong_byte() {
        assert!(!constant_time_token_eq(b"s3cr3t-tokeN", b"s3cr3t-token"));
        assert!(!constant_time_token_eq(b"S3cr3t-token", b"s3cr3t-token"));
    }

    #[test]
    fn token_eq_rejects_prefixes_and_extensions() {
        // The modulo-indexing trick must not make a repeated or truncated token match.
        assert!(!constant_time_token_eq(b"s3cr3t", b"s3cr3t-token"));
        assert!(!constant_time_token_eq(
            b"s3cr3t-token-extra",
            b"s3cr3t-token"
        ));
        assert!(!constant_time_token_eq(b"abab", b"ab"));
        assert!(!constant_time_token_eq(b"", b"ab"));
    }

    #[test]
    fn token_eq_rejects_everything_when_no_token_is_configured() {
        assert!(!constant_time_token_eq(b"", b""));
        assert!(!constant_time_token_eq(b"anything", b""));
    }

    #[test]
    fn token_eq_handles_non_ascii_and_long_tokens() {
        let expected = "ä".repeat(300);
        let mut wrong = expected.clone().into_bytes();
        let last = wrong.len() - 1;
        wrong[last] ^= 0x01;
        assert!(constant_time_token_eq(
            expected.as_bytes(),
            expected.as_bytes()
        ));
        assert!(!constant_time_token_eq(&wrong, expected.as_bytes()));
    }

    // --- id / length validation ---------------------------------------------------------

    #[test]
    fn validate_id_accepts_a_uuid() {
        assert!(validate_id("id", "0198f0c1-4a2b-7c3d-8e4f-5a6b7c8d9e0f").is_ok());
    }

    #[test]
    fn validate_id_rejects_empty_overlong_and_exotic_ids() {
        assert!(validate_id("id", "").is_err());
        assert!(validate_id("id", &"a".repeat(MAX_ID_LEN + 1)).is_err());
        assert!(validate_id("id", &"a".repeat(MAX_ID_LEN)).is_ok());
        // A truncating MySQL would collapse these two onto one primary key.
        assert!(validate_id("id", "0198f0c1-4a2b-7c3d-8e4f-5a6b7c8d9e0f-evil").is_err());
        assert!(validate_id("id", "id with space").is_err());
        assert!(validate_id("id", "id'; DROP TABLE bundles--").is_err());
        assert!(validate_id("id", "ıd-non-ascii").is_err());
    }

    #[test]
    fn check_text_len_uses_byte_length() {
        assert!(check_text_len("message", None).is_ok());
        assert!(check_text_len("message", Some(&"a".repeat(MAX_TEXT_BYTES))).is_ok());
        assert!(check_text_len("message", Some(&"a".repeat(MAX_TEXT_BYTES + 1))).is_err());
        // 2 bytes per char: half the char count is already over the byte limit.
        assert!(check_text_len("message", Some(&"ä".repeat(MAX_TEXT_BYTES / 2 + 1))).is_err());
    }

    #[test]
    fn check_target_cohorts_len_bounds_the_array() {
        let ok: Vec<String> = (0..MAX_TARGET_COHORTS).map(|i| i.to_string()).collect();
        let too_many: Vec<String> = (0..MAX_TARGET_COHORTS + 1).map(|i| i.to_string()).collect();
        assert!(check_target_cohorts_len(None).is_ok());
        assert!(check_target_cohorts_len(Some(&ok)).is_ok());
        assert!(check_target_cohorts_len(Some(&too_many)).is_err());
    }

    #[test]
    fn cohorts_to_json_round_trips() {
        let cohorts = vec!["1".to_string(), "beta-testers".to_string()];
        assert_eq!(
            cohorts_to_json(&cohorts),
            serde_json::json!(["1", "beta-testers"])
        );
        assert_eq!(cohorts_to_json(&[]), serde_json::json!([]));
    }

    // --- POST /bundles payload validation ------------------------------------------------

    fn sample_bundle() -> CLIBundle {
        CLIBundle {
            id: "0198f0c1-4a2b-7c3d-8e4f-5a6b7c8d9e0f".to_string(),
            platform: "ios".to_string(),
            should_force_update: None,
            enabled: None,
            file_hash: "abc".to_string(),
            git_commit_hash: None,
            message: None,
            channel: None,
            storage_uri: "s3://bucket/key".to_string(),
            target_app_version: Some("1.0.0".to_string()),
            fingerprint_hash: None,
            metadata: None,
            rollout_cohort_count: None,
            target_cohorts: None,
            manifest_storage_uri: None,
            manifest_file_hash: None,
            asset_base_storage_uri: None,
            patches: None,
        }
    }

    fn sample_patch(base: &str) -> serde_json::Value {
        serde_json::json!({
            "baseBundleId": base,
            "baseFileHash": "base-hash",
            "patchFileHash": "patch-hash",
            "patchStorageUri": "s3://bucket/patch",
        })
    }

    fn patch_bases(cb: &CLIBundle) -> Vec<String> {
        get_bundle_patches(cb.patches.as_ref())
            .into_iter()
            .map(|p| p.base_bundle_id)
            .collect()
    }

    #[test]
    fn validate_cli_bundle_accepts_a_normal_payload() {
        assert!(validate_cli_bundle(&sample_bundle()).is_ok());
    }

    #[test]
    fn validate_cli_bundle_rejects_a_bad_id() {
        let mut cb = sample_bundle();
        cb.id = String::new();
        assert!(validate_cli_bundle(&cb).is_err());
    }

    #[test]
    fn validate_cli_bundle_rejects_oversized_text() {
        let mut cb = sample_bundle();
        cb.message = Some("a".repeat(MAX_TEXT_BYTES + 1));
        assert!(validate_cli_bundle(&cb).is_err());
    }

    /// A repeated `baseBundleId` is DROPPED, not rejected.
    ///
    /// This used to answer 400 `Duplicate patch baseBundleId "…"`. Upstream's
    /// `getBundlePatches` keeps the first occurrence and silently discards the rest, and a
    /// payload upstream publishes must not fail here — the reason the check existed (two
    /// patches colliding on the derived primary key `"{bundle_id}:{base_bundle_id}"`) is
    /// satisfied just as well by dropping the repeat, and without the 400.
    #[test]
    fn duplicate_patch_bases_are_dropped_not_rejected() {
        let mut cb = sample_bundle();
        cb.patches = Some(serde_json::json!([
            sample_patch("0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f"),
            sample_patch("0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f"),
        ]));
        assert!(validate_cli_bundle(&cb).is_ok());
        assert_eq!(
            patch_bases(&cb),
            vec!["0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f".to_string()],
            "the repeated base must collapse to one patch row, not two"
        );

        cb.patches = Some(serde_json::json!([
            sample_patch("0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f"),
            sample_patch("0198f0c1-1111-7c3d-8e4f-5a6b7c8d9e0f"),
        ]));
        assert!(validate_cli_bundle(&cb).is_ok());
        assert_eq!(patch_bases(&cb).len(), 2);

        // Under the `ascii_general_ci` collation on the id columns these two ARE the same
        // primary key, so the repeat has to collapse here as well or MySQL answers 1062.
        // Upstream, comparing case-sensitively, would keep both — see `get_bundle_patches`.
        cb.patches = Some(serde_json::json!([
            sample_patch("0198f0c1-000a-7c3d-8e4f-5a6b7c8d9e0f"),
            sample_patch("0198f0c1-000A-7c3d-8e4f-5a6b7c8d9e0f"),
        ]));
        assert!(validate_cli_bundle(&cb).is_ok());
        assert_eq!(
            patch_bases(&cb),
            vec!["0198f0c1-000a-7c3d-8e4f-5a6b7c8d9e0f".to_string()],
        );
    }

    /// A patch entry that is not four strings is DROPPED, matching upstream's
    /// `isBundlePatchArtifact` filter — the whole request must not fail over it.
    #[test]
    fn malformed_patch_entries_are_dropped_not_rejected() {
        let mut cb = sample_bundle();
        cb.patches = Some(serde_json::json!([
            { "baseBundleId": "0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f", "baseFileHash": "h", "patchFileHash": "p" },
            { "baseBundleId": "0198f0c1-1111-7c3d-8e4f-5a6b7c8d9e0f", "baseFileHash": null, "patchFileHash": "p", "patchStorageUri": "s3://b/p" },
            sample_patch("0198f0c1-2222-7c3d-8e4f-5a6b7c8d9e0f"),
        ]));
        assert!(validate_cli_bundle(&cb).is_ok());
        assert_eq!(
            patch_bases(&cb),
            vec!["0198f0c1-2222-7c3d-8e4f-5a6b7c8d9e0f".to_string()],
            "only the well-formed entry survives"
        );

        // A non-array `patches` is no patches at all, not a rejected request.
        cb.patches = Some(serde_json::json!({ "baseBundleId": "x" }));
        assert!(validate_cli_bundle(&cb).is_ok());
        assert!(patch_bases(&cb).is_empty());

        cb.patches = Some(serde_json::Value::Null);
        assert!(validate_cli_bundle(&cb).is_ok());
        assert!(patch_bases(&cb).is_empty());
    }

    /// A surviving patch still has to fit the `CHAR(36)` column, which upstream's
    /// schema-less path never has to care about.
    #[test]
    fn validate_cli_bundle_rejects_a_bad_patch_base_id() {
        let mut cb = sample_bundle();
        cb.patches = Some(serde_json::json!([sample_patch("not a uuid")]));
        assert!(validate_cli_bundle(&cb).is_err());
    }

    #[test]
    fn parse_bundle_metadata_matches_upstream() {
        use serde_json::json;
        // A real object survives, in both storage forms.
        assert_eq!(
            parse_bundle_metadata(Some(&json!({ "a": 1 }))),
            Some(json!({ "a": 1 }))
        );
        assert_eq!(
            parse_bundle_metadata(Some(&json!("{\"a\":1}"))),
            Some(json!({ "a": 1 }))
        );
        assert_eq!(parse_bundle_metadata(Some(&json!({}))), Some(json!({})));
        // Everything else is ABSENT -- not null, not {}.
        assert_eq!(parse_bundle_metadata(None), None);
        assert_eq!(parse_bundle_metadata(Some(&json!(null))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!(""))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!("not json"))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!("[1,2,3]"))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!("null"))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!([1, 2, 3]))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!(0))), None);
        assert_eq!(parse_bundle_metadata(Some(&json!(false))), None);
    }

    #[test]
    fn require_bundle_patch_payload_matches_upstream() {
        use serde_json::json;
        let id = "bundle-1";

        assert_eq!(
            require_bundle_patch_payload(json!({ "enabled": false }), id),
            Ok(json!({ "enabled": false })),
        );
        // An array body collapses to its FIRST element.
        assert_eq!(
            require_bundle_patch_payload(json!([{ "enabled": false }, { "enabled": true }]), id),
            Ok(json!({ "enabled": false })),
        );
        // A matching id is stripped.
        assert_eq!(
            require_bundle_patch_payload(json!({ "id": id, "enabled": false }), id),
            Ok(json!({ "enabled": false })),
        );
        // `id !== void 0` -- an explicit null counts as PRESENT, so it is a mismatch.
        assert_eq!(
            require_bundle_patch_payload(json!({ "id": null }), id),
            Err("Bundle id mismatch"),
        );
        assert_eq!(
            require_bundle_patch_payload(json!({ "id": "other" }), id),
            Err("Bundle id mismatch"),
        );
        for body in [json!([]), json!(null), json!("nope"), json!(7), json!(true)] {
            assert_eq!(
                require_bundle_patch_payload(body.clone(), id),
                Err("Invalid bundle payload"),
                "body {body} must be rejected"
            );
        }
    }

    // --- list filters ---------------------------------------------------------------------

    fn empty_list_params() -> ListBundlesParams {
        ListBundlesParams {
            channel: None,
            platform: None,
            enabled_raw: None,
            enabled: None,
            limit: None,
            page: None,
            offset: None,
            target_app_version: None,
            target_app_version_in: Vec::new(),
            target_app_version_not_null_raw: None,
            target_app_version_not_null: None,
            fingerprint_hash: None,
            id_eq: None,
            id_gt: None,
            id_gte: None,
            id_lt: None,
            id_lte: None,
            id_in: Vec::new(),
            after: None,
            before: None,
        }
    }

    #[test]
    fn query_get_all_matches_search_params_get_all() {
        let q = Some("idIn=a&idIn=b&channel=production&idIn=c");
        assert_eq!(query_get_all(q, "idIn"), vec!["a", "b", "c"]);
        // Zero occurrences -> empty, which upstream maps to `undefined` (filter omitted).
        assert_eq!(query_get_all(q, "targetAppVersionIn"), Vec::<String>::new());
        assert_eq!(query_get_all(None, "idIn"), Vec::<String>::new());
        // A single occurrence is a one-element list, not a bare scalar.
        assert_eq!(query_get_all(Some("idIn=a"), "idIn"), vec!["a"]);
        // An empty occurrence is a real one-element list containing "".
        assert_eq!(query_get_all(Some("idIn="), "idIn"), vec![""]);
        // A comma is data, not a separator: upstream sees ONE id containing a comma.
        assert_eq!(query_get_all(Some("idIn=a,b"), "idIn"), vec!["a,b"]);
        // Percent-encoding and `+` are decoded the way URLSearchParams does.
        assert_eq!(query_get_all(Some("idIn=a%2Cb"), "idIn"), vec!["a,b"]);
        assert_eq!(query_get_all(Some("idIn=a+b"), "idIn"), vec!["a b"]);
        // A prefix match must not be picked up.
        assert_eq!(query_get_all(Some("idInX=a"), "idIn"), Vec::<String>::new());
    }

    #[test]
    fn is_platform_accepts_only_ios_and_android() {
        assert!(is_platform("ios"));
        assert!(is_platform("android"));
        assert!(!is_platform("windows"));
        assert!(!is_platform("iOS"));
        assert!(!is_platform(""));
    }

    #[test]
    fn repeated_in_filters_produce_one_placeholder_each() {
        let mut params = empty_list_params();
        params.id_in = vec!["a".to_string(), "b,c".to_string()];
        params.target_app_version_in = vec!["1.0.0".to_string()];

        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        // "b,c" is one bound value, not two.
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND target_app_version IN (?) AND id IN (?, ?)"
        );

        // An empty list omits the clause entirely.
        let empty = empty_list_params();
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &empty);
        assert_eq!(qb.sql(), "SELECT 1 WHERE 1=1");
    }

    #[test]
    fn validate_list_params_bounds_in_lists() {
        let mut params = empty_list_params();
        assert!(validate_list_params(&params).is_ok());

        params.id_in = vec!["x".to_string(); MAX_IN_LIST_VALUES];
        assert!(validate_list_params(&params).is_ok());

        params.id_in = vec!["x".to_string(); MAX_IN_LIST_VALUES + 1];
        assert!(validate_list_params(&params).is_err());

        params.id_in = Vec::new();
        params.target_app_version_in = vec!["1.0.0".to_string(); MAX_IN_LIST_VALUES + 1];
        assert!(validate_list_params(&params).is_err());
    }

    // --- upstream pagination semantics -----------------------------------------------------

    #[test]
    fn parse_positive_integer_param_matches_upstream() {
        let p = |raw: Option<&str>| parse_positive_integer_param("limit", raw, 50, 100);

        assert_eq!(p(None), Ok(50));
        assert_eq!(p(Some("1")), Ok(1));
        assert_eq!(p(Some("100")), Ok(100));
        // Upstream *rejects* these rather than clamping them.
        assert!(p(Some("0")).is_err());
        assert!(p(Some("-1")).is_err());
        assert!(p(Some("101")).is_err());
        assert!(p(Some("1.5")).is_err());
        assert!(p(Some("abc")).is_err());
        assert!(p(Some("")).is_err()); // Number("") === 0
        assert!(p(Some("Infinity")).is_err());
        assert!(p(Some("NaN")).is_err());
        // JS `Number` accepts these spellings of whole numbers.
        assert_eq!(p(Some(" 7 ")), Ok(7));
        assert_eq!(p(Some("+7")), Ok(7));
        assert_eq!(p(Some("7.0")), Ok(7));
        assert_eq!(p(Some("1e2")), Ok(100));
        assert_eq!(p(Some("0x10")), Ok(16));

        assert_eq!(
            p(Some("0")).unwrap_err(),
            "The 'limit' query parameter must be a positive integer between 1 and 100."
        );
    }

    #[test]
    fn parse_page_param_has_no_upper_bound_and_its_own_message() {
        assert_eq!(parse_page_param(None), Ok(None));
        assert_eq!(parse_page_param(Some("1")), Ok(Some(1)));
        // No max, unlike `limit`.
        assert_eq!(parse_page_param(Some("1000000")), Ok(Some(1_000_000)));
        assert!(parse_page_param(Some("0")).is_err());
        assert!(parse_page_param(Some("-3")).is_err());
        assert!(parse_page_param(Some("2.5")).is_err());
        assert_eq!(
            parse_page_param(Some("x")).unwrap_err(),
            "The 'page' query parameter must be a positive integer."
        );
        // Beyond i64 it saturates; the offset it yields is clamped to the last page anyway.
        assert_eq!(parse_page_param(Some("1e30")), Ok(Some(i64::MAX)));
    }

    #[test]
    fn calculate_pagination_matches_upstream() {
        assert_eq!(
            calculate_pagination(0, 2, 0),
            PaginationMeta {
                total: 0,
                has_next_page: false,
                has_previous_page: false,
                current_page: 1,
                total_pages: 0
            }
        );
        // First page of 7 rows at limit 2.
        assert_eq!(
            calculate_pagination(7, 2, 0),
            PaginationMeta {
                total: 7,
                has_next_page: true,
                has_previous_page: false,
                current_page: 1,
                total_pages: 4
            }
        );
        // The `before=b4` page starts at index 1: currentPage is 1 *and* hasPreviousPage
        // is true. Incoherent, and exactly what upstream returns.
        assert_eq!(
            calculate_pagination(7, 2, 1),
            PaginationMeta {
                total: 7,
                has_next_page: true,
                has_previous_page: true,
                current_page: 1,
                total_pages: 4
            }
        );
        // Exhausted `after` cursor: startIndex jumps to total.
        assert_eq!(
            calculate_pagination(7, 2, 7),
            PaginationMeta {
                total: 7,
                has_next_page: false,
                has_previous_page: true,
                current_page: 4,
                total_pages: 4
            }
        );
    }

    #[test]
    fn build_cursor_page_query_matches_upstream() {
        // `after` keeps the descending order and filters `id < after`.
        assert_eq!(
            build_cursor_page_query(Some("b6"), None, true),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::Lt("b6".into()),
                ascending: false
            }
        );
        // `before` flips to ascending, filters `id > before` and reverses the rows.
        assert_eq!(
            build_cursor_page_query(None, Some("b4"), true),
            CursorPlan {
                reverse_data: true,
                id_filter: IdFilter::Gt("b4".into()),
                ascending: true
            }
        );
        // No cursor: plain descending first page.
        assert_eq!(
            build_cursor_page_query(None, None, true),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::None,
                ascending: false
            }
        );
        // `after` wins when both are supplied — upstream tests `cursor.after` first.
        assert_eq!(
            build_cursor_page_query(Some("b6"), Some("b4"), true),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::Lt("b6".into()),
                ascending: false
            }
        );
        // JS truthiness: an empty cursor string is falsy, so `?after=` / `?before=` are not
        // cursors at all and must not produce an `id` predicate.
        assert_eq!(
            build_cursor_page_query(Some(""), None, true),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::None,
                ascending: false
            }
        );
        assert_eq!(
            build_cursor_page_query(None, Some(""), true),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::None,
                ascending: false
            }
        );
        // An empty `after` falls through to a non-empty `before`.
        assert_eq!(
            build_cursor_page_query(Some(""), Some("b4"), true),
            CursorPlan {
                reverse_data: true,
                id_filter: IdFilter::Gt("b4".into()),
                ascending: true
            }
        );
        // Ascending base ordering mirrors every predicate.
        assert_eq!(
            build_cursor_page_query(Some("b6"), None, false),
            CursorPlan {
                reverse_data: false,
                id_filter: IdFilter::Gt("b6".into()),
                ascending: true
            }
        );
    }

    /// Replay of the probe harness against the pure functions: 7 rows `b1..b7`, descending.
    /// Returns `(data ids, meta, next_cursor, previous_cursor)` for one request.
    fn replay(after: Option<&str>, before: Option<&str>, limit: i64) -> (Vec<String>, String) {
        let rows: Vec<String> = (1..=7).map(|i| format!("b{i}")).collect();
        let total = rows.len() as i64;

        let plan = build_cursor_page_query(after, before, true);
        let mut page: Vec<String> = rows
            .iter()
            .filter(|id| match &plan.id_filter {
                IdFilter::Gt(c) => id.as_str() > c.as_str(),
                IdFilter::Lt(c) => id.as_str() < c.as_str(),
                IdFilter::None => true,
            })
            .cloned()
            .collect();
        if plan.ascending {
            page.sort();
        } else {
            page.sort_by(|a, b| b.cmp(a));
        }
        page.truncate(limit as usize);
        if plan.reverse_data {
            page.reverse();
        }

        let has_cursor =
            after.is_some_and(|c| !c.is_empty()) || before.is_some_and(|c| !c.is_empty());
        let start_index = match page.first() {
            _ if !has_cursor => 0,
            None => {
                if after.is_some() {
                    total
                } else {
                    0
                }
            }
            Some(first) => rows
                .iter()
                .filter(|id| id.as_str() > first.as_str())
                .count() as i64,
        };
        let meta = calculate_pagination(total, limit, start_index);
        let (echo_after, echo_before) = if has_cursor {
            (after, before)
        } else {
            (None, None)
        };
        let keys = derive_cursor_keys(total, start_index, &page, echo_after, echo_before);
        let json = pagination_json(&meta, &keys);
        (page, serde_json::to_string(&json).unwrap())
    }

    #[test]
    fn before_cursor_returns_the_preceding_page_not_the_first_one() {
        // Recorded from the real upstream probe: `?limit=2&before=b4` over b1..b7.
        let (data, pagination) = replay(None, Some("b4"), 2);
        assert_eq!(data, vec!["b6", "b5"]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7,
                "hasNextPage": true,
                "hasPreviousPage": true,
                "currentPage": 1,
                "totalPages": 4,
                "nextCursor": "b5",
                "previousCursor": "b6"
            })
        );
    }

    #[test]
    fn cursor_pages_match_the_recorded_upstream_probe() {
        // No cursor.
        let (data, pagination) = replay(None, None, 2);
        assert_eq!(data, vec!["b7", "b6"]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": true, "hasPreviousPage": false,
                "currentPage": 1, "totalPages": 4, "nextCursor": "b6"
            })
        );

        // after=b6.
        let (data, pagination) = replay(Some("b6"), None, 2);
        assert_eq!(data, vec!["b5", "b4"]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": true, "hasPreviousPage": true,
                "currentPage": 2, "totalPages": 4,
                "nextCursor": "b4", "previousCursor": "b5"
            })
        );

        // before=b7: already the first page, so the page comes back empty and the cursor
        // is echoed as `nextCursor`.
        let (data, pagination) = replay(None, Some("b7"), 2);
        assert!(data.is_empty());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": true, "hasPreviousPage": false,
                "currentPage": 1, "totalPages": 4, "nextCursor": "b7"
            })
        );

        // after=b1: past the end. startIndex jumps to total and the cursor comes back as
        // `previousCursor`.
        let (data, pagination) = replay(Some("b1"), None, 2);
        assert!(data.is_empty());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": false, "hasPreviousPage": true,
                "currentPage": 4, "totalPages": 4, "previousCursor": "b1"
            })
        );

        // A garbage cursor is not an error: it is just an id filter that matches nothing.
        let (data, pagination) = replay(Some("ZZZZ"), None, 2);
        assert!(data.is_empty());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": false, "hasPreviousPage": true,
                "currentPage": 4, "totalPages": 4, "previousCursor": "ZZZZ"
            })
        );

        // ...and one that sorts below every id walks back to the last page.
        let (data, pagination) = replay(None, Some("!!!!"), 2);
        assert_eq!(data, vec!["b2", "b1"]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pagination).unwrap(),
            serde_json::json!({
                "total": 7, "hasNextPage": false, "hasPreviousPage": true,
                "currentPage": 3, "totalPages": 4, "previousCursor": "b2"
            })
        );
    }

    #[test]
    fn cursor_keys_are_omitted_never_null() {
        let meta = calculate_pagination(7, 2, 0);
        let json = pagination_json(
            &meta,
            &CursorKeys {
                next_cursor: None,
                previous_cursor: None,
            },
        );
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("nextCursor"));
        assert!(!obj.contains_key("previousCursor"));
        assert_eq!(obj.len(), 5);

        let json = pagination_json(
            &meta,
            &CursorKeys {
                next_cursor: Some("b6".into()),
                previous_cursor: None,
            },
        );
        assert_eq!(json["nextCursor"], serde_json::json!("b6"));
        assert!(!json.as_object().unwrap().contains_key("previousCursor"));
    }

    #[test]
    fn derive_cursor_keys_matches_upstream() {
        let ids = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };

        // First page: no previous, next is the last row.
        assert_eq!(
            derive_cursor_keys(7, 0, &ids(&["b7", "b6"]), None, None),
            CursorKeys {
                next_cursor: Some("b6".into()),
                previous_cursor: None
            }
        );
        // Middle page: both.
        assert_eq!(
            derive_cursor_keys(7, 1, &ids(&["b6", "b5"]), None, Some("b4")),
            CursorKeys {
                next_cursor: Some("b5".into()),
                previous_cursor: Some("b6".into())
            }
        );
        // The discriminating case: `after=b2, limit=2` leaves ONE row at startIndex 6, so
        // `startIndex + data.length < total` is `6 + 1 < 7` -> false and nextCursor is
        // suppressed. The rule counts rows actually returned, not `offset + limit`.
        assert_eq!(
            derive_cursor_keys(7, 6, &ids(&["b1"]), Some("b2"), None),
            CursorKeys {
                next_cursor: None,
                previous_cursor: Some("b1".into())
            }
        );
        // Exhausted `after`: the empty page echoes the cursor back as previousCursor.
        assert_eq!(
            derive_cursor_keys(7, 7, &[], Some("b1"), None),
            CursorKeys {
                next_cursor: None,
                previous_cursor: Some("b1".into())
            }
        );
        // Exhausted `before`: echoed as nextCursor.
        assert_eq!(
            derive_cursor_keys(7, 0, &[], None, Some("b7")),
            CursorKeys {
                next_cursor: Some("b7".into()),
                previous_cursor: None
            }
        );
        // Both supplied and the page came back empty: both are echoed.
        assert_eq!(
            derive_cursor_keys(7, 7, &[], Some("b1"), Some("b7")),
            CursorKeys {
                next_cursor: Some("b7".into()),
                previous_cursor: Some("b1".into())
            }
        );
        // Empty page with no cursor at all: nothing to echo.
        assert_eq!(
            derive_cursor_keys(0, 0, &[], None, None),
            CursorKeys {
                next_cursor: None,
                previous_cursor: None
            }
        );
        // Empty-string cursors are falsy and echo nothing.
        assert_eq!(
            derive_cursor_keys(7, 0, &[], Some(""), Some("")),
            CursorKeys {
                next_cursor: None,
                previous_cursor: None
            }
        );
    }

    #[test]
    fn parse_platform_param_matches_upstream() {
        assert_eq!(parse_platform_param(None), Ok(None));
        assert_eq!(parse_platform_param(Some("ios")), Ok(Some("ios".into())));
        assert_eq!(
            parse_platform_param(Some("android")),
            Ok(Some("android".into()))
        );
        // No case folding, no trimming, no list form.
        for bad in ["IOS", "Android", "iOS", "web", "ios,android", " ios "] {
            assert!(parse_platform_param(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse_platform_param(Some("web")).unwrap_err(),
            "Invalid platform: web. Expected 'ios' or 'android'."
        );
        // The empty string is not treated as absent; the odd message is upstream's.
        assert_eq!(
            parse_platform_param(Some("")).unwrap_err(),
            "Invalid platform: . Expected 'ios' or 'android'."
        );
    }

    #[test]
    fn parse_truthy_string_param_matches_upstream() {
        assert_eq!(parse_truthy_string_param(None), None);
        // Only `""` is falsy among strings, so it is the only value that drops the filter.
        assert_eq!(parse_truthy_string_param(Some("")), None);
        // Everything else is kept, including the ones that look falsy.
        assert_eq!(parse_truthy_string_param(Some("0")), Some("0".to_string()));
        assert_eq!(parse_truthy_string_param(Some(" ")), Some(" ".to_string()));
        assert_eq!(
            parse_truthy_string_param(Some("false")),
            Some("false".to_string())
        );
        assert_eq!(
            parse_truthy_string_param(Some("production")),
            Some("production".to_string())
        );
    }

    #[test]
    fn truthy_and_nullable_rules_are_not_the_same_rule() {
        // The same input, two meanings, one handler — the pair a future tidy-up would
        // collapse into one "helpful" implementation.
        assert_eq!(
            parse_nullable_string_param(Some("null")),
            NullableStringParam::Null // targetAppVersion / fingerprintHash -> IS NULL
        );
        assert_eq!(
            parse_truthy_string_param(Some("null")),
            Some("null".to_string()) // channel -> the literal four-character string
        );

        // And the same asymmetry on the empty string, in the other direction.
        assert_eq!(parse_truthy_string_param(Some("")), None);
        assert_eq!(
            parse_nullable_string_param(Some("")),
            NullableStringParam::Value(String::new())
        );

        // Proven end to end on the generated SQL, not just on the helpers.
        let mut params = empty_list_params();
        params.channel = Some("null".to_string());
        params.target_app_version = Some("null".to_string());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND channel = ? AND target_app_version IS NULL"
        );
    }

    #[test]
    fn parse_nullable_string_param_matches_upstream() {
        assert_eq!(
            parse_nullable_string_param(None),
            NullableStringParam::Absent
        );
        // Only the exact lowercase `null` becomes an IS NULL filter.
        assert_eq!(
            parse_nullable_string_param(Some("null")),
            NullableStringParam::Null
        );
        // Case-sensitive: these are ordinary string values.
        for literal in ["NULL", "Null", "nulL", " null "] {
            assert_eq!(
                parse_nullable_string_param(Some(literal)),
                NullableStringParam::Value(literal.to_string()),
                "{literal}"
            );
        }
        // The empty string is a value, not an absent parameter.
        assert_eq!(
            parse_nullable_string_param(Some("")),
            NullableStringParam::Value(String::new())
        );
        assert_eq!(
            parse_nullable_string_param(Some("1.0.0")),
            NullableStringParam::Value("1.0.0".to_string())
        );
    }

    #[test]
    fn nullable_string_filters_share_one_rule() {
        // The regression this guards: `?targetAppVersion=null` meant IS NULL while
        // `?fingerprintHash=null` bound the literal string 'null'.
        let mut params = empty_list_params();
        params.target_app_version = Some("null".to_string());
        params.fingerprint_hash = Some("null".to_string());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND fingerprint_hash IS NULL AND target_app_version IS NULL"
        );

        // Uppercase is a value on both, not IS NULL.
        let mut params = empty_list_params();
        params.target_app_version = Some("NULL".to_string());
        params.fingerprint_hash = Some("NULL".to_string());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND fingerprint_hash = ? AND target_app_version = ?"
        );
    }

    #[test]
    fn parse_boolean_param_matches_upstream() {
        assert_eq!(parse_boolean_param("enabled", None), Ok(None));
        assert_eq!(parse_boolean_param("enabled", Some("true")), Ok(Some(true)));
        assert_eq!(
            parse_boolean_param("enabled", Some("false")),
            Ok(Some(false))
        );
        // Only the two exact spellings: no 1/0, no case folding, no trimming, and the
        // empty string is a 400 rather than "absent".
        for bad in ["1", "0", "TRUE", "True", "yes", "", " true "] {
            assert!(parse_boolean_param("enabled", Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse_boolean_param("enabled", Some("1")).unwrap_err(),
            "The 'enabled' query parameter must be 'true' or 'false'."
        );
        assert_eq!(
            parse_boolean_param("targetAppVersionNotNull", Some("x")).unwrap_err(),
            "The 'targetAppVersionNotNull' query parameter must be 'true' or 'false'."
        );
    }

    #[test]
    fn parse_string_array_param_matches_get_all() {
        assert_eq!(parse_string_array_param(&[]), None);
        // `?idIn=` is a one-element list holding "", NOT absent.
        assert_eq!(
            parse_string_array_param(&["".to_string()]),
            Some(vec!["".to_string()])
        );
        // Duplicates kept, order preserved, values opaque.
        assert_eq!(
            parse_string_array_param(&["c".into(), "a".into(), "a".into()]),
            Some(vec!["c".into(), "a".into(), "a".into()])
        );
        assert_eq!(
            parse_string_array_param(&[">=1.0.0 <2.0.0".to_string()]),
            Some(vec![">=1.0.0 <2.0.0".to_string()])
        );
    }

    #[test]
    fn empty_string_scalars_follow_js_truthiness() {
        // `channel` and the scalar id filters are folded through truthiness upstream, so an
        // empty value drops the clause entirely...
        let mut params = empty_list_params();
        params.channel = Some(String::new());
        params.id_eq = Some(String::new());
        params.id_gt = Some(String::new());
        params.id_gte = Some(String::new());
        params.id_lt = Some(String::new());
        params.id_lte = Some(String::new());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(qb.sql(), "SELECT 1 WHERE 1=1");

        // ...while `targetAppVersion` and `fingerprintHash` are guarded with `!== undefined`
        // and therefore keep it as a filter on the empty string.
        let mut params = empty_list_params();
        params.target_app_version = Some(String::new());
        params.fingerprint_hash = Some(String::new());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND fingerprint_hash = ? AND target_app_version = ?"
        );

        // `?idIn=` is a filter on the empty string, not an absent filter.
        let mut params = empty_list_params();
        params.id_in = vec![String::new()];
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(qb.sql(), "SELECT 1 WHERE 1=1 AND id IN (?)");
    }

    #[test]
    fn page_offset_is_clamped_to_the_last_page() {
        // Mirror of `createDatabasePlugin.mjs:228-234` for total=7, limit=2 → 4 pages,
        // maxOffset 6.
        let resolve = |page: i64, total: i64, limit: i64| {
            let total_pages = if total == 0 {
                0
            } else {
                total / limit + i64::from(total % limit != 0)
            };
            let max_offset = if total_pages == 0 {
                0
            } else {
                (total_pages.max(1) - 1).saturating_mul(limit)
            };
            page.saturating_sub(1).saturating_mul(limit).min(max_offset)
        };

        assert_eq!(resolve(1, 7, 2), 0);
        assert_eq!(resolve(2, 7, 2), 2);
        assert_eq!(resolve(4, 7, 2), 6);
        assert_eq!(resolve(99, 7, 2), 6); // clamped, not an empty page
        assert_eq!(resolve(i64::MAX, 7, 2), 6); // and it does not overflow
        assert_eq!(resolve(3, 0, 2), 0); // no rows at all
    }

    #[test]
    fn apply_filters_only_pushes_placeholders_never_caller_text() {
        let mut params = empty_list_params();
        params.channel = Some("production'; DROP TABLE bundles--".to_string());
        params.id_in = vec!["a".to_string(), "b".to_string()];
        params.after = Some("' OR 1=1--".to_string());
        params.target_app_version = Some("1.0.0".to_string());

        let mut qb: sqlx::QueryBuilder<sqlx::MySql> =
            sqlx::QueryBuilder::new("SELECT * FROM bundles WHERE app_name = ");
        qb.push_bind("my-app");
        apply_filters(&mut qb, &params);

        let sql = qb.sql();
        assert!(!sql.contains("DROP TABLE"));
        assert!(!sql.contains("OR 1=1"));
        // The cursor is deliberately absent: `apply_filters` feeds the count query too,
        // and upstream never lets a cursor narrow `pagination.total`.
        assert_eq!(
            sql,
            "SELECT * FROM bundles WHERE app_name = ? AND channel = ? \
             AND target_app_version = ? AND id IN (?, ?)"
        );
    }

    #[test]
    fn apply_filters_maps_the_literal_null_target_version() {
        let mut params = empty_list_params();
        params.target_app_version = Some("null".to_string());
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new("SELECT 1 WHERE 1=1");
        apply_filters(&mut qb, &params);
        assert_eq!(
            qb.sql(),
            "SELECT 1 WHERE 1=1 AND target_app_version IS NULL"
        );
    }

    // --- PATCH /bundles/{id} SQL construction ---------------------------------------------

    #[test]
    fn empty_patch_builds_no_statement() {
        // `UPDATE bundles SET  WHERE ...` would be a syntax error, so an empty patch has
        // to be answered without touching the database.
        assert!(build_update_query(
            UpdateBundlePayload::default(),
            "my-app".to_string(),
            "bundle-1".to_string()
        )
        .is_none());
    }

    #[test]
    fn single_field_patch_is_well_formed_and_app_scoped() {
        let payload = UpdateBundlePayload {
            enabled: Some(Some(false)),
            ..Default::default()
        };
        let qb = build_update_query(payload, "my-app".to_string(), "bundle-1".to_string()).unwrap();
        assert_eq!(
            qb.sql(),
            "UPDATE bundles SET enabled = ? WHERE app_name = ? AND id = ?"
        );
    }

    #[test]
    fn multi_field_patch_separates_assignments_with_commas() {
        let payload = UpdateBundlePayload {
            platform: Some(Some("android".to_string())),
            should_force_update: Some(Some(true)),
            message: Some(Some("hello".to_string())),
            target_cohorts: Some(Some(vec!["1".to_string()])),
            ..Default::default()
        };
        let qb = build_update_query(payload, "my-app".to_string(), "bundle-1".to_string()).unwrap();
        assert_eq!(
            qb.sql(),
            "UPDATE bundles SET platform = ?, should_force_update = ?, message = ?, \
             target_cohorts = ? WHERE app_name = ? AND id = ?"
        );
    }

    #[test]
    fn patch_values_are_bound_never_interpolated() {
        // A caller-controlled value must reach the statement as a placeholder, whatever
        // it contains.
        let payload = UpdateBundlePayload {
            channel: Some(Some("production'; DROP TABLE bundles--".to_string())),
            ..Default::default()
        };
        let qb = build_update_query(payload, "my-app".to_string(), "b'--".to_string()).unwrap();
        assert_eq!(
            qb.sql(),
            "UPDATE bundles SET channel = ? WHERE app_name = ? AND id = ?"
        );
    }

    #[test]
    fn every_patchable_column_produces_a_placeholder() {
        // If a future field is added to `UpdateBundlePayload` and wired up with a
        // caller-supplied column name, this assertion is what catches it.
        let payload = UpdateBundlePayload {
            platform: Some(Some("ios".to_string())),
            should_force_update: Some(Some(true)),
            enabled: Some(Some(true)),
            file_hash: Some(Some("h".to_string())),
            git_commit_hash: Some(Some("g".to_string())),
            message: Some(Some("m".to_string())),
            channel: Some(Some("production".to_string())),
            storage_uri: Some(Some("s3://b/k".to_string())),
            target_app_version: Some(Some("1.0.0".to_string())),
            fingerprint_hash: Some(Some("fp".to_string())),
            metadata: Some(Some(serde_json::json!({"a": 1}))),
            rollout_cohort_count: Some(Some(500)),
            target_cohorts: Some(Some(vec!["beta".to_string()])),
            manifest_storage_uri: Some(Some("s3://b/m".to_string())),
            manifest_file_hash: Some(Some("mh".to_string())),
            asset_base_storage_uri: Some(Some("s3://b/a".to_string())),
            // `patches` writes ROWS, not a column, so it never appears in this statement --
            // `update_bundle` handles it separately. Setting it here proves that.
            patches: Some(Some(serde_json::json!([]))),
        };
        let qb = build_update_query(payload, "my-app".to_string(), "bundle-1".to_string()).unwrap();
        let sql = qb.sql();

        // 16 SET assignments + 2 WHERE predicates.
        assert_eq!(sql.matches('?').count(), 18);
        // `id` is never in the SET list: a PATCH must not move a bundle to a new id.
        assert!(!sql.contains("SET id ="));
        assert!(!sql.contains(", id ="));
        assert!(sql.ends_with(" WHERE app_name = ? AND id = ?"));
    }

    /// An explicit null must reach the statement as a SET, not be silently dropped. Before the
    /// double option this produced NO statement at all: `{"message": null}` answered
    /// `200 {"success":true}` and left the row untouched, so a nullable column could never be
    /// cleared through the API.
    #[test]
    fn an_explicit_null_clears_a_nullable_column() {
        let payload = UpdateBundlePayload {
            message: Some(None),
            ..Default::default()
        };
        let qb = build_update_query(payload, "my-app".to_string(), "bundle-1".to_string()).unwrap();
        assert_eq!(
            qb.sql(),
            "UPDATE bundles SET message = ? WHERE app_name = ? AND id = ?"
        );

        // An ABSENT key still produces nothing -- that is the distinction being drawn.
        assert!(build_update_query(
            UpdateBundlePayload::default(),
            "my-app".to_string(),
            "bundle-1".to_string()
        )
        .is_none());
    }

    /// `metadata` and `rolloutCohortCount` are NOT NULL with defaults, so an explicit null
    /// RESETS them rather than clearing them -- `bundleToRow` writes `?? {}` and
    /// `?? DEFAULT_ROLLOUT_COHORT_COUNT`.
    #[test]
    fn an_explicit_null_resets_the_defaulted_columns() {
        let payload = UpdateBundlePayload {
            metadata: Some(None),
            rollout_cohort_count: Some(None),
            ..Default::default()
        };
        let qb = build_update_query(payload, "my-app".to_string(), "bundle-1".to_string()).unwrap();
        assert_eq!(
            qb.sql(),
            "UPDATE bundles SET rollout_cohort_count = ?, metadata = ? WHERE app_name = ? AND id = ?"
        );
    }

    #[test]
    fn an_explicit_null_on_a_not_null_column_is_rejected() {
        for (field, payload) in [
            (
                "platform",
                UpdateBundlePayload {
                    platform: Some(None),
                    ..Default::default()
                },
            ),
            (
                "enabled",
                UpdateBundlePayload {
                    enabled: Some(None),
                    ..Default::default()
                },
            ),
            (
                "channel",
                UpdateBundlePayload {
                    channel: Some(None),
                    ..Default::default()
                },
            ),
            (
                "storageUri",
                UpdateBundlePayload {
                    storage_uri: Some(None),
                    ..Default::default()
                },
            ),
            (
                "fileHash",
                UpdateBundlePayload {
                    file_hash: Some(None),
                    ..Default::default()
                },
            ),
            (
                "shouldForceUpdate",
                UpdateBundlePayload {
                    should_force_update: Some(None),
                    ..Default::default()
                },
            ),
        ] {
            let err = validate_update_payload(&payload)
                .expect_err("a null on a NOT NULL column must be rejected");
            assert!(err.contains(field), "message {err:?} should name {field}");
        }
    }

    /// The metadata merge, replayed from the recorded upstream cases. See
    /// [`merge_bundle_metadata`] for the table these come from.
    #[test]
    fn merge_bundle_metadata_matches_es_toolkit() {
        use serde_json::json;
        let merge = |current: serde_json::Value, patch: serde_json::Value| {
            merge_bundle_metadata(Some(&current), &patch)
        };

        assert_eq!(
            merge(json!({"b":2,"nested":{"x":1}}), json!({"a":1})),
            json!({"b":2,"nested":{"x":1},"a":1})
        );
        assert_eq!(
            merge(json!({"nested":{"x":1}}), json!({"nested":{"y":2}})),
            json!({"nested":{"x":1,"y":2}})
        );
        assert_eq!(merge(json!({"b":2}), json!({})), json!({"b":2}));
        assert_eq!(merge(json!({"a":1}), json!({"a":2})), json!({"a":2}));
        assert_eq!(
            merge(json!({"a":{"deep":1}}), json!({"a":"flat"})),
            json!({"a":"flat"})
        );
        assert_eq!(
            merge(json!({"a":"flat"}), json!({"a":{"deep":1}})),
            json!({"a":{"deep":1}})
        );
        // Index by index, keeping the stored tail. NOT a replace -- see the doc comment.
        assert_eq!(
            merge(json!({"a":[1,2,3]}), json!({"a":[9]})),
            json!({"a":[9,2,3]})
        );
        assert_eq!(
            merge(json!({"a":[9]}), json!({"a":[1,2,3]})),
            json!({"a":[1,2,3]})
        );
        assert_eq!(merge(json!({"a":1}), json!({"a":null})), json!({"a":null}));
        // No stored metadata at all: the patch is the result.
        assert_eq!(merge_bundle_metadata(None, &json!({"a":1})), json!({"a":1}));
    }

    /// ONE PATCH BODY, TWO MERGE SEMANTICS -- the invariant a single-key test cannot catch.
    ///
    /// `metadata` deep merges; `targetCohorts` and `patches` are REPLACED, because those two
    /// are upstream's `REPLACE_ON_UPDATE_KEYS`. Anyone unifying the three into one "helpful"
    /// rule breaks exactly one of them, and only a test that exercises them in the SAME
    /// request notices. This is the sibling of `truthy_and_nullable_rules_are_not_the_same_rule`
    /// in the query-parameter layer.
    #[test]
    fn one_patch_body_has_two_merge_semantics() {
        use serde_json::json;

        let stored_metadata = json!({"kept": 1});
        let stored_cohorts = ["alpha".to_string(), "beta".to_string()];

        let patch_metadata = json!({"added": true});
        let patch_cohorts = vec!["gamma".to_string()];

        // metadata MERGES: the stored key survives alongside the new one.
        let merged_metadata = merge_bundle_metadata(Some(&stored_metadata), &patch_metadata);
        assert_eq!(merged_metadata, json!({"kept": 1, "added": true}));
        assert!(
            merged_metadata.get("kept").is_some(),
            "metadata must MERGE -- if this fails someone turned it into a replace"
        );

        // targetCohorts REPLACES: the stored list is gone entirely.
        let payload = UpdateBundlePayload {
            target_cohorts: Some(Some(patch_cohorts.clone())),
            ..Default::default()
        };
        let replaced = payload
            .target_cohorts
            .as_ref()
            .and_then(Option::as_ref)
            .expect("supplied");
        assert_eq!(replaced, &patch_cohorts);
        assert!(
            !replaced.iter().any(|c| stored_cohorts.contains(c)),
            "targetCohorts must REPLACE -- if this fails someone turned it into a merge"
        );

        // patches REPLACES too, and an empty list clears rather than preserves.
        assert!(get_bundle_patches(Some(&json!([]))).is_empty());
        assert_eq!(
            get_bundle_patches(Some(&json!([sample_patch(
                "0198f0c1-0000-7c3d-8e4f-5a6b7c8d9e0f"
            )])))
            .len(),
            1
        );
    }

    #[test]
    fn validate_update_payload_rejects_oversized_text() {
        let payload = UpdateBundlePayload {
            storage_uri: Some(Some("a".repeat(MAX_TEXT_BYTES + 1))),
            ..Default::default()
        };
        assert!(validate_update_payload(&payload).is_err());
        assert!(validate_update_payload(&UpdateBundlePayload::default()).is_ok());
    }
}
