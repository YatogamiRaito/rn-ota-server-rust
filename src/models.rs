use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct Bundle {
    pub id: String,
    pub app_name: String,
    pub platform: String,
    pub should_force_update: i8, // mysql TINYINT(1) / BOOLEAN maps to i8 or bool. sqlx mysql maps boolean to bool or i8 depending on drivers. It is safer to use i8 or bool. Let's see: sqlx mysql BOOLEAN is alias for TINYINT, which can be read as i8 or bool. Usually bool is supported. Let's use bool and if there is any mapping issue we can adjust. Actually, standard sqlx mysql maps tinyint(1) to bool.
    pub enabled: i8, // same, maps to i8 or bool. Let's use i8 to be safe against mysql boolean driver variations in sqlx! i8 represents 0 or 1.
    pub file_hash: String,
    pub git_commit_hash: Option<String>,
    pub message: Option<String>,
    pub channel: String,
    pub storage_uri: String,
    pub target_app_version: Option<String>,
    pub fingerprint_hash: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub rollout_cohort_count: i32,
    pub target_cohorts: Option<serde_json::Value>,
    pub manifest_storage_uri: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub asset_base_storage_uri: Option<String>,
    pub ctime: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow, Serialize, Deserialize, Clone, Debug)]
pub struct BundlePatch {
    pub id: String,
    pub bundle_id: String,
    pub base_bundle_id: String,
    pub base_file_hash: String,
    pub patch_file_hash: String,
    pub patch_storage_uri: String,
    pub order_index: i32,
    pub ctime: chrono::DateTime<chrono::Utc>,
}
