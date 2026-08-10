use crate::config::RateLimitConfig;
use crate::AppState;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::Router;
use std::time::Duration;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::{PeerIpKeyExtractor, SmartIpKeyExtractor};
use tower_governor::GovernorLayer;

pub mod api;
pub mod check;

/// Wire every application route. Rate limiting is off, which is also the default the
/// server runs with; use [`configure_routes_with_rate_limit`] to opt in.
pub fn configure_routes(state: AppState) -> Router {
    configure_routes_with_rate_limit(state, &RateLimitConfig::disabled())
}

/// Same as [`configure_routes`], but applies the optional per-client rate limit to the
/// unauthenticated update-check routes. The authenticated CLI API is never throttled
/// here — a deploy pushing many bundles must not be mistaken for abuse.
pub fn configure_routes_with_rate_limit(state: AppState, rate_limit: &RateLimitConfig) -> Router {
    let check_routes = Router::new()
        // App Version Strategy Routes
        .route(
            "/{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{minBundleId}/{bundleId}",
            get(check::check_app_version_no_cohort),
        )
        .route(
            "/{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{minBundleId}/{bundleId}/{cohort}",
            get(check::check_app_version_with_cohort),
        )
        // Fingerprint Strategy Routes
        .route(
            "/{app}/hot-updater/fingerprint/{platform}/{fingerprintHash}/{channel}/{minBundleId}/{bundleId}",
            get(check::check_fingerprint_no_cohort),
        )
        .route(
            "/{app}/hot-updater/fingerprint/{platform}/{fingerprintHash}/{channel}/{minBundleId}/{bundleId}/{cohort}",
            get(check::check_fingerprint_with_cohort),
        );

    let check_routes = apply_rate_limit(check_routes, rate_limit);

    let api_routes = Router::new()
        // CLI API Routes (authorized)
        .route(
            "/{app}/hot-updater/api/bundles/channels",
            get(api::list_channels),
        )
        .route(
            "/{app}/hot-updater/api/bundles/{id}",
            get(api::get_bundle)
                .patch(api::update_bundle)
                .delete(api::delete_bundle),
        )
        .route(
            "/{app}/hot-updater/api/bundles",
            get(api::list_bundles).post(api::create_bundles),
        )
        // Made explicit rather than inherited: `POST /bundles` is the only route that
        // takes a body, and until now it relied on Axum's implicit default. The value is
        // deliberately identical to that default, so no CLI payload that worked before
        // starts failing. The update-check routes are not covered — they have no body.
        .layer(DefaultBodyLimit::max(MAX_CLI_API_BODY_BYTES));

    check_routes.merge(api_routes).with_state(state)
}

/// The two key extractors are different types, so the layer has to be applied inside
/// each branch rather than built once and returned.
fn apply_rate_limit(router: Router<AppState>, rate_limit: &RateLimitConfig) -> Router<AppState> {
    if !rate_limit.enabled {
        return router;
    }

    // `per_second` requests sustained -> one cell replenished every 1/per_second second.
    let period = Duration::from_nanos(1_000_000_000 / u64::from(rate_limit.per_second.max(1)));
    let mut builder = GovernorConfigBuilder::default();
    builder.period(period).burst_size(rate_limit.burst);

    if rate_limit.trust_proxy_headers {
        let mut builder = builder.key_extractor(SmartIpKeyExtractor);
        match builder.finish() {
            Some(config) => {
                // The keyed limiter keeps one entry per client IP until told to forget
                // the ones that have fully replenished.
                let limiter = config.limiter().clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(LIMITER_GC_INTERVAL);
                    loop {
                        ticker.tick().await;
                        limiter.retain_recent();
                    }
                });
                router.layer(GovernorLayer::<SmartIpKeyExtractor, _, Body>::new(config))
            }
            None => router,
        }
    } else {
        match builder.finish() {
            Some(config) => {
                let limiter = config.limiter().clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(LIMITER_GC_INTERVAL);
                    loop {
                        ticker.tick().await;
                        limiter.retain_recent();
                    }
                });
                router.layer(GovernorLayer::<PeerIpKeyExtractor, _, Body>::new(config))
            }
            None => router,
        }
    }
}

/// Request body ceiling for the authenticated CLI API. 2 MiB, i.e. exactly Axum's own
/// default — spelled out here so a future Axum change cannot silently move it.
const MAX_CLI_API_BODY_BYTES: usize = 2 * 1024 * 1024;

/// How often the rate limiter forgets client keys that have fully replenished.
const LIMITER_GC_INTERVAL: Duration = Duration::from_secs(60);
