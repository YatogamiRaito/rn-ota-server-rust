use axum::{body::Body, http::Request, response::IntoResponse, routing::get, Json, Router};
use rn_ota_server_rust::config::{DbPoolConfig, ObservabilityConfig, RateLimitConfig};
use rn_ota_server_rust::observability;
use rn_ota_server_rust::{config, db, AppState};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::{error, info, warn, Level};

const HELP: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    env!("CARGO_PKG_DESCRIPTION"),
    "\n\n",
    "USAGE:\n",
    "    ",
    env!("CARGO_PKG_NAME"),
    " [OPTIONS]\n\n",
    "OPTIONS:\n",
    "    -h, --help       Print this help and exit\n",
    "    -V, --version    Print the version and exit\n\n",
    "The server takes no other arguments: it is configured entirely through environment\n",
    "variables, and loads a .env file from the working directory if one is present.\n",
    "See ",
    env!("CARGO_PKG_REPOSITORY"),
    "#configuration\n"
);

#[tokio::main]
async fn main() {
    // Configuration is env-only; the two flags below are the conventions someone
    // running a downloaded binary will reach for first.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("Unknown argument: {other}\n");
                print!("{HELP}");
                std::process::exit(2);
            }
        }
    }

    // Load the environment before the subscriber is built, so RUST_LOG can be set in
    // .env like every other setting.
    let dotenv_result = dotenvy::dotenv();

    observability::init_tracing();

    if let Err(e) = dotenv_result {
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
    let pool_config = exit_on_config_error(DbPoolConfig::from_env());
    let obs_config = exit_on_config_error(ObservabilityConfig::from_env());
    let rate_limit = exit_on_config_error(RateLimitConfig::from_env());

    // Keeps the `app` metric label bounded to the configured apps.
    observability::init_known_apps(config.apps.keys().cloned());

    let metrics_handle = if obs_config.metrics_enabled {
        match observability::init_metrics() {
            Ok(handle) => Some(handle),
            Err(err) => {
                error!("Metrics initialization failed: {}", err);
                std::process::exit(1);
            }
        }
    } else {
        info!("Metrics disabled (METRICS_ENABLED=false); /metrics is not served.");
        None
    };

    let cors = match observability::cors_layer(&obs_config.cors_allowed_origins) {
        Ok(layer) => layer,
        Err(err) => {
            error!("CORS configuration failed: {}", err);
            std::process::exit(1);
        }
    };

    // Initialize database pool and run migrations
    let pool = match db::init_db_with_options(&config.database_url, &pool_config).await {
        Ok(p) => p,
        Err(err) => {
            error!("Database initialization failed: {}", err);
            std::process::exit(1);
        }
    };

    if rate_limit.enabled {
        warn!(
            per_second = rate_limit.per_second,
            burst = rate_limit.burst,
            trust_proxy_headers = rate_limit.trust_proxy_headers,
            "Rate limiting is ENABLED on the update-check routes; clients over the limit get 429."
        );
    }

    let state = AppState {
        config: config.clone(),
        db: pool.clone(),
    };

    // Build the Axum router
    let mut app = Router::new()
        .route("/version", get(get_version))
        .route("/health", get(get_health))
        .merge(rn_ota_server_rust::routes::configure_routes_with_rate_limit(state, &rate_limit));

    if let Some(handle) = metrics_handle {
        app = app.merge(observability::metrics_router(handle, pool));
    }

    // Applied innermost first: metrics see the matched route, the trace span sees the
    // request id set by the outermost layer. None of these layers alter the response a
    // hot-updater client observes on the happy path.
    let mut app = app.layer(axum::middleware::from_fn(observability::track_http_metrics));

    if let Some(level) = observability::access_log_level(obs_config.http_log_level) {
        app = app.layer(
            TraceLayer::new_for_http()
                .make_span_with(move |request: &Request<Body>| {
                    observability::make_http_span(level, request)
                })
                // The span already carries method/route/request_id; a second event on
                // the way in would only double the volume on the hot path.
                .on_request(DefaultOnRequest::new().level(Level::TRACE))
                .on_response(
                    DefaultOnResponse::new()
                        .level(level)
                        .latency_unit(LatencyUnit::Millis),
                )
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        );
    }

    if let Some(cors) = cors {
        app = app.layer(cors);
    }

    let app = app
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("Invalid HOST:PORT combination");
    info!("Starting rn-ota-server-rust on {}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(err) => {
            error!("Failed to bind {}: {}", addr, err);
            std::process::exit(1);
        }
    };

    // `into_make_service_with_connect_info` makes the peer address available to the
    // rate limiter's key extractor; it changes nothing for the handlers themselves.
    if let Err(err) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    {
        error!("Server error: {}", err);
        std::process::exit(1);
    }

    info!("Shutdown complete.");
}

/// Configuration problems are fatal and must name the variable at fault — the operator
/// is usually staring at a container that will not start.
fn exit_on_config_error<T>(result: Result<T, String>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => {
            error!("Configuration loading failed: {}", err);
            std::process::exit(1);
        }
    }
}

async fn get_version() -> impl IntoResponse {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Liveness probe for containers, load balancers and PM2. Deliberately cheap —
/// it does not touch the database, so a slow DB cannot cause a restart loop.
async fn get_health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM — the signal `docker stop`
/// and most orchestrators send.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                error!("Failed to install SIGTERM handler: {}", err);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT, shutting down gracefully..."),
        _ = terminate => info!("Received SIGTERM, shutting down gracefully..."),
    }
}
