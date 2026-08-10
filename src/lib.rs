pub mod cohort;
pub mod config;
pub mod db;
pub mod models;
pub mod observability;
pub mod routes;
pub mod semver;
pub mod storage;

#[derive(Clone)]
pub struct AppState {
    pub config: std::sync::Arc<config::Config>,
    pub db: sqlx::MySqlPool,
}
