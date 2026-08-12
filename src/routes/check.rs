use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::cohort::is_cohort_eligible_for_update;
use crate::config::AppStorageConfig;
use crate::models::{Bundle, BundlePatch};
use crate::observability::{
    record_update_check, record_update_check_error, DegradedReason, ErrorOutcome, UpdateOutcome,
};
use crate::semver::{coerce_version, satisfies};
use crate::storage::get_presigned_url;
use crate::AppState;

const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    pub id: String,
    pub status: String, // "UPDATE" | "ROLLBACK" | "UP_TO_DATE"
    pub should_force_update: bool,
    // `fileHash`, `message` and `fileUrl` are always present, null included: they come from
    // `makeResponse`/`INIT_BUNDLE_ROLLBACK_UPDATE_INFO`, which spell them out. A null `fileUrl`
    // in particular is a protocol *signal* (reset to the built-in bundle) and must not vanish —
    // see `docs/upstream-parity.md` §3.4.
    pub file_hash: Option<String>,
    pub message: Option<String>,
    pub file_url: Option<String>,
    // The three manifest artifacts are spread in with `...manifestArtifacts` only when
    // `resolveManifestArtifacts` returned something, so upstream OMITS the keys entirely when
    // it has no artifacts. Serialising them as explicit nulls was a shape deviation on every
    // degraded response (fixture cases E01-E12c, G05).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_file_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_assets: Option<std::collections::HashMap<String, ChangedAsset>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedAsset {
    // Reference: resolveChangedAssets -- `const changedAsset = { fileHash }; if (fileUrl)
    // changedAsset.file = ...`. The key is OMITTED when there is no url, not set to null; a
    // patch-only asset carries `patch` and nothing else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<ChangedAssetFile>,
    pub file_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<ChangedAssetPatch>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedAssetFile {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChangedAssetPatch {
    pub algorithm: String, // always "bsdiff"
    pub base_bundle_id: String,
    pub base_file_hash: String,
    pub patch_file_hash: String,
    pub patch_url: String,
}

// NOTE: manifest.json uses camelCase ("bundleId") in the real file -- without rename_all
// this field never matches and EVERY manifest parse silently failed.
//
// Reference: updateArtifacts.ts `isBundleManifest`. That predicate is STRICTER than "the
// fields we happen to read parse", and the difference is observable: a manifest it rejects
// yields NO artifacts at all, so the device does a full download rather than a diff. Each
// arm of it is reproduced below; `tests/fixtures/artifacts_fixtures.json` cases E07-E12c pin
// them.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// `typeof manifest.bundleId !== "string"` rejects the manifest. It is never compared with
    /// the bundle that carries it (fixture case E14) -- only its type is checked.
    #[allow(dead_code)]
    bundle_id: String,
    #[serde(deserialize_with = "deserialize_manifest_assets")]
    assets: std::collections::HashMap<String, ManifestAsset>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAsset {
    file_hash: String,
    /// Upstream requires `signature === undefined || typeof signature === "string"`. A
    /// present-but-non-string signature -- including `null`, which is not `undefined` --
    /// invalidates the WHOLE manifest, not just its asset (cases E12, E12b).
    ///
    /// `#[serde(default)]` covers "absent"; the custom deserializer runs only when the key is
    /// present and rejects anything that is not a string, `null` included.
    #[serde(default, deserialize_with = "deserialize_present_string")]
    #[allow(dead_code)]
    signature: Option<String>,
}

fn deserialize_present_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

/// `isBundleManifest` only rejects an ARRAY at the top level and per asset -- it never checks
/// `assets` itself, and an array passes its `typeof === "object"` test. So `assets: []` is a
/// valid empty manifest upstream, and `assets: [{...}]` is a valid one whose asset path is the
/// array index (cases E19, E20). A plain `HashMap` deserialiser rejects both.
fn deserialize_manifest_assets<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, ManifestAsset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Assets {
        Map(std::collections::HashMap<String, ManifestAsset>),
        Seq(Vec<ManifestAsset>),
    }

    Ok(match Assets::deserialize(deserializer)? {
        Assets::Map(map) => map,
        Assets::Seq(seq) => seq
            .into_iter()
            .enumerate()
            .map(|(index, asset)| (index.to_string(), asset))
            .collect(),
    })
}

/// Reference: `BR_COMPRESSED_ASSET_PATH_RE = /(^|\/)index\.[^/]+\.bundle$/`.
///
/// Three things that regex does NOT do, each of which this used to get wrong or could:
///   * it never normalises backslashes, so `build\index.a.bundle` does **not** match (an
///     earlier version replaced `\` with `/` first and wrongly reported brotli here, sending
///     the device to a `.br` object that was never uploaded -- fixture case C09);
///   * `[^/]+` needs at least one character, so `index.bundle` and `index..bundle` do not
///     match (C03, C04) while `index.a.b.c.bundle` does (C05);
///   * it is case-sensitive, so `Index.a.bundle` does not match (C11).
fn uses_brotli_asset(path: &str) -> bool {
    // `(^|/)` -- the match must start at the beginning or just after a forward slash, which is
    // the same as saying it matches the last `/`-delimited component.
    let component = path.rsplit('/').next().unwrap_or(path);
    let Some(rest) = component.strip_prefix("index.") else {
        return false;
    };
    let Some(middle) = rest.strip_suffix(".bundle") else {
        return false;
    };
    !middle.is_empty()
}

/// JavaScript's `encodeURIComponent`: every byte outside the unreserved set
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is percent-encoded.
///
/// This is not the same set the `url` crate applies when a path is assigned, which is why
/// building the path by hand is necessary: `url` leaves `+`, `&` and `=` alone, so an asset
/// called `a+b.png` used to resolve to a key one character different from the one the CLI
/// uploaded (fixture cases B23, B24). Space, `#`, `?` and non-ASCII happened to agree.
fn encode_uri_component(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Reference: plugin-core `createStorageUriWithRelativePath` --
/// `relativePath.replace(/\\/g, "/").split("/").filter(Boolean).map(encodeURIComponent).join("/")`
///
/// `filter(Boolean)` is load-bearing: it drops EMPTY segments, so `assets//logo.png` and
/// `/assets/logo.png` both collapse to `assets/logo.png` (cases B27, B30). Assigning the raw
/// string to `Url::set_path` kept the empty segment and addressed a key that does not exist.
///
/// Dot-segment removal (`./x` -> `x`) is left to `set_path`, which applies the same WHATWG
/// path parser `new URL()` does -- and the encoding above is preserved verbatim by it, since
/// `%` is not in the `url` crate's path-encode set.
fn join_storage_uri(base_storage_uri: &url::Url, base_path: &str, relative_path: &str) -> String {
    let encoded: Vec<String> = relative_path
        .replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_uri_component)
        .collect();

    let mut url = base_storage_uri.clone();
    url.set_path(&format!("{}/{}", base_path, encoded.join("/")));
    url.to_string()
}

/// Reference: plugin-core `getContentAddressedAssetStoragePath` + `getAssetStorageLayout`.
///
/// Returns `None` only when the asset base is not a parseable URI at all -- upstream's
/// `new URL()` throws there. Every artifact is dropped rather than a syntactically invented
/// URI being handed to the presigner.
fn resolve_manifest_asset_storage_uri(
    asset_base_storage_uri: &str,
    asset_path: &str,
    file_hash: &str,
) -> Option<String> {
    let parsed_url = url::Url::parse(asset_base_storage_uri).ok()?;
    let base_path = parsed_url.path().trim_end_matches('/').to_string();
    let is_content_addressed = base_path.ends_with("/assets") || base_path == "/assets";

    if !is_content_addressed {
        // `getLegacyManifestAssetStoragePath` is the identity on the manifest-relative path.
        return Some(join_storage_uri(&parsed_url, &base_path, asset_path));
    }

    let extension = if asset_path.ends_with(".br") {
        ".br".to_string()
    } else {
        match asset_path.rsplit_once('.') {
            Some((_, ext)) => format!(".{ext}"),
            None => String::new(),
        }
    };

    // `fileHash.slice(0, 2)` on a JS string counts UTF-16 units and never panics on a short
    // or non-ASCII value; `&file_hash[0..2]` did, taking the whole request down with it
    // whenever a manifest declared an asset whose hash was shorter than two bytes or whose
    // second byte fell inside a multi-byte character (cases B12, B13, B15). Taking two `char`s
    // agrees with `slice(0, 2)` for every character in the Basic Multilingual Plane, which
    // covers every hash any real toolchain emits.
    let shard: String = file_hash.chars().take(2).collect();

    // An empty shard is not written as an empty segment: `filter(Boolean)` in
    // `createStorageUriWithRelativePath` removes it (case B13).
    Some(join_storage_uri(
        &parsed_url,
        &base_path,
        &format!("sha256/{shard}/{file_hash}{extension}"),
    ))
}

fn supports_explicit_no_update_response(headers: &HeaderMap) -> bool {
    if let Some(sdk_version) = headers
        .get("Hot-Updater-SDK-Version")
        .and_then(|v| v.to_str().ok())
    {
        let version_str = sdk_version.trim();
        if let Some(v) = coerce_version(version_str) {
            return v >= semver::Version::new(0, 31, 0);
        }
    }
    false
}

// Unlike the manifest path, a swallow here CAN change the decision: unreadable `target_cohorts`
// makes the bundle look untargeted, so a device that should have been let in by an explicit
// cohort list falls back to the rollout percentage. That is the right fallback (it fails closed,
// never opening a bundle to a device outside the rollout), but it means silently bad data would
// look exactly like correct data, so a malformed value is logged.
fn parse_target_cohorts(val: &Option<serde_json::Value>) -> Option<Vec<String>> {
    // ONE implementation, shared with the CLI API's response mapping. This used to be a
    // second, stricter copy (`serde_json::from_value::<Vec<String>>`) that answered `None`
    // for a column holding the array as a JSON *string* and for any array with a stray
    // non-string element — both of which upstream reads successfully. On this path that
    // silently un-targets the bundle, so a device inside an explicit cohort list is judged
    // by the rollout percentage instead. See `api::parse_target_cohorts`.
    let parsed = crate::routes::api::parse_target_cohorts(val);

    // Still worth a line when a non-empty column produced nothing: the fallback is safe
    // (it never opens a bundle to a device outside the rollout) but bad data must not look
    // exactly like correct data.
    if parsed.is_none() && !matches!(val, None | Some(serde_json::Value::Null)) {
        warn!("Malformed target_cohorts on a bundle; treating it as untargeted (rollout only)");
    }

    parsed
}

/// Reference: `appVersionStrategy` -- `if (... || !b.targetAppVersion ||
/// !semverSatisfies(b.targetAppVersion, appVersion) ...) continue;`
///
/// `!b.targetAppVersion` is falsy for BOTH `null`/`undefined` and the empty string, so an
/// empty `target_app_version` drops the bundle before any semver work happens. This has to be
/// checked explicitly on the Rust side: npm-compatible range parsing treats an empty range as
/// `*`, so `satisfies(v, "")` matches every version and an untargeted bundle would ship.
pub fn matches_target_app_version(target: Option<&str>, client_version: &semver::Version) -> bool {
    match target {
        Some(t) if !t.is_empty() => satisfies(client_version, t),
        _ => false,
    }
}

/// Reference: `fingerprintStrategy` -- `if (... || !b.fingerprintHash ||
/// b.fingerprintHash !== fingerprintHash ...) continue;`
///
/// The `!b.fingerprintHash` arm means a stored empty-string hash never matches, not even when
/// the request itself carries an empty `fingerprintHash`. A plain `stored == requested` would
/// let those two empties match.
pub fn matches_fingerprint(stored: Option<&str>, requested: &str) -> bool {
    match stored {
        Some(s) if !s.is_empty() => s == requested,
        _ => false,
    }
}

// Reference: updateArtifacts.ts resolveUniqueHbcAssetPath -- returns the asset path ending
// in ".bundle" if the manifest contains exactly ONE (the Hermes bytecode file); None if ambiguous.
fn resolve_unique_hbc_asset_path(manifest: &Manifest) -> Option<String> {
    let mut candidates: Vec<&String> = manifest
        .assets
        .keys()
        .filter(|path| path.ends_with(".bundle"))
        .collect();
    candidates.sort();
    if candidates.len() == 1 {
        Some(candidates[0].clone())
    } else {
        None
    }
}

/// Reference: @hot-updater/core `getBundlePatch` -- `getBundlePatches(bundle).find(patch =>
/// patch.baseBundleId === baseBundleId)`, i.e. the FIRST record naming this base, compared with
/// `===`.
///
/// This used to compare with `eq_ignore_ascii_case`, on the argument that the id columns are
/// `ascii_general_ci` and MySQL would accept a foreign-key reference written in the other case.
/// That argument is real but it does not license the deviation: upstream is case-sensitive, and
/// under `ascii_general_ci` the `bundles` table cannot hold both `…0000aa` and `…0000AA` in the
/// first place, so the only thing case-insensitivity could ever do is match a patch row whose
/// recorded base is spelled differently from the bundle it points at. Fixture case D06 pins the
/// upstream answer (no patch) and D06b the same shape with matching case (patch selected), so
/// the two cannot be collapsed again.
///
/// Note the first-match semantics also reproduce `getBundlePatches`'s de-duplication: it keeps
/// the first record per `baseBundleId`, which is what `find` on the caller's `ORDER BY
/// order_index ASC` list does (case D11).
fn find_bundle_patch<'a>(
    patches: &'a [BundlePatch],
    base_bundle_id: &str,
) -> Option<&'a BundlePatch> {
    patches.iter().find(|p| p.base_bundle_id == base_bundle_id)
}

/// The bsdiff patch a plan selected, still holding the storage URI rather than a download URL —
/// presigning is the caller's job, which is what keeps [`plan_manifest_artifacts`] pure.
#[derive(Debug, Clone)]
pub struct PlannedPatch {
    pub asset_path: String,
    pub base_bundle_id: String,
    pub base_file_hash: String,
    pub patch_file_hash: String,
    pub patch_storage_uri: String,
}

/// One changed asset, with the storage URI it resolves to.
#[derive(Debug, Clone)]
pub struct PlannedAsset {
    pub asset_path: String,
    pub file_hash: String,
    pub storage_uri: String,
    /// `Some("br")` exactly when the asset is stored brotli-compressed.
    pub compression: Option<String>,
}

/// Everything `resolveManifestArtifacts` decides that does not need S3 or the database.
#[derive(Debug)]
pub struct ArtifactPlan {
    pub manifest_file_hash: String,
    pub assets: Vec<PlannedAsset>,
    pub patch: Option<PlannedPatch>,
}

/// Reference: `updateArtifacts.ts` `resolveManifestArtifacts` + `resolveChangedAssets` +
/// `resolveHbcPatchDescriptor`, with every I/O step lifted out to the caller.
///
/// This is the same extraction `decide_update` and `calculate_pagination` already had, and for
/// the same reason: it is the part with all the rules in it, and a pure function can be replayed
/// against the recorded upstream fixtures on any machine, with no Docker. See
/// `tests/artifacts_parity_tests.rs::the_artifact_plan_matches_upstream`.
///
/// `None` means "no artifacts at all" — upstream returns the three fields together or not at
/// all, so a caller must never emit a partial set.
pub fn plan_manifest_artifacts(
    manifest_file_hash: Option<&str>,
    asset_base_storage_uri: Option<&str>,
    target_manifest: &Manifest,
    current_manifest: Option<&Manifest>,
    current_bundle_id: Option<&str>,
    target_patches: &[BundlePatch],
) -> Option<ArtifactPlan> {
    // `if (!manifestStorageUri || !manifestFileHash || !assetBaseStorageUri) return null` --
    // a JS falsy test, so an EMPTY STRING drops the artifacts exactly like NULL does. Reading
    // these as `Option` alone treated `Some("")` as present and emitted `manifestFileHash: ""`
    // alongside a full asset set (fixture cases E04, E21, E22).
    let manifest_file_hash = manifest_file_hash.filter(|value| !value.is_empty())?;
    let asset_base_storage_uri = asset_base_storage_uri.filter(|value| !value.is_empty())?;

    // resolveHbcPatchDescriptor, minus the presign. Every field it guards with `!` is a JS
    // falsy test, so an empty string counts as absent (cases D08, D09, D10).
    let patch = current_bundle_id
        .filter(|id| *id != NIL_UUID)
        .and_then(|id| find_bundle_patch(target_patches, id))
        .filter(|patch| {
            !patch.patch_storage_uri.is_empty()
                && !patch.patch_file_hash.is_empty()
                && !patch.base_file_hash.is_empty()
        })
        .and_then(|patch| {
            resolve_unique_hbc_asset_path(target_manifest).map(|asset_path| PlannedPatch {
                asset_path,
                base_bundle_id: patch.base_bundle_id.clone(),
                base_file_hash: patch.base_file_hash.clone(),
                patch_file_hash: patch.patch_file_hash.clone(),
                patch_storage_uri: patch.patch_storage_uri.clone(),
            })
        });

    let mut assets = Vec::new();
    for (asset_path, asset) in target_manifest.assets.iter() {
        // `if (currentManifest?.assets[assetPath]?.fileHash === asset.fileHash) return null`
        let unchanged = current_manifest
            .and_then(|manifest| manifest.assets.get(asset_path))
            .is_some_and(|previous| previous.file_hash == asset.file_hash);
        if unchanged {
            continue;
        }

        let uses_brotli = uses_brotli_asset(asset_path);
        // `.br` is appended BEFORE the storage URI is resolved, which is why a brotli HBC asset
        // is stored as `<hash>.br` and not `<hash>.bundle`.
        let stored_path = if uses_brotli {
            format!("{asset_path}.br")
        } else {
            asset_path.clone()
        };

        assets.push(PlannedAsset {
            asset_path: asset_path.clone(),
            file_hash: asset.file_hash.clone(),
            storage_uri: resolve_manifest_asset_storage_uri(
                asset_base_storage_uri,
                &stored_path,
                &asset.file_hash,
            )?,
            compression: uses_brotli.then(|| "br".to_string()),
        });
    }

    // Deterministic order, so a caller that serialises this (or a test that compares it) does
    // not depend on `HashMap` iteration order.
    assets.sort_by(|left, right| left.asset_path.cmp(&right.asset_path));

    Some(ArtifactPlan {
        manifest_file_hash: manifest_file_hash.to_string(),
        assets,
        patch,
    })
}

struct ManifestArtifacts {
    manifest_url: String,
    manifest_file_hash: String,
    changed_assets: std::collections::HashMap<String, ChangedAsset>,
}

// Reference: updateArtifacts.ts resolveManifestArtifacts -- manifestUrl/manifestFileHash/
// changedAssets must be returned ALL TOGETHER or NOT AT ALL (Pick<...> | null in the spec).
// A partially resolved response (e.g. manifestUrl present but changedAssets missing) leads
// the native side to apply a broken/incomplete update.
//
// FAILURE POLICY -- and it is NOT uniform, because upstream's is not.
//
// `Ok(None)` means "upstream also returns null here": there is nothing to resolve, or the
// manifest document itself was rejected. `Err` means "upstream throws here", which the caller
// turns into a 5xx. The dividing line is exactly where upstream draws it:
//
//   * A STORAGE failure propagates. `readStorageText` and `resolveFileUrl` throw, and
//     `resolveManifestArtifacts` -> `getAppUpdateInfo` awaits them with no `try`, so the whole
//     update-check fails. Recorded as fixture cases F01, F02, F04, F05 and D14.
//   * A MALFORMED MANIFEST does not. `fetchBundleManifest` catches its own `JSON.parse` and
//     returns null, and `isBundleManifest` rejects a well-formed-but-wrong document the same
//     way, so the update ships with no artifacts (cases E05-E12c, E16).
//   * ONE storage failure is swallowed, and only one: `resolveChangedAssets` wraps the
//     per-asset `resolveFileUrl` in a `try` and rethrows `if (!patch)`. So an asset whose
//     download URL cannot be produced is emitted patch-only when a bsdiff patch covers it
//     (case F03), and fails the request otherwise (F01). That branch is deliberate upstream
//     behaviour, not an oversight -- do not widen the catch to the other assets, and do not
//     narrow it away. `the_per_asset_presign_catch_is_exactly_as_wide_as_upstreams` pins it.
//
// This function used to degrade on every one of those, on the reasoning that artifacts are a
// download optimisation and a 5xx stalls a rollout. That reasoning is sound but the deviation
// was rejected in favour of exact upstream compatibility; see the changelog in
// docs/upstream-parity.md. The consequence to be aware of when reading logs: a transient S3
// fault on the manifest or an asset is now a failed update-check, not a bigger download.
//
// The two DATABASE reads below still degrade. Upstream would propagate those too (its
// `getBundleById` is awaited inside a `Promise.all` with no catch), but there is no recorded
// case for a database fault -- the fixture generator cannot produce one -- and this file does
// not change behaviour that no fixture pins. Called out in the report rather than guessed at.
//
// Every path that degrades MUST log with enough context to spot it; `unwrap_or_default()` /
// `.ok()` with no log is banned here. And EVERY path here -- degrading or propagating -- records
// `ota_update_check_errors_total{app,reason,outcome}` before it returns, with `outcome="failed"`
// when the device is denied the update and `outcome="degraded"` when it merely pays for a bigger
// download. A propagating path that skipped the counter would show up only as an anonymous 500.
async fn resolve_manifest_artifacts(
    state: &AppState,
    bundle: &Bundle,
    current_bundle_id: &str,
    storage: &AppStorageConfig,
) -> Result<Option<ManifestArtifacts>, anyhow::Error> {
    // A falsy (NULL *or* empty) manifest URI means there is nothing to read; the other two
    // columns are checked inside `plan_manifest_artifacts`, which owns that rule.
    let Some(manifest_uri) = bundle
        .manifest_storage_uri
        .as_ref()
        .filter(|uri| !uri.is_empty())
    else {
        return Ok(None);
    };

    // Absent vs unreadable are different answers upstream, and they must stay different here:
    // a manifest that was never uploaded reads as `null` and simply means "no artifacts", while
    // a failed read throws. See `read_s3_file_optional`.
    let manifest_text = match crate::storage::read_s3_file_optional(storage, manifest_uri)
        .await
        .map_err(|err| {
            error!(
                bundle_id = %bundle.id,
                manifest_storage_uri = %manifest_uri,
                error = %err,
                "Failed to read the target manifest; failing the update-check, as upstream does"
            );
            record_update_check_error(
                &bundle.app_name,
                DegradedReason::ManifestUnavailable,
                ErrorOutcome::Failed,
            );
            err
        })? {
        Some(text) => text,
        None => {
            warn!(
                bundle_id = %bundle.id,
                manifest_storage_uri = %manifest_uri,
                "The target manifest object does not exist; serving the update without a \
                 manifest diff (every asset re-downloaded)"
            );
            record_update_check_error(
                &bundle.app_name,
                DegradedReason::ManifestUnavailable,
                ErrorOutcome::Degraded,
            );
            return Ok(None);
        }
    };

    // A document that does not parse, or that `isBundleManifest` would reject, is NOT a storage
    // failure: upstream catches its own `JSON.parse` and returns null, and the update ships with
    // no artifacts. This is the one manifest path that still degrades.
    let target_manifest: Manifest = match serde_json::from_str(&manifest_text) {
        Ok(manifest) => manifest,
        Err(err) => {
            error!(
                bundle_id = %bundle.id,
                manifest_storage_uri = %manifest_uri,
                error = %err,
                "Target manifest is not a valid bundle manifest; serving the update without a \
                 manifest diff (every asset re-downloaded)"
            );
            record_update_check_error(
                &bundle.app_name,
                DegradedReason::ManifestUnavailable,
                ErrorOutcome::Degraded,
            );
            return Ok(None);
        }
    };

    let manifest_url = get_presigned_url(storage, manifest_uri)
        .await
        .map_err(|err| {
            error!(
                bundle_id = %bundle.id,
                manifest_storage_uri = %manifest_uri,
                error = %err,
                "Failed to presign the target manifest; failing the update-check, as upstream does"
            );
            record_update_check_error(
                &bundle.app_name,
                DegradedReason::ManifestUnavailable,
                ErrorOutcome::Failed,
            );
            err
        })?;

    // `current_bundle_id` comes straight off the request path, so it is fully client-controlled.
    // It MUST be scoped to the tenant (`app_name`) -- otherwise app A could name a bundle id
    // belonging to app B and have its manifest diffed (and its patch records matched) here.
    // The scope is taken from `bundle.app_name`: the target bundle was already selected by the
    // app-scoped candidate query, so it is a trusted value rather than request input.
    //
    // A bundle id owned by another app therefore resolves to None -- treated exactly like an id
    // that does not exist at all (a deleted bundle, or a client reporting an unknown id). That is
    // the safe direction for a manifest diff: with no current manifest every asset counts as
    // changed and the client downloads the full set, which is correct-but-slower rather than a
    // broken partial update. Silently degrading also avoids turning the response into an oracle
    // for "does bundle X exist in some other app".
    //
    // "Not found" and "the query failed" both yield None, but only the second is abnormal, so
    // they are separated here: a miss is routine (NIL, deleted bundle, other tenant) and must
    // stay silent, a DB error is not and gets a warning.
    let current_bundle = if current_bundle_id != NIL_UUID {
        match sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE id = ? AND app_name = ?")
            .bind(current_bundle_id)
            .bind(&bundle.app_name)
            .fetch_optional(&state.db)
            .await
        {
            Ok(found) => found,
            Err(err) => {
                warn!(
                    app_name = %bundle.app_name,
                    current_bundle_id = %current_bundle_id,
                    error = %err,
                    "Failed to load the device's current bundle; serving the update without a \
                     manifest diff (every asset re-downloaded)"
                );
                record_update_check_error(
                    &bundle.app_name,
                    DegradedReason::CurrentBundleUnavailable,
                    ErrorOutcome::Degraded,
                );
                None
            }
        }
    } else {
        None
    };

    // The diff base. Reading it FAILS the request exactly as the target manifest does -- upstream
    // fetches both inside the same `Promise.all` and catches neither (case F05). Only the parse
    // still degrades: a current manifest that is malformed leaves `currentManifest` null, which
    // simply marks every asset as changed (case E16).
    let current_manifest: Option<Manifest> = match current_bundle
        .as_ref()
        .and_then(|cb| cb.manifest_storage_uri.as_ref())
        .filter(|uri| !uri.is_empty())
    {
        Some(cm_uri) => {
            let text = crate::storage::read_s3_file_optional(storage, cm_uri)
                .await
                .map_err(|err| {
                    error!(
                        current_manifest_storage_uri = %cm_uri,
                        error = %err,
                        "Failed to read the device's current manifest; failing the update-check, \
                         as upstream does"
                    );
                    record_update_check_error(
                        &bundle.app_name,
                        DegradedReason::ManifestUnavailable,
                        ErrorOutcome::Failed,
                    );
                    err
                })?;
            // Two ways to end up with no diff base, both of which upstream treats identically:
            // the object is absent (`readText` -> null, case E15) or it is not a valid bundle
            // manifest (`fetchBundleManifest` catches its own parse, case E16). Either way every
            // asset is marked changed and the update still ships. Only a failed READ propagates,
            // which is the `?` above.
            match text.as_deref().map(serde_json::from_str::<Manifest>) {
                Some(Ok(manifest)) => Some(manifest),
                Some(Err(err)) => {
                    error!(
                        current_manifest_storage_uri = %cm_uri,
                        error = %err,
                        "The device's current manifest is not a valid bundle manifest; every \
                         asset will be marked changed"
                    );
                    record_update_check_error(
                        &bundle.app_name,
                        DegradedReason::ManifestUnavailable,
                        ErrorOutcome::Degraded,
                    );
                    None
                }
                None => {
                    warn!(
                        current_manifest_storage_uri = %cm_uri,
                        "The device's current manifest object does not exist; every asset will \
                         be marked changed"
                    );
                    record_update_check_error(
                        &bundle.app_name,
                        DegradedReason::CurrentBundleUnavailable,
                        ErrorOutcome::Degraded,
                    );
                    None
                }
            }
        }
        None => None,
    };

    // Tenant scope: scoped DIRECTLY by `app_name`, and it must stay that way. Bundle ids are
    // unique only within an app now that the primary key is `(app_name, id)`, so two tenants
    // can hold the same id and an unscoped `WHERE bundle_id = ?` would return both their patch
    // rows. `bundle.app_name` is a trusted value -- it comes from the app-scoped candidates
    // query in the caller, never from request input.
    //
    // An empty result is ambiguous (no patches recorded vs. the query failed) and the two must
    // not be conflated: losing the patch list costs the device a full HBC download instead of a
    // small bsdiff, which is worth a warning even though the update itself stays correct. Note it
    // cannot select the WRONG base -- `find_bundle_patch` matches `base_bundle_id` exactly, so a
    // missing row yields no patch rather than a mismatched one.
    let target_patches: Vec<BundlePatch> = match sqlx::query_as(
        "SELECT * FROM bundle_patches WHERE app_name = ? AND bundle_id = ? ORDER BY order_index ASC",
    )
    .bind(&bundle.app_name)
    .bind(&bundle.id)
    .fetch_all(&state.db)
    .await
    {
        Ok(patches) => patches,
        Err(err) => {
            warn!(
                app_name = %bundle.app_name,
                bundle_id = %bundle.id,
                error = %err,
                "Failed to load bundle patches; serving the update without a bsdiff patch \
                 (device downloads the full bundle)"
            );
            record_update_check_error(&bundle.app_name, DegradedReason::PatchUnavailable, ErrorOutcome::Degraded);
            Vec::new()
        }
    };

    // Everything above this line is I/O; everything below is the plan being turned into URLs.
    // The rules themselves — which asset counts as changed, which storage URI it resolves to,
    // whether the bsdiff patch applies — live in the pure function, where they are replayed
    // against the recorded upstream fixtures without a container.
    let Some(plan) = plan_manifest_artifacts(
        bundle.manifest_file_hash.as_deref(),
        bundle.asset_base_storage_uri.as_deref(),
        &target_manifest,
        current_manifest.as_ref(),
        current_bundle.as_ref().map(|cb| cb.id.as_str()),
        &target_patches,
    ) else {
        return Ok(None);
    };

    // The patch URL. `resolveHbcPatchDescriptor` awaits `resolveFileUrl` with NO `try` -- unlike
    // the per-asset call below, which has one -- so a patch that cannot be presigned fails the
    // whole update-check (case D14). That asymmetry is upstream's, and it is easy to read as a
    // bug in their code; it is nonetheless what they do.
    let patch = match plan.patch {
        Some(planned) => {
            let patch_url = get_presigned_url(storage, &planned.patch_storage_uri)
                .await
                .map_err(|err| {
                    error!(
                        patch_storage_uri = %planned.patch_storage_uri,
                        error = %err,
                        "Failed to presign the bsdiff patch; failing the update-check, as \
                         upstream does (resolveHbcPatchDescriptor does not catch)"
                    );
                    record_update_check_error(
                        &bundle.app_name,
                        DegradedReason::PatchUnavailable,
                        ErrorOutcome::Failed,
                    );
                    err
                })?;
            Some((
                planned.asset_path,
                ChangedAssetPatch {
                    algorithm: "bsdiff".to_string(),
                    base_bundle_id: planned.base_bundle_id,
                    base_file_hash: planned.base_file_hash,
                    patch_file_hash: planned.patch_file_hash,
                    patch_url,
                },
            ))
        }
        None => None,
    };

    let mut assets_map = std::collections::HashMap::new();
    for planned in plan.assets {
        let patch_for_asset = patch
            .as_ref()
            .filter(|(asset_path, _)| *asset_path == planned.asset_path)
            .map(|(_, patch)| patch.clone());

        // THE ONE CATCH. Reference: resolveChangedAssets --
        //
        //     try { fileUrl = await resolveFileUrl(storageUri, context); }
        //     catch (error) { if (!patch) throw error; }
        //
        // An asset covered by a bsdiff patch survives losing its full-download URL, because the
        // device can still reconstruct it; it is emitted with `patch` and no `file` (case F03).
        // Any other asset rethrows and the update-check fails (cases F01, F02).
        //
        // Do not widen this to a blanket `.ok()` and do not remove it: both directions are
        // real deviations, and `the_per_asset_presign_catch_is_exactly_as_wide_as_upstreams`
        // fails on either.
        let file = match get_presigned_url(storage, &planned.storage_uri).await {
            Ok(url) => Some(ChangedAssetFile {
                url,
                compression: planned.compression,
            }),
            Err(err) => {
                if patch_for_asset.is_none() {
                    error!(
                        asset_path = %planned.asset_path,
                        asset_storage_uri = %planned.storage_uri,
                        error = %err,
                        "Failed to presign a changed asset that no bsdiff patch covers; failing \
                         the update-check, as upstream does"
                    );
                    record_update_check_error(
                        &bundle.app_name,
                        DegradedReason::AssetUnavailable,
                        ErrorOutcome::Failed,
                    );
                    return Err(err);
                }
                warn!(
                    asset_path = %planned.asset_path,
                    asset_storage_uri = %planned.storage_uri,
                    error = %err,
                    "Failed to presign a changed asset, but a bsdiff patch covers it; serving it \
                     patch-only, which is upstream's one caught failure"
                );
                record_update_check_error(
                    &bundle.app_name,
                    DegradedReason::AssetUnavailable,
                    ErrorOutcome::Degraded,
                );
                None
            }
        };

        assets_map.insert(
            planned.asset_path,
            ChangedAsset {
                file,
                file_hash: planned.file_hash,
                patch: patch_for_asset,
            },
        );
    }

    Ok(Some(ManifestArtifacts {
        manifest_url,
        manifest_file_hash: plan.manifest_file_hash,
        changed_assets: assets_map,
    }))
}

/// `Err` here means the response cannot be served at all — see the note on `file_url` below.
async fn make_response(
    state: &AppState,
    bundle: &Bundle,
    status: &str,
    current_bundle_id: &str,
    storage: &AppStorageConfig,
) -> Result<AppUpdateInfo, anyhow::Error> {
    // The ONE place in this file that must not degrade.
    //
    // `fileUrl: null` is legal in upstream's type (`AppUpdateAvailableInfo.fileUrl: string | null`
    // in @hot-updater/core 0.35.8) but in the protocol it MEANS "reset to the built-in bundle":
    // the native side treats a null url as an instruction to clear the bundle URL, delete every
    // downloaded bundle and report success (`BundleFileStorageService.kt`, `.swift`). Pairing it
    // with `UPDATE` therefore does not fail an update, it silently destroys the device's OTA
    // state -- and with `shouldForceUpdate` the client reloads, asks again, gets the same answer
    // and loops, because its loop guard only compares bundle ids and the id really is newer than
    // the built-in one.
    //
    // Upstream never emits that combination: `resolveFileUrl` (server/src/storageAccess.ts)
    // returns null only for a null `storageUri` and throws on every real failure, and
    // `pluginCore.ts` awaits it with no catch, so a presign failure becomes a 5xx.
    //
    // A 5xx is retried harmlessly by the client. So: fail the request. Every OTHER artifact in
    // this response is an optimisation and still degrades to null -- see the failure policy on
    // `resolve_manifest_artifacts`.
    let file_url = get_presigned_url(storage, &bundle.storage_uri)
        .await
        .map_err(|err| {
            error!(
                bundle_id = %bundle.id,
                storage_uri = %bundle.storage_uri,
                error = %err,
                "Failed to presign the bundle; failing the update-check rather than telling the \
                 device to update with nothing to download"
            );
            record_update_check_error(
                &bundle.app_name,
                DegradedReason::PresignFailed,
                ErrorOutcome::Failed,
            );
            err
        })?;

    // `?` here is the whole of the degrade-vs-5xx decision: a storage failure while resolving
    // artifacts now fails the update-check, matching upstream. `Ok(None)` is the other outcome —
    // the bundle simply has no artifacts to offer — and still ships the update.
    let manifest_artifacts =
        resolve_manifest_artifacts(state, bundle, current_bundle_id, storage).await?;

    let (manifest_url, manifest_file_hash, changed_assets) = match manifest_artifacts {
        Some(artifacts) => (
            Some(artifacts.manifest_url),
            Some(artifacts.manifest_file_hash),
            Some(artifacts.changed_assets),
        ),
        None => (None, None, None),
    };

    Ok(AppUpdateInfo {
        id: bundle.id.clone(),
        status: status.to_string(),
        // Reference: pluginCore.ts makeResponse -- ROLLBACK is always a force update,
        // regardless of the bundle's own should_force_update field.
        should_force_update: status == "ROLLBACK" || bundle.should_force_update != 0,
        file_hash: Some(bundle.file_hash.clone()),
        message: bundle.message.clone(),
        file_url: Some(file_url),
        manifest_url,
        manifest_file_hash,
        changed_assets,
    })
}

// Struct to store query context/parameters for checking updates
#[allow(dead_code)]
struct UpdateCheckParams {
    app_name: String,
    platform: String,
    channel: String,
    min_bundle_id: String,
    bundle_id: String,
    cohort: Option<String>,
}

/// Reference: @hot-updater/js getUpdateInfo -> appVersionStrategy/fingerprintStrategy
/// (the state machine is identical in both). Pure/synchronous -- no DB or S3 access, which
/// is why `tests/decision_tests.rs` can test it against the real package via fixtures.
/// `candidates` must ALREADY be filtered by the caller on platform/channel/enabled/
/// version-or-fingerprint match. The order is NOT relied upon: both candidate loops below take
/// the maximum id, exactly as upstream does, so a caller ordering that disagrees with the
/// byte-wise `<`/`>` used here (the id columns are case-insensitive `ascii_general_ci`, these
/// comparisons are not) cannot change the outcome.
pub enum Decision {
    Update(Bundle),
    Rollback(Bundle),
    /// Reference: INIT_BUNDLE_ROLLBACK_UPDATE_INFO -- forced return to the native bundle.
    InitRollback,
    NoUpdate,
}

pub fn decide_update(
    candidates: &[Bundle],
    cohort: Option<&str>,
    client_bundle_id: &str,
    min_bundle_id: &str,
) -> Decision {
    let current_bundle = candidates.iter().find(|b| b.id == client_bundle_id);

    let is_current_eligible = match current_bundle {
        Some(cb) => {
            let targets = parse_target_cohorts(&cb.target_cohorts);
            is_cohort_eligible_for_update(
                &cb.id,
                cohort,
                Some(cb.rollout_cohort_count),
                targets.as_deref(),
            )
        }
        None => false,
    };

    // update_candidate: highest-ID candidate (> client_bundle_id) eligible for the cohort.
    // Reference: findLatestEligibleUpdateCandidate -- `if (bundle.id.localeCompare(bundleId) > 0
    // && isEligibleUpdateCandidate(bundle, cohort) && (!updateCandidate ||
    // bundle.id.localeCompare(updateCandidate.id) > 0)) updateCandidate = bundle;`
    //
    // Upstream scans the whole list and keeps the maximum rather than stopping at the first hit,
    // and so do we. An early `break` would be correct only while the caller's ordering agrees
    // with the `>` used here; the id columns are `ascii_general_ci` (case-INSENSITIVE) whereas
    // this comparison is byte-wise, so `ORDER BY id DESC` and this loop can disagree for
    // mixed-case hex ids. Taking the maximum makes the result independent of the input order.
    let mut update_candidate: Option<&Bundle> = None;
    for b in candidates {
        if b.id.as_str() > client_bundle_id && update_candidate.is_none_or(|uc| b.id > uc.id) {
            let targets = parse_target_cohorts(&b.target_cohorts);
            if is_cohort_eligible_for_update(
                &b.id,
                cohort,
                Some(b.rollout_cohort_count),
                targets.as_deref(),
            ) {
                update_candidate = Some(b);
            }
        }
    }

    // rollback_candidate: highest-ID candidate (< client_bundle_id) -- NO cohort/rollout test.
    // Reference: appVersionStrategy/fingerprintStrategy --
    //   `else if (bundleId !== NIL_UUID && b.id.localeCompare(bundleId) < 0) {
    //        if (!rollbackCandidate || b.id.localeCompare(rollbackCandidate.id) > 0) ... }`
    // Only `updateCandidate` goes through `isEligibleUpdateCandidate`; the rollback target is
    // selected purely by id. Filtering it by cohort would turn a one-step rollback into a full
    // native rollback (INIT_BUNDLE_ROLLBACK_UPDATE_INFO) whenever the older bundle happens to
    // sit outside the rollout -- which is exactly the situation a rollback exists for.
    // As with `update_candidate`, upstream keeps the maximum over the whole list rather than
    // stopping at the first hit, so this does not depend on the caller's ordering either.
    let mut rollback_candidate: Option<&Bundle> = None;
    for b in candidates {
        if b.id.as_str() < client_bundle_id && rollback_candidate.is_none_or(|rc| b.id > rc.id) {
            rollback_candidate = Some(b);
        }
    }

    if client_bundle_id == NIL_UUID {
        return match update_candidate {
            Some(uc) => Decision::Update(uc.clone()),
            None => Decision::NoUpdate,
        };
    }

    if is_current_eligible {
        return match update_candidate {
            Some(uc) => Decision::Update(uc.clone()),
            None => Decision::NoUpdate,
        };
    }

    // The current bundle has fallen outside the rollout/cohort
    if let Some(uc) = update_candidate {
        Decision::Update(uc.clone())
    } else if let Some(rc) = rollback_candidate {
        Decision::Rollback(rc.clone())
    } else if client_bundle_id <= min_bundle_id {
        Decision::NoUpdate
    } else {
        Decision::InitRollback
    }
}

/// `Ok(None)` is "no update for this device"; `Err` is "this response cannot be served" and
/// becomes a 5xx. The two are very different and must not be conflated: see `make_response`.
async fn evaluate_update(
    state: &AppState,
    params: UpdateCheckParams,
    candidates: Vec<Bundle>,
) -> Result<Option<AppUpdateInfo>, anyhow::Error> {
    let Some(app_config) = state.config.get_app_config(&params.app_name) else {
        return Ok(None);
    };
    let storage = &app_config.storage;

    let decision = decide_update(
        &candidates,
        params.cohort.as_deref(),
        &params.bundle_id,
        &params.min_bundle_id,
    );

    // InitRollback is folded into ROLLBACK because that is what the client is told.
    let outcome = match &decision {
        Decision::Update(_) => UpdateOutcome::Update,
        Decision::Rollback(_) | Decision::InitRollback => UpdateOutcome::Rollback,
        Decision::NoUpdate => UpdateOutcome::UpToDate,
    };

    let response = match decision {
        Decision::Update(b) => {
            Some(make_response(state, &b, "UPDATE", &params.bundle_id, storage).await?)
        }
        Decision::Rollback(b) => {
            Some(make_response(state, &b, "ROLLBACK", &params.bundle_id, storage).await?)
        }
        // The one legitimate `file_url: null`: upstream's INIT_BUNDLE_ROLLBACK_UPDATE_INFO, the
        // reset-to-built-in shape the null is reserved for. It carries NIL_UUID and ROLLBACK,
        // which is exactly what the client checks for before treating a null url as a reset.
        Decision::InitRollback => Some(AppUpdateInfo {
            id: NIL_UUID.to_string(),
            status: "ROLLBACK".to_string(),
            should_force_update: true,
            file_hash: None,
            message: None,
            file_url: None,
            manifest_url: None,
            manifest_file_hash: None,
            changed_assets: None,
        }),
        Decision::NoUpdate => None,
    };

    // Counted here rather than beside the decision, because `make_response` can fail and
    // the `?` above returns before this line. A presign failure is answered with a 500, and
    // counting the decision first would report an UPDATE that no device received -- the
    // reading an operator takes from this metric is "updates shipped", so during a storage
    // incident the graph would look healthy while every device got nothing.
    record_update_check(&params.app_name, &params.platform, outcome);

    Ok(response)
}

// Helper to fetch and evaluate app version candidates
#[allow(clippy::too_many_arguments)]
async fn check_app_version_helper(
    headers: HeaderMap,
    state: AppState,
    app_name: String,
    platform: String,
    app_version: String,
    channel: String,
    min_bundle_id: String,
    bundle_id: String,
    cohort: Option<String>,
) -> impl IntoResponse {
    if state.config.get_app_config(&app_name).is_none() {
        return (StatusCode::NOT_FOUND, "Application not found").into_response();
    }

    if platform != "ios" && platform != "android" {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid platform. Use 'ios' or 'android'.",
        )
            .into_response();
    }

    let parsed_client_version = match coerce_version(&app_version) {
        Some(v) => v,
        None => return (StatusCode::BAD_REQUEST, "Invalid appVersion format").into_response(),
    };

    // Query candidates matching app, platform, channel, enabled, id >= min_bundle_id
    let candidates_result = sqlx::query_as::<_, Bundle>(
        "SELECT * FROM bundles WHERE app_name = ? AND platform = ? AND channel = ? AND enabled = 1 AND id >= ? ORDER BY id DESC"
    )
    .bind(&app_name)
    .bind(&platform)
    .bind(&channel)
    .bind(&min_bundle_id)
    .fetch_all(&state.db)
    .await;

    let candidates = match candidates_result {
        Ok(c) => c,
        Err(err) => {
            error!("Database check query failed: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    // Filter candidates based on semver satisfying client's app_version
    let filtered_candidates: Vec<Bundle> = candidates
        .into_iter()
        .filter(|b| {
            matches_target_app_version(b.target_app_version.as_deref(), &parsed_client_version)
        })
        .collect();

    let params = UpdateCheckParams {
        app_name,
        platform,
        channel,
        min_bundle_id,
        bundle_id,
        cohort,
    };

    let result = evaluate_update(&state, params, filtered_candidates).await;

    match result {
        Ok(Some(info)) => Json(serde_json::to_value(&info).unwrap()).into_response(),
        Ok(None) => {
            if supports_explicit_no_update_response(&headers) {
                Json(serde_json::json!({ "status": "UP_TO_DATE" })).into_response()
            } else {
                Json(serde_json::json!(null)).into_response()
            }
        }
        // The bundle exists and was chosen, but no download URL could be produced for it. The
        // client retries a failed check harmlessly; it would act on a null `fileUrl`.
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Storage error").into_response(),
    }
}

// GET /:app/hot-updater/app-version/:platform/:appVersion/:channel/:minBundleId/:bundleId
pub async fn check_app_version_no_cohort(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, platform, app_version, channel, min_bundle_id, bundle_id)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
) -> impl IntoResponse {
    check_app_version_helper(
        headers,
        state,
        app_name,
        platform,
        app_version,
        channel,
        min_bundle_id,
        bundle_id,
        None,
    )
    .await
}

// GET /:app/hot-updater/app-version/:platform/:appVersion/:channel/:minBundleId/:bundleId/:cohort
pub async fn check_app_version_with_cohort(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, platform, app_version, channel, min_bundle_id, bundle_id, cohort)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
) -> impl IntoResponse {
    check_app_version_helper(
        headers,
        state,
        app_name,
        platform,
        app_version,
        channel,
        min_bundle_id,
        bundle_id,
        Some(cohort),
    )
    .await
}

// Helper to fetch and evaluate fingerprint candidates
#[allow(clippy::too_many_arguments)]
async fn check_fingerprint_helper(
    headers: HeaderMap,
    state: AppState,
    app_name: String,
    platform: String,
    fingerprint_hash: String,
    channel: String,
    min_bundle_id: String,
    bundle_id: String,
    cohort: Option<String>,
) -> impl IntoResponse {
    if state.config.get_app_config(&app_name).is_none() {
        return (StatusCode::NOT_FOUND, "Application not found").into_response();
    }

    if platform != "ios" && platform != "android" {
        return (
            StatusCode::BAD_REQUEST,
            "Invalid platform. Use 'ios' or 'android'.",
        )
            .into_response();
    }

    // Query candidates matching app, platform, channel, enabled, id >= min_bundle_id, AND exact
    // fingerprint_hash. `fingerprint_hash <> ''` reproduces upstream's `!b.fingerprintHash` arm:
    // a stored empty hash is falsy in JS and must not match even an empty request hash (a NULL
    // hash is already excluded by the `=` comparison).
    let candidates_result = sqlx::query_as::<_, Bundle>(
        "SELECT * FROM bundles WHERE app_name = ? AND platform = ? AND channel = ? AND enabled = 1 AND id >= ? AND fingerprint_hash = ? AND fingerprint_hash <> '' ORDER BY id DESC"
    )
    .bind(&app_name)
    .bind(&platform)
    .bind(&channel)
    .bind(&min_bundle_id)
    .bind(&fingerprint_hash)
    .fetch_all(&state.db)
    .await;

    let candidates = match candidates_result {
        Ok(c) => c,
        Err(err) => {
            error!("Database check query failed: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    // Redundant with the SQL predicates above, but it keeps the fingerprint match rule in one
    // place (`matches_fingerprint`) -- the same function `tests/decision_tests.rs` replays -- so
    // the two cannot drift apart, and it does not depend on the DB collation.
    let candidates: Vec<Bundle> = candidates
        .into_iter()
        .filter(|b| matches_fingerprint(b.fingerprint_hash.as_deref(), &fingerprint_hash))
        .collect();

    let params = UpdateCheckParams {
        app_name,
        platform,
        channel,
        min_bundle_id,
        bundle_id,
        cohort,
    };

    let result = evaluate_update(&state, params, candidates).await;

    match result {
        Ok(Some(info)) => Json(serde_json::to_value(&info).unwrap()).into_response(),
        Ok(None) => {
            if supports_explicit_no_update_response(&headers) {
                Json(serde_json::json!({ "status": "UP_TO_DATE" })).into_response()
            } else {
                Json(serde_json::json!(null)).into_response()
            }
        }
        // The bundle exists and was chosen, but no download URL could be produced for it. The
        // client retries a failed check harmlessly; it would act on a null `fileUrl`.
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Storage error").into_response(),
    }
}

// GET /:app/hot-updater/fingerprint/:platform/:fingerprintHash/:channel/:minBundleId/:bundleId
pub async fn check_fingerprint_no_cohort(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, platform, fingerprint_hash, channel, min_bundle_id, bundle_id)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
) -> impl IntoResponse {
    check_fingerprint_helper(
        headers,
        state,
        app_name,
        platform,
        fingerprint_hash,
        channel,
        min_bundle_id,
        bundle_id,
        None,
    )
    .await
}

// GET /:app/hot-updater/fingerprint/:platform/:fingerprintHash/:channel/:minBundleId/:bundleId/:cohort
pub async fn check_fingerprint_with_cohort(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, platform, fingerprint_hash, channel, min_bundle_id, bundle_id, cohort)): Path<
        (String, String, String, String, String, String, String),
    >,
) -> impl IntoResponse {
    check_fingerprint_helper(
        headers,
        state,
        app_name,
        platform,
        fingerprint_hash,
        channel,
        min_bundle_id,
        bundle_id,
        Some(cohort),
    )
    .await
}
