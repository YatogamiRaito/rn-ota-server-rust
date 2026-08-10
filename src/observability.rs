//! Logging, tracing, metrics and CORS wiring.
//!
//! Everything here is additive: it observes traffic without changing any route path,
//! response body or status code that a hot-updater SDK or CLI client sees.

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use sqlx::MySqlPool;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{Level, Span};
use tracing_subscriber::EnvFilter;

use crate::config::HttpLogLevel;

/// Filter used when `RUST_LOG` is unset. `info` for this server, but the chattier
/// dependencies are pinned to `warn` so the default does not turn into a per-request
/// (or per-query) firehose.
pub const DEFAULT_LOG_FILTER: &str = "info,sqlx=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn,aws_config=warn,aws_sdk_s3=warn,aws_smithy_runtime=warn,aws_smithy_runtime_api=warn";

/// Header carrying the per-request correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

// ---------------------------------------------------------------------------
// Metric names
// ---------------------------------------------------------------------------

/// Counter: HTTP requests served, labelled `route`, `method`, `status`.
pub const METRIC_HTTP_REQUESTS: &str = "ota_http_requests_total";
/// Histogram: request latency in seconds, labelled `route`, `method`, `status`.
pub const METRIC_HTTP_DURATION: &str = "ota_http_request_duration_seconds";
/// Counter: update-check decisions, labelled `app`, `platform`, `outcome`.
pub const METRIC_UPDATE_CHECKS: &str = "ota_update_checks_total";
/// Gauge: connections currently held by the MySQL pool.
pub const METRIC_DB_POOL_CONNECTIONS: &str = "ota_db_pool_connections";
/// Gauge: idle connections in the MySQL pool.
pub const METRIC_DB_POOL_IDLE_CONNECTIONS: &str = "ota_db_pool_idle_connections";

/// Latency buckets tuned for an update-check endpoint: most of the mass is expected
/// between 1 ms and 250 ms, with a long tail when S3 presigning is involved.
const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Install the global tracing subscriber. Honours `RUST_LOG`, falling back to
/// [`DEFAULT_LOG_FILTER`]. Call once, after `.env` has been loaded so `RUST_LOG` can
/// be set there too.
pub fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Build the per-request span. `tracing` requires a static callsite, so the level has
/// to be branched on rather than passed through.
///
/// Kept deliberately small — three borrowed fields, no allocation on the hot path
/// beyond what the subscriber itself does.
pub fn make_http_span(level: Level, request: &Request<Body>) -> Span {
    let method = request.method().as_str();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or_else(|| request.uri().path());
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    macro_rules! span_at {
        ($lvl:expr) => {
            tracing::span!($lvl, "http", method, route, request_id)
        };
    }

    match level {
        Level::ERROR => span_at!(Level::ERROR),
        Level::WARN => span_at!(Level::WARN),
        Level::INFO => span_at!(Level::INFO),
        Level::DEBUG => span_at!(Level::DEBUG),
        Level::TRACE => span_at!(Level::TRACE),
    }
}

/// The level the access log events should be emitted at, or `None` when logging is off.
pub fn access_log_level(cfg: HttpLogLevel) -> Option<Level> {
    match cfg {
        HttpLogLevel::Off => None,
        HttpLogLevel::On(level) => Some(level),
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Install the Prometheus recorder and return the handle used to render `/metrics`.
///
/// Deliberately does not start the exporter's own HTTP listener: the text is rendered
/// from an Axum route on the server's single port instead.
pub fn init_metrics() -> Result<PrometheusHandle, String> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(METRIC_HTTP_DURATION.to_string()),
            LATENCY_BUCKETS,
        )
        .map_err(|err| format!("Invalid latency buckets: {err}"))?
        .install_recorder()
        .map_err(|err| format!("Failed to install the Prometheus recorder: {err}"))
}

/// The set of configured app names, used to keep the `app` metric label bounded.
static KNOWN_APPS: OnceLock<HashSet<String>> = OnceLock::new();

/// Register the configured app names. Anything else seen in a request path is folded
/// into the `unknown` label so a scanner hitting `/{random}/hot-updater/...` cannot
/// blow up the metric cardinality.
pub fn init_known_apps<I: IntoIterator<Item = String>>(apps: I) {
    let _ = KNOWN_APPS.set(apps.into_iter().collect());
}

/// The outcome of an update check, as reported to `/metrics`.
///
/// Mirrors the three states a hot-updater client can observe; `InitRollback` is folded
/// into `Rollback` because the client acts on both the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Update,
    Rollback,
    UpToDate,
}

impl UpdateOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateOutcome::Update => "UPDATE",
            UpdateOutcome::Rollback => "ROLLBACK",
            UpdateOutcome::UpToDate => "UP_TO_DATE",
        }
    }
}

/// Record one update-check decision.
///
/// This is the hook the check handlers should call once they know what they are
/// returning. Labels are bounded on purpose: `app` is folded to `unknown` unless it is
/// a configured app, `platform` to `other` unless it is `ios`/`android`. Never pass a
/// bundle id, app version, fingerprint or cohort here — they are unbounded.
pub fn record_update_check(app: &str, platform: &str, outcome: UpdateOutcome) {
    counter!(
        METRIC_UPDATE_CHECKS,
        "app" => app_label(app),
        "platform" => platform_label(platform),
        "outcome" => outcome.as_str(),
    )
    .increment(1);
}

fn app_label(app: &str) -> String {
    match KNOWN_APPS.get() {
        // Before the app list is registered (unit tests, tools) there is nothing to
        // validate against; metrics are a no-op there anyway.
        None => app.to_string(),
        Some(known) if known.contains(app) => app.to_string(),
        Some(_) => "unknown".to_string(),
    }
}

fn platform_label(platform: &str) -> &'static str {
    match platform.to_ascii_lowercase().as_str() {
        "ios" => "ios",
        "android" => "android",
        _ => "other",
    }
}

/// Collapse a matched route into a small, fixed set of classes. The matched path is
/// already bounded by the route table, but the class keeps the label short and stable
/// across route refactors — and unmatched requests (404s) all share one bucket.
fn route_class(matched: Option<&str>) -> &'static str {
    let Some(path) = matched else {
        return "unmatched";
    };
    if path.contains("/hot-updater/app-version/") {
        "update_check_app_version"
    } else if path.contains("/hot-updater/fingerprint/") {
        "update_check_fingerprint"
    } else if path.ends_with("/api/bundles/channels") {
        "api_bundle_channels"
    } else if path.ends_with("/api/bundles/{id}") {
        "api_bundle_item"
    } else if path.ends_with("/api/bundles") {
        "api_bundle_collection"
    } else if path == "/health" {
        "health"
    } else if path == "/version" {
        "version"
    } else if path == "/metrics" {
        "metrics"
    } else {
        "other"
    }
}

/// HTTP methods are an open set at the protocol level, so anything outside the standard
/// list is folded into `OTHER` to keep the label bounded.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

/// Axum middleware recording request counts and latency per route class.
pub async fn track_http_metrics(request: Request<Body>, next: axum::middleware::Next) -> Response {
    let route = route_class(
        request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    let method = method_label(request.method());
    let start = Instant::now();

    let response = next.run(request).await;

    let status = status_label(response.status());
    let elapsed = start.elapsed().as_secs_f64();
    counter!(METRIC_HTTP_REQUESTS, "route" => route, "method" => method, "status" => status)
        .increment(1);
    histogram!(METRIC_HTTP_DURATION, "route" => route, "method" => method, "status" => status)
        .record(elapsed);

    response
}

/// Status codes are bounded in practice, but `as_u16().to_string()` allocates on every
/// request; the codes this server actually returns are matched to static strings.
fn status_label(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200 => "200",
        204 => "204",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        409 => "409",
        429 => "429",
        500 => "500",
        502 => "502",
        503 => "503",
        code if code < 200 => "1xx",
        code if code < 300 => "2xx",
        code if code < 400 => "3xx",
        code if code < 500 => "4xx",
        _ => "5xx",
    }
}

/// `GET /metrics`, rendering the Prometheus text exposition format in-process.
///
/// The route is unauthenticated like `/health` and `/version`; restrict it at the
/// reverse proxy, or set `METRICS_ENABLED=false`, if that is not acceptable.
pub fn metrics_router(handle: PrometheusHandle, pool: MySqlPool) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let handle = handle.clone();
            let pool = pool.clone();
            async move { render_metrics(&handle, &pool) }
        }),
    )
}

fn render_metrics(handle: &PrometheusHandle, pool: &MySqlPool) -> Response {
    // Both are plain atomic loads on the pool's shared state, so sampling them per
    // scrape is cheaper than keeping a background task alive.
    gauge!(METRIC_DB_POOL_CONNECTIONS).set(f64::from(pool.size()));
    gauge!(METRIC_DB_POOL_IDLE_CONNECTIONS).set(pool.num_idle() as f64);

    // Drops metrics that have been idle past their timeout, keeping the registry from
    // growing without bound over the process lifetime.
    handle.run_upkeep();

    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        handle.render(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// Build the CORS layer from the configured origin list.
///
/// Returns `None` when no origins are configured, which is the default — see
/// [`crate::config::ObservabilityConfig`] for why sending no CORS headers at all is the
/// right default for this server.
pub fn cors_layer(allowed_origins: &[String]) -> Result<Option<CorsLayer>, String> {
    if allowed_origins.is_empty() {
        return Ok(None);
    }

    let origins = if allowed_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        let parsed = allowed_origins
            .iter()
            .map(|origin| {
                HeaderValue::from_str(origin).map_err(|_| {
                    format!("Invalid CORS_ALLOWED_ORIGINS entry '{origin}': not a valid origin.")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        AllowOrigin::list(parsed)
    };

    // Credentials are never allowed: this server authenticates the CLI with a bearer
    // token, so there is no cookie or basic-auth flow a browser could be tricked into
    // replaying, and `Allow-Credentials` cannot be combined with `*` anyway.
    Ok(Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
            ]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_class_is_bounded_and_stable() {
        assert_eq!(
            route_class(Some(
                "/{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{minBundleId}/{bundleId}"
            )),
            "update_check_app_version"
        );
        assert_eq!(
            route_class(Some(
                "/{app}/hot-updater/fingerprint/{platform}/{fingerprintHash}/{channel}/{minBundleId}/{bundleId}/{cohort}"
            )),
            "update_check_fingerprint"
        );
        assert_eq!(
            route_class(Some("/{app}/hot-updater/api/bundles/channels")),
            "api_bundle_channels"
        );
        assert_eq!(
            route_class(Some("/{app}/hot-updater/api/bundles/{id}")),
            "api_bundle_item"
        );
        assert_eq!(
            route_class(Some("/{app}/hot-updater/api/bundles")),
            "api_bundle_collection"
        );
        assert_eq!(route_class(Some("/health")), "health");
        assert_eq!(route_class(None), "unmatched");
    }

    #[test]
    fn platform_label_folds_unknown_values() {
        assert_eq!(platform_label("ios"), "ios");
        assert_eq!(platform_label("iOS"), "ios");
        assert_eq!(platform_label("android"), "android");
        assert_eq!(platform_label("../../etc/passwd"), "other");
    }

    #[test]
    fn method_label_folds_non_standard_methods() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(
            method_label(&Method::from_bytes(b"WEIRD").unwrap()),
            "OTHER"
        );
    }

    /// The recorder is process-global, so exactly one test may install it.
    #[tokio::test]
    async fn metrics_route_renders_the_prometheus_text_format() {
        use tower::ServiceExt;

        let handle = init_metrics().expect("recorder installs");
        record_update_check("some-app", "ios", UpdateOutcome::Update);

        let pool = MySqlPool::connect_lazy("mysql://localhost/test").expect("lazy pool");
        let response = metrics_router(handle, pool)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/plain; version=0.0.4; charset=utf-8"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(METRIC_UPDATE_CHECKS), "{body}");
        assert!(body.contains("outcome=\"UPDATE\""), "{body}");
        assert!(body.contains(METRIC_DB_POOL_CONNECTIONS), "{body}");
    }

    #[test]
    fn cors_is_disabled_when_no_origins_are_configured() {
        assert!(cors_layer(&[]).unwrap().is_none());
        assert!(cors_layer(&["https://ota.example.com".to_string()])
            .unwrap()
            .is_some());
        assert!(cors_layer(&["not a header value\n".to_string()]).is_err());
    }
}
