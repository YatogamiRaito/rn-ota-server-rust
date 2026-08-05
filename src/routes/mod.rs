use crate::AppState;
use axum::routing::get;
use axum::Router;

pub mod api;
pub mod check;

pub fn configure_routes(state: AppState) -> Router {
    Router::new()
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
        )
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
            get(api::list_bundles)
                .post(api::create_bundles),
        )
        .with_state(state)
}
