use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::error;

use crate::cohort;
use crate::models::{Bundle, BundlePatch};
use crate::AppState;

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
    pub metadata: serde_json::Value,
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

fn parse_target_cohorts(val: &Option<serde_json::Value>) -> Option<Vec<String>> {
    val.as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
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

fn map_to_client_bundle(b: Bundle, patches: Vec<BundlePatch>) -> ClientBundle {
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
        metadata: b.metadata.unwrap_or_else(|| serde_json::json!({})),
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

// Authorization helper
fn authorize(
    headers: &HeaderMap,
    state: &AppState,
    app_name: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let app_config = state
        .config
        .get_app_config(app_name)
        .ok_or((StatusCode::NOT_FOUND, "Application not found"))?;

    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header"))?;

    let expected = format!("Bearer {}", app_config.auth_token);
    if auth_header == expected {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid authorization token"))
    }
}

// 1. GET /:app/hot-updater/api/bundles/channels
pub async fn list_channels(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return err.into_response();
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
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
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
        return err.into_response();
    }

    let bundle_result =
        sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE app_name = ? AND id = ?")
            .bind(&app_name)
            .bind(&bundle_id)
            .fetch_optional(&state.db)
            .await;

    match bundle_result {
        Ok(Some(b)) => {
            let patches_result = sqlx::query_as::<_, BundlePatch>(
                "SELECT * FROM bundle_patches WHERE bundle_id = ? ORDER BY order_index ASC",
            )
            .bind(&bundle_id)
            .fetch_all(&state.db)
            .await;

            let patches = patches_result.unwrap_or_default();
            Json(map_to_client_bundle(b, patches)).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "Bundle not found").into_response(),
        Err(err) => {
            error!("Failed to fetch bundle: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

// Query parameters for GET /api/bundles
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ListBundlesParams {
    pub channel: Option<String>,
    pub platform: Option<String>,
    pub enabled: Option<bool>,
    pub limit: Option<i64>,
    pub page: Option<i64>,
    pub target_app_version: Option<String>,
    pub target_app_version_in: Option<String>, // Comma-separated
    pub target_app_version_not_null: Option<bool>,
    pub fingerprint_hash: Option<String>,
    pub id_eq: Option<String>,
    pub id_gt: Option<String>,
    pub id_gte: Option<String>,
    pub id_lt: Option<String>,
    pub id_lte: Option<String>,
    pub id_in: Option<String>, // Comma-separated
    pub after: Option<String>,
    pub before: Option<String>,
}

fn apply_filters<'a>(qb: &mut sqlx::QueryBuilder<'a, sqlx::MySql>, params: &'a ListBundlesParams) {
    if let Some(chan) = &params.channel {
        qb.push(" AND channel = ");
        qb.push_bind(chan);
    }
    if let Some(plat) = &params.platform {
        qb.push(" AND platform = ");
        qb.push_bind(plat);
    }
    if let Some(enabled) = params.enabled {
        qb.push(" AND enabled = ");
        qb.push_bind(if enabled { 1 } else { 0 });
    }
    if let Some(fp) = &params.fingerprint_hash {
        qb.push(" AND fingerprint_hash = ");
        qb.push_bind(fp);
    }
    if let Some(target) = &params.target_app_version {
        if target == "null" {
            qb.push(" AND target_app_version IS NULL");
        } else {
            qb.push(" AND target_app_version = ");
            qb.push_bind(target);
        }
    }
    if let Some(target_in_str) = &params.target_app_version_in {
        let versions: Vec<&str> = target_in_str.split(',').collect();
        if !versions.is_empty() {
            qb.push(" AND target_app_version IN (");
            let mut separated = qb.separated(", ");
            for v in versions {
                separated.push_bind(v);
            }
            qb.push(")");
        }
    }
    if let Some(not_null) = params.target_app_version_not_null {
        if not_null {
            qb.push(" AND target_app_version IS NOT NULL");
        } else {
            qb.push(" AND target_app_version IS NULL");
        }
    }

    // ID Filters
    if let Some(eq) = &params.id_eq {
        qb.push(" AND id = ");
        qb.push_bind(eq);
    }
    if let Some(gt) = &params.id_gt {
        qb.push(" AND id > ");
        qb.push_bind(gt);
    }
    if let Some(gte) = &params.id_gte {
        qb.push(" AND id >= ");
        qb.push_bind(gte);
    }
    if let Some(lt) = &params.id_lt {
        qb.push(" AND id < ");
        qb.push_bind(lt);
    }
    if let Some(lte) = &params.id_lte {
        qb.push(" AND id <= ");
        qb.push_bind(lte);
    }
    if let Some(id_in_str) = &params.id_in {
        let ids: Vec<&str> = id_in_str.split(',').collect();
        if !ids.is_empty() {
            qb.push(" AND id IN (");
            let mut separated = qb.separated(", ");
            for id in ids {
                separated.push_bind(id);
            }
            qb.push(")");
        }
    }

    // Cursor filters (DESC order assumed)
    if let Some(after) = &params.after {
        qb.push(" AND id < ");
        qb.push_bind(after);
    }
    if let Some(before) = &params.before {
        qb.push(" AND id > ");
        qb.push_bind(before);
    }
}

// 3. GET /:app/hot-updater/api/bundles
pub async fn list_bundles(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
    Query(params): Query<ListBundlesParams>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return err.into_response();
    }

    let limit = params.limit.unwrap_or(50).min(100);
    let page = params.page.unwrap_or(1).max(1);

    // Build dynamic where clause using SQLx QueryBuilder
    let mut query_builder = sqlx::QueryBuilder::new("SELECT * FROM bundles WHERE app_name = ");
    query_builder.push_bind(&app_name);

    let mut count_builder =
        sqlx::QueryBuilder::new("SELECT COUNT(*) FROM bundles WHERE app_name = ");
    count_builder.push_bind(&app_name);

    apply_filters(&mut query_builder, &params);
    apply_filters(&mut count_builder, &params);

    // Fetch total count
    let count_query = count_builder.build_query_scalar::<i64>();
    let total = count_query.fetch_one(&state.db).await.unwrap_or(0);

    // Pagination calculations
    let total_pages = ((total as f64) / (limit as f64)).ceil() as i64;
    let current_page = page;
    let offset = (page - 1) * limit;

    query_builder.push(" ORDER BY id DESC LIMIT ");
    query_builder.push_bind(limit);

    if params.after.is_none() && params.before.is_none() {
        query_builder.push(" OFFSET ");
        query_builder.push_bind(offset);
    }

    let query = query_builder.build_query_as::<Bundle>();
    let bundles_result = query.fetch_all(&state.db).await;

    let bundles = match bundles_result {
        Ok(b) => b,
        Err(err) => {
            error!("Failed to fetch bundles list: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    // Load patches for all returned bundles
    let mut client_bundles = Vec::new();
    if !bundles.is_empty() {
        let bundle_ids: Vec<String> = bundles.iter().map(|b| b.id.clone()).collect();
        let mut patches_builder =
            sqlx::QueryBuilder::new("SELECT * FROM bundle_patches WHERE bundle_id IN (");
        let mut separated = patches_builder.separated(", ");
        for id in &bundle_ids {
            separated.push_bind(id);
        }
        patches_builder.push(") ORDER BY order_index ASC");

        let patches_query = patches_builder.build_query_as::<BundlePatch>();
        let patches = patches_query.fetch_all(&state.db).await.unwrap_or_default();

        for b in bundles {
            let b_patches: Vec<BundlePatch> = patches
                .iter()
                .filter(|p| p.bundle_id == b.id)
                .cloned()
                .collect();
            client_bundles.push(map_to_client_bundle(b, b_patches));
        }
    }

    let has_next_page = offset + limit < total;
    let has_previous_page = offset > 0;
    let next_cursor = if has_next_page {
        client_bundles.last().map(|cb| cb.id.clone())
    } else {
        None
    };
    let previous_cursor = if has_previous_page {
        client_bundles.first().map(|cb| cb.id.clone())
    } else {
        None
    };

    Json(serde_json::json!({
        "data": client_bundles,
        "pagination": {
            "total": total,
            "hasNextPage": has_next_page,
            "hasPreviousPage": has_previous_page,
            "currentPage": current_page,
            "totalPages": total_pages,
            "nextCursor": next_cursor,
            "previousCursor": previous_cursor
        }
    }))
    .into_response()
}

// 4. POST /:app/hot-updater/api/bundles
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CLIPatch {
    pub base_bundle_id: String,
    pub base_file_hash: String,
    pub patch_file_hash: String,
    pub patch_storage_uri: String,
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
    pub patches: Option<Vec<CLIPatch>>,
}

pub async fn create_bundles(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(app_name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return err.into_response();
    }

    // Parse payload as either a single bundle object or an array of bundles
    let cli_bundles: Vec<CLIBundle> = if body.is_array() {
        match serde_json::from_value(body) {
            Ok(list) => list,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "Invalid bundles array payload").into_response()
            }
        }
    } else {
        match serde_json::from_value(body) {
            Ok(single) => vec![single],
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "Invalid bundle object payload").into_response()
            }
        }
    };

    // Run transaction
    let mut tx = match state.db.begin().await {
        Ok(t) => t,
        Err(err) => {
            error!("Failed to begin transaction: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
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

        if let Err(message) = assert_bundle_persistence_constraints(
            &cb.target_app_version,
            &cb.fingerprint_hash,
            rollout_cohort_count,
            &cb.target_cohorts,
        ) {
            return (StatusCode::BAD_REQUEST, message).into_response();
        }

        let target_cohorts_json = cb
            .target_cohorts
            .map(|tc| serde_json::to_value(tc).unwrap());
        let metadata_json = cb.metadata.unwrap_or_else(|| serde_json::json!({}));

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
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database write error").into_response();
        }

        // Delete previous patches for the bundle
        let delete_patches_result = sqlx::query("DELETE FROM bundle_patches WHERE bundle_id = ?")
            .bind(&cb.id)
            .execute(&mut *tx)
            .await;

        if let Err(err) = delete_patches_result {
            error!("Failed to clear patches for bundle {}: {}", cb.id, err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database write error").into_response();
        }

        // Insert new patches
        if let Some(patches) = cb.patches {
            for (index, p) in patches.into_iter().enumerate() {
                let patch_id = format!("{}:{}", cb.id, p.base_bundle_id);
                let order_index = index as i32;

                let insert_patch_result = sqlx::query(
                    r#"
                    INSERT INTO bundle_patches (
                        id, bundle_id, base_bundle_id, base_file_hash, patch_file_hash, patch_storage_uri, order_index
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)
                    "#
                )
                .bind(&patch_id)
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
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Database write error")
                        .into_response();
                }
            }
        }
    }

    if let Err(err) = tx.commit().await {
        error!("Failed to commit bundle save transaction: {}", err);
        return (StatusCode::INTERNAL_SERVER_ERROR, "Database commit error").into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "success": true })),
    )
        .into_response()
}

// 5. PATCH /:app/hot-updater/api/bundles/:id
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBundlePayload {
    pub id: Option<String>,
    pub platform: Option<String>,
    pub should_force_update: Option<bool>,
    pub enabled: Option<bool>,
    pub file_hash: Option<String>,
    pub git_commit_hash: Option<String>,
    pub message: Option<String>,
    pub channel: Option<String>,
    pub storage_uri: Option<String>,
    pub target_app_version: Option<String>,
    pub fingerprint_hash: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub rollout_cohort_count: Option<i32>,
    pub target_cohorts: Option<Vec<String>>,
    pub manifest_storage_uri: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub asset_base_storage_uri: Option<String>,
}

pub async fn update_bundle(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, bundle_id)): Path<(String, String)>,
    Json(payload): Json<UpdateBundlePayload>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return err.into_response();
    }

    if let Some(ref id) = payload.id {
        if id != &bundle_id {
            return (StatusCode::BAD_REQUEST, "Bundle id mismatch").into_response();
        }
    }

    let current =
        match sqlx::query_as::<_, Bundle>("SELECT * FROM bundles WHERE app_name = ? AND id = ?")
            .bind(&app_name)
            .bind(&bundle_id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => return (StatusCode::NOT_FOUND, "Bundle not found").into_response(),
            Err(err) => {
                error!("Failed to fetch bundle {} for update: {}", bundle_id, err);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
            }
        };

    // Validate constraints against the merged row that results after the patch is applied
    // (reference: HotUpdaterApi.updateBundleById -> assertBundlePersistenceConstraints({ ...current, ...newBundle })).
    // NOTE: an omitted field cannot be distinguished from an explicitly `null` field with a
    // single Option; both are treated as "leave unchanged" (the current value is kept).
    let merged_target_app_version = payload
        .target_app_version
        .clone()
        .or_else(|| current.target_app_version.clone());
    let merged_fingerprint_hash = payload
        .fingerprint_hash
        .clone()
        .or_else(|| current.fingerprint_hash.clone());
    let merged_rollout_cohort_count = payload
        .rollout_cohort_count
        .unwrap_or(current.rollout_cohort_count);
    let merged_target_cohorts = match &payload.target_cohorts {
        Some(tc) => Some(tc.clone()),
        None => parse_target_cohorts(&current.target_cohorts),
    };

    if let Err(message) = assert_bundle_persistence_constraints(
        &merged_target_app_version,
        &merged_fingerprint_hash,
        merged_rollout_cohort_count,
        &merged_target_cohorts,
    ) {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }

    // Build dynamic update query (only the submitted fields are SET)
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

    if let Some(v) = payload.platform {
        set_field!("platform", v);
    }
    if let Some(v) = payload.should_force_update {
        set_field!("should_force_update", if v { 1i8 } else { 0i8 });
    }
    if let Some(v) = payload.enabled {
        set_field!("enabled", if v { 1i8 } else { 0i8 });
    }
    if let Some(v) = payload.file_hash {
        set_field!("file_hash", v);
    }
    if let Some(v) = payload.git_commit_hash {
        set_field!("git_commit_hash", v);
    }
    if let Some(v) = payload.message {
        set_field!("message", v);
    }
    if let Some(v) = payload.channel {
        set_field!("channel", v);
    }
    if let Some(v) = payload.storage_uri {
        set_field!("storage_uri", v);
    }
    if let Some(v) = payload.target_app_version {
        set_field!("target_app_version", v);
    }
    if let Some(v) = payload.fingerprint_hash {
        set_field!("fingerprint_hash", v);
    }
    if let Some(v) = payload.metadata {
        set_field!("metadata", v);
    }
    if let Some(v) = payload.rollout_cohort_count {
        set_field!("rollout_cohort_count", v);
    }
    if let Some(v) = payload.target_cohorts {
        set_field!("target_cohorts", serde_json::to_value(v).unwrap());
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

    if !has_update {
        return Json(serde_json::json!({ "success": true })).into_response();
    }

    qb.push(" WHERE app_name = ");
    qb.push_bind(app_name);
    qb.push(" AND id = ");
    qb.push_bind(bundle_id.clone());

    let result = qb.build().execute(&state.db).await;

    match result {
        Ok(_) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(err) => {
            error!("Failed to update bundle {}: {}", bundle_id, err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

// 6. DELETE /:app/hot-updater/api/bundles/:id
pub async fn delete_bundle(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((app_name, bundle_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(err) = authorize(&headers, &state, &app_name) {
        return err.into_response();
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
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
