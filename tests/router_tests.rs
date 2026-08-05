use rn_ota_server_rust::{
    config::{AppConfig, AppStorageConfig, Config},
    routes::configure_routes,
    AppState,
};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn test_router_initialization_does_not_panic() {
    let app_config = AppConfig {
        name: "test-app".to_string(),
        auth_token: "test-token".to_string(),
        storage: AppStorageConfig {
            endpoint: Some("test-endpoint".to_string()),
            access_key_id: "test-key".to_string(),
            secret_access_key: "test-secret".to_string(),
            bucket_name: "test-bucket".to_string(),
        },
    };

    let config = Arc::new(Config {
        database_url: "mysql://localhost/test".to_string(),
        host: "127.0.0.1".to_string(),
        port: 3000,
        apps: HashMap::from([("test-app".to_string(), app_config)]),
    });

    // Use a lazy pool so we don't attempt to connect to a real MySQL instance during the unit test.
    let db_pool =
        MySqlPool::connect_lazy("mysql://localhost/test").expect("Failed to create lazy pool");

    let state = AppState {
        config,
        db: db_pool,
    };

    // This will panic at runtime if there are any invalid route patterns under Axum 0.8.
    let _app = configure_routes(state);
}
