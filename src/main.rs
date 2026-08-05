use axum::{response::IntoResponse, routing::get, Json, Router};
use rn_ota_server_rust::{config, db, AppState};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Load environment from .env file
    if let Err(e) = dotenvy::dotenv() {
        info!("dotenv: {}", e);
    }

    // Load configurations
    let config = match config::Config::from_env() {
        Ok(cfg) => Arc::new(cfg),
        Err(err) => {
            error!("Configuration loading failed: {}", err);
            std::process::exit(1);
        }
    };

    // Initialize database pool and run migrations
    let pool = match db::init_db(&config.database_url).await {
        Ok(p) => p,
        Err(err) => {
            error!("Database initialization failed: {}", err);
            std::process::exit(1);
        }
    };

    let state = AppState {
        config: config.clone(),
        db: pool,
    };

    // Build the Axum router
    let app = Router::new()
        .route("/version", get(get_version))
        .merge(rn_ota_server_rust::routes::configure_routes(state));

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("Invalid HOST:PORT combination");
    info!("Starting rn-ota-server-rust on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn get_version() -> impl IntoResponse {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION")
    }))
}
