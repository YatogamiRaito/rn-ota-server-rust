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
use crate::observability::{record_update_check, UpdateOutcome};
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
    pub file_hash: Option<String>,
    pub message: Option<String>,
    pub file_url: Option<String>,
    pub manifest_url: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub changed_assets: Option<std::collections::HashMap<String, ChangedAsset>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedAsset {
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[allow(dead_code)]
    bundle_id: String,
    assets: std::collections::HashMap<String, ManifestAsset>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ManifestAsset {
    file_hash: String,
}

fn uses_brotli_asset(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if let Some(filename) = parts.last() {
        if filename.starts_with("index.") && filename.ends_with(".bundle") {
            return filename.len() > "index..bundle".len();
        }
    }
    false
}

fn resolve_manifest_asset_storage_uri(
    asset_base_storage_uri: &str,
    asset_path: &str,
    file_hash: &str,
) -> String {
    let parsed_url = match url::Url::parse(asset_base_storage_uri) {
        Ok(u) => u,
        Err(_) => {
            return format!(
                "{}/{}",
                asset_base_storage_uri.trim_end_matches('/'),
                asset_path
            )
        }
    };

    let path_normalized = parsed_url.path().trim_end_matches('/');
    let is_content_addressed = path_normalized.ends_with("/assets") || path_normalized == "/assets";

    if is_content_addressed {
        let ext = if asset_path.ends_with(".br") {
            ".br"
        } else if let Some(pos) = asset_path.rfind('.') {
            &asset_path[pos..]
        } else {
            ""
        };

        let prefix = &file_hash[0..2];
        let relative_path = format!("sha256/{}/{}{}", prefix, file_hash, ext);

        let mut u = parsed_url.clone();
        let new_path = format!("{}/{}", path_normalized, relative_path);
        u.set_path(&new_path);
        u.to_string()
    } else {
        let mut u = parsed_url.clone();
        let new_path = format!("{}/{}", path_normalized, asset_path.replace('\\', "/"));
        u.set_path(&new_path);
        u.to_string()
    }
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
    let value = val.as_ref()?;
    match serde_json::from_value::<Vec<String>>(value.clone()) {
        Ok(cohorts) => Some(cohorts),
        Err(err) => {
            warn!(
                error = %err,
                "Malformed target_cohorts on a bundle; treating it as untargeted (rollout only)"
            );
            None
        }
    }
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

// Reference: @hot-updater/core getBundlePatch -- finds the record in the target bundle's
// patch list whose base_bundle_id == currentBundle.id.
//
// COLLATION: both sides come from the database, and the id columns are `ascii_general_ci`, so
// MySQL considers `…000AB` and `…000ab` the same value -- the FK from `bundle_patches.base_bundle_id`
// to `bundles.id` accepts a reference written in the other case (verified on MySQL 8.0.46). A
// byte-wise `==` here would reject a patch row the database itself regards as a valid reference,
// silently costing the device a full download. Matching the column's own semantics instead.
// REVERT THIS if the id columns ever move back to a `_bin` collation: byte equality would then
// be the correct comparison and this normalisation would be wrong.
fn find_bundle_patch<'a>(
    patches: &'a [BundlePatch],
    base_bundle_id: &str,
) -> Option<&'a BundlePatch> {
    patches
        .iter()
        .find(|p| p.base_bundle_id.eq_ignore_ascii_case(base_bundle_id))
}

struct HbcPatchDescriptor {
    asset_path: String,
    patch: ChangedAssetPatch,
}

// Reference: updateArtifacts.ts resolveHbcPatchDescriptor -- if a bsdiff patch record exists
// from the current bundle to the target bundle (and a unique HBC asset can be resolved in the
// manifest), produces a presigned patch URL.
async fn resolve_hbc_patch_descriptor(
    current_bundle: Option<&Bundle>,
    target_patches: &[BundlePatch],
    target_manifest: &Manifest,
    storage: &AppStorageConfig,
) -> Option<HbcPatchDescriptor> {
    let current_bundle = current_bundle?;
    let matching_patch = find_bundle_patch(target_patches, &current_bundle.id)?;
    let patch_asset_path = resolve_unique_hbc_asset_path(target_manifest)?;

    // A presign failure drops only the patch, not the update: the asset keeps its full-download
    // `file` URL. Logged rather than swallowed -- see the note on `resolve_manifest_artifacts`.
    let patch_url = match get_presigned_url(storage, &matching_patch.patch_storage_uri).await {
        Ok(url) => url,
        Err(err) => {
            warn!(
                patch_storage_uri = %matching_patch.patch_storage_uri,
                error = %err,
                "Failed to presign bsdiff patch; device will download the full bundle instead"
            );
            return None;
        }
    };

    Some(HbcPatchDescriptor {
        asset_path: patch_asset_path,
        patch: ChangedAssetPatch {
            algorithm: "bsdiff".to_string(),
            base_bundle_id: matching_patch.base_bundle_id.clone(),
            base_file_hash: matching_patch.base_file_hash.clone(),
            patch_file_hash: matching_patch.patch_file_hash.clone(),
            patch_url,
        },
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
// FAILURE POLICY for this whole function: everything resolved here is a DOWNLOAD OPTIMISATION,
// never a correctness input. The manifest diff and the bsdiff patch exist only to make the
// device transfer fewer bytes; whatever they fail to resolve simply falls back to a full
// download, which is always correct. Deciding WHICH bundle the device gets happens earlier, in
// `decide_update`, and never depends on anything in here.
//
// So failures degrade rather than 500. That is a deliberate choice, not laziness: returning 500
// on the device update-check path turns "the update ships, just as a bigger download" into "no
// update at all", because a hot-updater client treats a failed check as "stay on the current
// bundle". A transient DB or S3 blip would then stall a rollout instead of merely slowing it.
//
// The cost of that choice is invisibility, so every degrading path below MUST log a warning
// with enough context to spot it. `unwrap_or_default()` / `.ok()` with no log is banned here.
// NOTE for observability: there is currently no counter for "served degraded". A
// `ota_update_check_degraded_total{app,reason}` counter in `src/observability.rs` would make
// these warnings alertable; that file is not owned here, so it is left as a recommendation.
async fn resolve_manifest_artifacts(
    state: &AppState,
    bundle: &Bundle,
    current_bundle_id: &str,
    storage: &AppStorageConfig,
) -> Option<ManifestArtifacts> {
    let manifest_uri = bundle.manifest_storage_uri.as_ref()?;
    let asset_base_uri = bundle.asset_base_storage_uri.as_ref()?;
    let manifest_file_hash = bundle.manifest_file_hash.clone()?;

    let manifest_text = match crate::storage::read_s3_file(storage, manifest_uri).await {
        Ok(text) => text,
        Err(err) => {
            error!(
                "Failed to read target manifest from S3 {}: {}",
                manifest_uri, err
            );
            return None;
        }
    };

    let target_manifest: Manifest = match serde_json::from_str(&manifest_text) {
        Ok(m) => m,
        Err(err) => {
            error!(
                "Failed to parse target manifest JSON from {}: {}",
                manifest_uri, err
            );
            return None;
        }
    };

    let manifest_url = match get_presigned_url(storage, manifest_uri).await {
        Ok(url) => url,
        Err(err) => {
            error!(
                "Failed to generate presigned url for manifest of bundle {}: {}",
                bundle.id, err
            );
            return None;
        }
    };

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
                None
            }
        }
    } else {
        None
    };

    let current_manifest: Option<Manifest> = match &current_bundle {
        Some(cb) => match &cb.manifest_storage_uri {
            Some(cm_uri) => match crate::storage::read_s3_file(storage, cm_uri).await {
                Ok(text) => match serde_json::from_str::<Manifest>(&text) {
                    Ok(m) => Some(m),
                    Err(err) => {
                        error!(
                            "Failed to parse current manifest JSON from {}: {}",
                            cm_uri, err
                        );
                        None
                    }
                },
                Err(err) => {
                    error!(
                        "Failed to read current manifest from S3 {}: {}",
                        cm_uri, err
                    );
                    None
                }
            },
            None => None,
        },
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
            Vec::new()
        }
    };

    let patch_descriptor = resolve_hbc_patch_descriptor(
        current_bundle.as_ref(),
        &target_patches,
        &target_manifest,
        storage,
    )
    .await;

    let mut assets_map = std::collections::HashMap::new();
    for (asset_path, asset) in target_manifest.assets.iter() {
        let is_changed = match &current_manifest {
            Some(cm) => match cm.assets.get(asset_path) {
                Some(ca) => ca.file_hash != asset.file_hash,
                None => true,
            },
            None => true,
        };
        if !is_changed {
            continue;
        }

        let uses_brotli = uses_brotli_asset(asset_path);
        let download_path = if uses_brotli {
            format!("{}.br", asset_path)
        } else {
            asset_path.clone()
        };
        let asset_storage_uri =
            resolve_manifest_asset_storage_uri(asset_base_uri, &download_path, &asset.file_hash);

        let patch_for_asset = patch_descriptor
            .as_ref()
            .filter(|d| &d.asset_path == asset_path)
            .map(|d| d.patch.clone());

        let file = match get_presigned_url(storage, &asset_storage_uri).await {
            Ok(url) => Some(ChangedAssetFile {
                url,
                compression: if uses_brotli {
                    Some("br".to_string())
                } else {
                    None
                },
            }),
            Err(err) => {
                if patch_for_asset.is_none() {
                    // Reference: resolveChangedAssets -- if neither the full file nor a
                    // patch can be resolved, the ENTIRE changedAssets set is considered
                    // invalid (the client falls back to a full bundle download); a
                    // missing/broken set is NEVER returned.
                    error!(
                        "Failed to generate presigned url for asset {}: {}",
                        asset_storage_uri, err
                    );
                    return None;
                }
                None
            }
        };

        assets_map.insert(
            asset_path.clone(),
            ChangedAsset {
                file,
                file_hash: asset.file_hash.clone(),
                patch: patch_for_asset,
            },
        );
    }

    Some(ManifestArtifacts {
        manifest_url,
        manifest_file_hash,
        changed_assets: assets_map,
    })
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
            err
        })?;

    let manifest_artifacts =
        resolve_manifest_artifacts(state, bundle, current_bundle_id, storage).await;

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

    // Observability only: a counter increment, no effect on the decision or the response.
    // InitRollback is folded into ROLLBACK because that is what the client is told.
    record_update_check(
        &params.app_name,
        &params.platform,
        match &decision {
            Decision::Update(_) => UpdateOutcome::Update,
            Decision::Rollback(_) | Decision::InitRollback => UpdateOutcome::Rollback,
            Decision::NoUpdate => UpdateOutcome::UpToDate,
        },
    );

    match decision {
        Decision::Update(b) => Ok(Some(
            make_response(state, &b, "UPDATE", &params.bundle_id, storage).await?,
        )),
        Decision::Rollback(b) => Ok(Some(
            make_response(state, &b, "ROLLBACK", &params.bundle_id, storage).await?,
        )),
        // The one legitimate `file_url: null`: upstream's INIT_BUNDLE_ROLLBACK_UPDATE_INFO, the
        // reset-to-built-in shape the null is reserved for. It carries NIL_UUID and ROLLBACK,
        // which is exactly what the client checks for before treating a null url as a reset.
        Decision::InitRollback => Ok(Some(AppUpdateInfo {
            id: NIL_UUID.to_string(),
            status: "ROLLBACK".to_string(),
            should_force_update: true,
            file_hash: None,
            message: None,
            file_url: None,
            manifest_url: None,
            manifest_file_hash: None,
            changed_assets: None,
        })),
        Decision::NoUpdate => Ok(None),
    }
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
