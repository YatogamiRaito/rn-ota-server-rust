use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::cohort::is_cohort_eligible_for_update;
use crate::models::{Bundle, BundlePatch};
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

fn parse_target_cohorts(val: &Option<serde_json::Value>) -> Option<Vec<String>> {
    val.as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
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
fn find_bundle_patch<'a>(
    patches: &'a [BundlePatch],
    base_bundle_id: &str,
) -> Option<&'a BundlePatch> {
    patches.iter().find(|p| p.base_bundle_id == base_bundle_id)
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
    endpoint: Option<&str>,
    access_key: &str,
    secret_key: &str,
    bucket_name: &str,
) -> Option<HbcPatchDescriptor> {
    let current_bundle = current_bundle?;
    let matching_patch = find_bundle_patch(target_patches, &current_bundle.id)?;
    let patch_asset_path = resolve_unique_hbc_asset_path(target_manifest)?;

    let patch_url = get_presigned_url(
        endpoint,
        access_key,
        secret_key,
        bucket_name,
        &matching_patch.patch_storage_uri,
    )
    .await
    .ok()?;

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
async fn resolve_manifest_artifacts(
    state: &AppState,
    bundle: &Bundle,
    current_bundle_id: &str,
    endpoint: Option<&str>,
    access_key: &str,
    secret_key: &str,
    bucket_name: &str,
) -> Option<ManifestArtifacts> {
    let manifest_uri = bundle.manifest_storage_uri.as_ref()?;
    let asset_base_uri = bundle.asset_base_storage_uri.as_ref()?;
    let manifest_file_hash = bundle.manifest_file_hash.clone()?;

    let manifest_text = match crate::storage::read_s3_file(
        endpoint,
        access_key,
        secret_key,
        bucket_name,
        manifest_uri,
    )
    .await
    {
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

    let manifest_url = match get_presigned_url(
        endpoint,
        access_key,
        secret_key,
        bucket_name,
        manifest_uri,
    )
    .await
    {
        Ok(url) => url,
        Err(err) => {
            error!(
                "Failed to generate presigned url for manifest of bundle {}: {}",
                bundle.id, err
            );
            return None;
        }
    };

    let current_bundle = if current_bundle_id != NIL_UUID {
        sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE id = ?")
            .bind(current_bundle_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let current_manifest: Option<Manifest> = match &current_bundle {
        Some(cb) => match &cb.manifest_storage_uri {
            Some(cm_uri) => match crate::storage::read_s3_file(
                endpoint,
                access_key,
                secret_key,
                bucket_name,
                cm_uri,
            )
            .await
            {
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

    let target_patches: Vec<BundlePatch> =
        sqlx::query_as("SELECT * FROM bundle_patches WHERE bundle_id = ? ORDER BY order_index ASC")
            .bind(&bundle.id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

    let patch_descriptor = resolve_hbc_patch_descriptor(
        current_bundle.as_ref(),
        &target_patches,
        &target_manifest,
        endpoint,
        access_key,
        secret_key,
        bucket_name,
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

        let file = match get_presigned_url(
            endpoint,
            access_key,
            secret_key,
            bucket_name,
            &asset_storage_uri,
        )
        .await
        {
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

#[allow(clippy::too_many_arguments)]
async fn make_response(
    state: &AppState,
    bundle: &Bundle,
    status: &str,
    current_bundle_id: &str,
    endpoint: Option<&str>,
    access_key: &str,
    secret_key: &str,
    bucket_name: &str,
) -> AppUpdateInfo {
    let file_url = match get_presigned_url(
        endpoint,
        access_key,
        secret_key,
        bucket_name,
        &bundle.storage_uri,
    )
    .await
    {
        Ok(url) => Some(url),
        Err(err) => {
            error!(
                "Failed to generate presigned url for bundle {}: {}",
                bundle.id, err
            );
            None
        }
    };

    let manifest_artifacts = resolve_manifest_artifacts(
        state,
        bundle,
        current_bundle_id,
        endpoint,
        access_key,
        secret_key,
        bucket_name,
    )
    .await;

    let (manifest_url, manifest_file_hash, changed_assets) = match manifest_artifacts {
        Some(artifacts) => (
            Some(artifacts.manifest_url),
            Some(artifacts.manifest_file_hash),
            Some(artifacts.changed_assets),
        ),
        None => (None, None, None),
    };

    AppUpdateInfo {
        id: bundle.id.clone(),
        status: status.to_string(),
        // Reference: pluginCore.ts makeResponse -- ROLLBACK is always a force update,
        // regardless of the bundle's own should_force_update field.
        should_force_update: status == "ROLLBACK" || bundle.should_force_update != 0,
        file_hash: Some(bundle.file_hash.clone()),
        message: bundle.message.clone(),
        file_url,
        manifest_url,
        manifest_file_hash,
        changed_assets,
    }
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
/// version-or-fingerprint match, and sorted by id DESC.
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

    // update_candidate: highest-ID candidate (> client_bundle_id) eligible for the cohort
    let mut update_candidate: Option<&Bundle> = None;
    for b in candidates {
        if b.id.as_str() > client_bundle_id {
            let targets = parse_target_cohorts(&b.target_cohorts);
            if is_cohort_eligible_for_update(
                &b.id,
                cohort,
                Some(b.rollout_cohort_count),
                targets.as_deref(),
            ) {
                update_candidate = Some(b);
                break; // candidates are sorted id DESC, so the first match is the newest
            }
        }
    }

    // rollback_candidate: highest-ID candidate (< client_bundle_id) eligible for the cohort
    let mut rollback_candidate: Option<&Bundle> = None;
    for b in candidates {
        if b.id.as_str() < client_bundle_id {
            let targets = parse_target_cohorts(&b.target_cohorts);
            if is_cohort_eligible_for_update(
                &b.id,
                cohort,
                Some(b.rollout_cohort_count),
                targets.as_deref(),
            ) {
                rollback_candidate = Some(b);
                break;
            }
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

async fn evaluate_update(
    state: &AppState,
    params: UpdateCheckParams,
    candidates: Vec<Bundle>,
) -> Option<AppUpdateInfo> {
    let app_config = state.config.get_app_config(&params.app_name)?;
    let storage = &app_config.storage;

    let decision = decide_update(
        &candidates,
        params.cohort.as_deref(),
        &params.bundle_id,
        &params.min_bundle_id,
    );

    match decision {
        Decision::Update(b) => Some(
            make_response(
                state,
                &b,
                "UPDATE",
                &params.bundle_id,
                storage.endpoint.as_deref(),
                &storage.access_key_id,
                &storage.secret_access_key,
                &storage.bucket_name,
            )
            .await,
        ),
        Decision::Rollback(b) => Some(
            make_response(
                state,
                &b,
                "ROLLBACK",
                &params.bundle_id,
                storage.endpoint.as_deref(),
                &storage.access_key_id,
                &storage.secret_access_key,
                &storage.bucket_name,
            )
            .await,
        ),
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
            if let Some(ref target) = b.target_app_version {
                satisfies(&parsed_client_version, target)
            } else {
                false
            }
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
        Some(info) => Json(serde_json::to_value(&info).unwrap()).into_response(),
        None => {
            if supports_explicit_no_update_response(&headers) {
                Json(serde_json::json!({ "status": "UP_TO_DATE" })).into_response()
            } else {
                Json(serde_json::json!(null)).into_response()
            }
        }
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

    // Query candidates matching app, platform, channel, enabled, id >= min_bundle_id, AND exact fingerprint_hash
    let candidates_result = sqlx::query_as::<_, Bundle>(
        "SELECT * FROM bundles WHERE app_name = ? AND platform = ? AND channel = ? AND enabled = 1 AND id >= ? AND fingerprint_hash = ? ORDER BY id DESC"
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
        Some(info) => Json(serde_json::to_value(&info).unwrap()).into_response(),
        None => {
            if supports_explicit_no_update_response(&headers) {
                Json(serde_json::json!({ "status": "UP_TO_DATE" })).into_response()
            } else {
                Json(serde_json::json!(null)).into_response()
            }
        }
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
