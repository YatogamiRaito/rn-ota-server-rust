//! End-to-end tests for the device-facing update-check endpoints
//! (`/{app}/hot-updater/app-version/...` and `/{app}/hot-updater/fingerprint/...`)
//! against a real MySQL 8 database.
//!
//! `tests/decision_tests.rs` already covers `decide_update` as a pure function against
//! fixtures generated from `@hot-updater/js`. What is exercised here is everything around
//! it that the fixtures cannot reach: the SQL candidate queries (app/platform/channel/
//! enabled/`min_bundle_id` filtering), the response serialisation, the status codes, and
//! the fact that these routes are unauthenticated by design.
//!
//! Bundles are seeded without a `manifest_storage_uri`, so `resolve_manifest_artifacts`
//! short-circuits before any S3 read; presigning itself is a local signing operation and
//! never touches the network. No S3 or network access is required.
//!
//! Gating: see the module docs in `tests/common/mod.rs`. CI must run
//! `OTA_REQUIRE_DOCKER_TESTS=1 cargo test --all-features`.

mod common;

use common::{auth_token, bundle_id, SeedBundle, TestApp, NIL_UUID};
use rn_ota_server_rust::cohort;
use serde_json::json;

const APP_A: &str = "app-a";
const APP_B: &str = "app-b";

/// `GET /{app}/hot-updater/app-version/{platform}/{appVersion}/{channel}/{min}/{current}`
fn app_version_url(
    app: &str,
    platform: &str,
    version: &str,
    channel: &str,
    current: &str,
) -> String {
    format!("/{app}/hot-updater/app-version/{platform}/{version}/{channel}/{NIL_UUID}/{current}")
}

fn app_version_url_min(
    app: &str,
    platform: &str,
    version: &str,
    channel: &str,
    min: &str,
    current: &str,
) -> String {
    format!("/{app}/hot-updater/app-version/{platform}/{version}/{channel}/{min}/{current}")
}

fn fingerprint_url(
    app: &str,
    platform: &str,
    fingerprint: &str,
    channel: &str,
    current: &str,
) -> String {
    format!(
        "/{app}/hot-updater/fingerprint/{platform}/{fingerprint}/{channel}/{NIL_UUID}/{current}"
    )
}

/// The device SDK only understands the explicit `{"status":"UP_TO_DATE"}` body from 0.31.0
/// onwards; older clients must keep getting a bare `null`.
const SDK_HEADER: &str = "Hot-Updater-SDK-Version";

// ---------------------------------------------------------------------------
// Decisions against real rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn app_version_check_returns_update_for_the_newest_eligible_bundle() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let older = bundle_id(1);
    let newest = bundle_id(2);
    app.seed(&SeedBundle::new(&older, APP_A)).await;
    app.seed(&SeedBundle::new(&newest, APP_A).message("shiny new bundle"))
        .await;

    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());

    let body = res.json();
    assert_eq!(body["status"], json!("UPDATE"));
    assert_eq!(body["id"], json!(newest));
    assert_eq!(body["fileHash"], json!(format!("hash-{newest}")));
    assert_eq!(body["message"], json!("shiny new bundle"));
    assert_eq!(body["shouldForceUpdate"], json!(false));
    assert!(
        body["fileUrl"]
            .as_str()
            .is_some_and(|u| u.contains(&newest)),
        "fileUrl should be a presigned URL for the bundle: {}",
        body["fileUrl"]
    );
    // No manifest on the row, so the manifest triple must be absent as a whole.
    assert_eq!(body["manifestUrl"], json!(null));
    assert_eq!(body["manifestFileHash"], json!(null));
    assert_eq!(body["changedAssets"], json!(null));
}

#[tokio::test]
async fn app_version_check_forces_the_update_when_the_bundle_says_so() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A).force_update(true))
        .await;

    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["status"], json!("UPDATE"));
    assert_eq!(body["shouldForceUpdate"], json!(true));
}

#[tokio::test]
async fn app_version_check_rolls_back_when_the_current_bundle_is_gone() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let previous = bundle_id(1);
    let current = bundle_id(2);
    app.seed(&SeedBundle::new(&previous, APP_A)).await;
    // The bundle the device is running has been disabled, so it is no longer a candidate.
    app.seed(&SeedBundle::new(&current, APP_A).enabled(false))
        .await;

    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            &current,
        ))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());

    let body = res.json();
    assert_eq!(body["status"], json!("ROLLBACK"));
    assert_eq!(body["id"], json!(previous));
    // A ROLLBACK is always forced, whatever the bundle's own flag says.
    assert_eq!(body["shouldForceUpdate"], json!(true));
}

#[tokio::test]
async fn app_version_check_rolls_back_to_the_native_bundle_when_nothing_matches() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    // No rows at all, and the device is running something newer than minBundleId.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            &bundle_id(9),
        ))
        .await
        .json();

    assert_eq!(body["status"], json!("ROLLBACK"));
    assert_eq!(body["id"], json!(NIL_UUID));
    assert_eq!(body["shouldForceUpdate"], json!(true));
    assert_eq!(body["fileUrl"], json!(null));
    assert_eq!(body["fileHash"], json!(null));
}

#[tokio::test]
async fn app_version_check_reports_up_to_date_to_modern_sdks() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;
    let uri = app_version_url(APP_A, "ios", "1.0.0", "production", &id);

    let res = app.get_with_header(&uri, SDK_HEADER, "0.31.0").await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!({ "status": "UP_TO_DATE" }));

    // 0.35.x is what the README targets.
    let res = app.get_with_header(&uri, SDK_HEADER, "0.35.8").await;
    assert_eq!(res.json(), json!({ "status": "UP_TO_DATE" }));
}

#[tokio::test]
async fn app_version_check_returns_null_to_pre_0_31_sdks() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;
    let uri = app_version_url(APP_A, "ios", "1.0.0", "production", &id);

    // No header at all.
    let res = app.get_anon(&uri).await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!(null));

    // An SDK older than the one that learned to read UP_TO_DATE.
    let res = app.get_with_header(&uri, SDK_HEADER, "0.30.9").await;
    assert_eq!(res.json(), json!(null));
}

// ---------------------------------------------------------------------------
// Candidate-query filtering (the part the pure decision fixtures cannot reach)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn candidates_are_filtered_by_channel() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let prod = bundle_id(1);
    let beta = bundle_id(2);
    app.seed(&SeedBundle::new(&prod, APP_A).channel("production"))
        .await;
    app.seed(&SeedBundle::new(&beta, APP_A).channel("beta"))
        .await;

    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(prod), "the beta bundle must not leak");

    let body = app
        .get_anon(&app_version_url(APP_A, "ios", "1.0.0", "beta", NIL_UUID))
        .await
        .json();
    assert_eq!(body["id"], json!(beta));
}

#[tokio::test]
async fn candidates_are_filtered_by_platform() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let ios = bundle_id(1);
    let android = bundle_id(2);
    app.seed(&SeedBundle::new(&ios, APP_A).platform("ios"))
        .await;
    app.seed(&SeedBundle::new(&android, APP_A).platform("android"))
        .await;

    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(ios));

    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "android",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(android));
}

#[tokio::test]
async fn disabled_bundles_are_never_offered() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    app.seed(&SeedBundle::new(&bundle_id(1), APP_A).enabled(false))
        .await;

    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.json(), json!(null));
}

#[tokio::test]
async fn candidates_are_filtered_by_semver_range() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let exact = bundle_id(1);
    let range = bundle_id(2);
    app.seed(&SeedBundle::new(&exact, APP_A).target_app_version(Some("1.0.0")))
        .await;
    app.seed(&SeedBundle::new(&range, APP_A).target_app_version(Some(">=2.0.0")))
        .await;

    // A 1.0.0 device only sees the exact-match bundle.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(exact));

    // A 2.1.0 device only satisfies the range bundle.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "2.1.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(range));

    // A 3.0.0 device: still only the range bundle.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "3.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(range));
}

#[tokio::test]
async fn min_bundle_id_excludes_older_candidates() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    app.seed(&SeedBundle::new(&bundle_id(1), APP_A)).await;
    app.seed(&SeedBundle::new(&bundle_id(2), APP_A)).await;

    // Without a floor, a device on bundle 4 (which no longer exists) rolls back to 2.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            &bundle_id(4),
        ))
        .await
        .json();
    assert_eq!(body["status"], json!("ROLLBACK"));
    assert_eq!(body["id"], json!(bundle_id(2)));

    // Raising minBundleId above every stored bundle removes them all from the candidate
    // set, and since the device is at or below the floor there is nothing to say.
    let body = app
        .get_anon(&app_version_url_min(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            &bundle_id(5),
            &bundle_id(4),
        ))
        .await
        .json();
    assert_eq!(body, json!(null));
}

#[tokio::test]
async fn one_apps_bundles_are_never_offered_to_another_app() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };

    let b_bundle = bundle_id(1);
    app.seed(&SeedBundle::new(&b_bundle, APP_B)).await;

    // app-a has no bundles of its own; it must not be handed app-b's.
    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!(null));

    // app-b does get it.
    let body = app
        .get_anon(&app_version_url(
            APP_B,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["id"], json!(b_bundle));
}

// ---------------------------------------------------------------------------
// Cohorts / staged rollout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_partially_rolled_out_bundle_is_offered_only_to_cohorts_inside_the_rollout() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A).rollout(1)).await;

    // Pick one cohort inside and one outside the 1/1000 rollout window for this bundle id.
    let inside = (1..=1000)
        .find(|c| cohort::get_numeric_cohort_rollout_position(&id, *c) < 1)
        .expect("some cohort must fall inside the rollout");
    let outside = (1..=1000)
        .find(|c| cohort::get_numeric_cohort_rollout_position(&id, *c) >= 1)
        .expect("some cohort must fall outside the rollout");

    let body = app
        .get_anon(&format!(
            "{}/{inside}",
            app_version_url(APP_A, "ios", "1.0.0", "production", NIL_UUID)
        ))
        .await
        .json();
    assert_eq!(body["status"], json!("UPDATE"));
    assert_eq!(body["id"], json!(id));

    let body = app
        .get_anon(&format!(
            "{}/{outside}",
            app_version_url(APP_A, "ios", "1.0.0", "production", NIL_UUID)
        ))
        .await
        .json();
    assert_eq!(body, json!(null), "cohort {outside} is outside the rollout");

    // A device that sends no cohort at all only ever gets fully rolled-out bundles.
    let body = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body, json!(null));
}

#[tokio::test]
async fn an_explicitly_targeted_cohort_bypasses_the_rollout_window() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, APP_A)
            .rollout(0)
            .target_cohorts(&["qa-team"]),
    )
    .await;

    let base = app_version_url(APP_A, "ios", "1.0.0", "production", NIL_UUID);

    let body = app.get_anon(&format!("{base}/qa-team")).await.json();
    assert_eq!(body["status"], json!("UPDATE"));
    assert_eq!(body["id"], json!(id));

    let body = app.get_anon(&format!("{base}/other-team")).await.json();
    assert_eq!(body, json!(null));
}

// ---------------------------------------------------------------------------
// Fingerprint strategy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fingerprint_check_matches_only_the_exact_hash() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, APP_A)
            .target_app_version(None)
            .fingerprint("fp-aaa"),
    )
    .await;

    let body = app
        .get_anon(&fingerprint_url(
            APP_A,
            "ios",
            "fp-aaa",
            "production",
            NIL_UUID,
        ))
        .await
        .json();
    assert_eq!(body["status"], json!("UPDATE"));
    assert_eq!(body["id"], json!(id));

    let res = app
        .get_anon(&fingerprint_url(
            APP_A,
            "ios",
            "fp-bbb",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!(null));
}

#[tokio::test]
async fn fingerprint_check_reports_up_to_date_to_modern_sdks() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, APP_A)
            .target_app_version(None)
            .fingerprint("fp-aaa"),
    )
    .await;

    let res = app
        .get_with_header(
            &fingerprint_url(APP_A, "ios", "fp-aaa", "production", &id),
            SDK_HEADER,
            "0.35.8",
        )
        .await;
    assert_eq!(res.json(), json!({ "status": "UP_TO_DATE" }));
}

// ---------------------------------------------------------------------------
// Request validation and auth posture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_app_is_not_found_on_both_strategies() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .get_anon(&app_version_url(
            "no-such-app",
            "ios",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 404, "body: {}", res.text());

    let res = app
        .get_anon(&fingerprint_url(
            "no-such-app",
            "ios",
            "fp",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 404, "body: {}", res.text());
}

#[tokio::test]
async fn an_invalid_platform_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "windows",
            "1.0.0",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());

    let res = app
        .get_anon(&fingerprint_url(
            APP_A,
            "windows",
            "fp",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());
}

#[tokio::test]
async fn an_unparseable_app_version_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .get_anon(&app_version_url(
            APP_A,
            "ios",
            "not-a-version",
            "production",
            NIL_UUID,
        ))
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());
}

/// The device-facing routes carry no bearer token. They must work without one, and must
/// not start failing just because a client happens to send an irrelevant header.
#[tokio::test]
async fn update_check_routes_are_unauthenticated_by_design() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;
    let uri = app_version_url(APP_A, "ios", "1.0.0", "production", NIL_UUID);

    let anonymous = app.get_anon(&uri).await;
    assert_eq!(anonymous.status, 200);
    assert_eq!(anonymous.json()["id"], json!(id));

    let headers = [
        "Bearer totally-wrong".to_string(),
        format!("Bearer {}", auth_token(APP_A)),
    ];
    for header in headers {
        let res = app.get_with_auth(&uri, &header).await;
        assert_eq!(
            res.status, 200,
            "Authorization: {header:?} must be ignored, not rejected"
        );
        assert_eq!(res.json()["id"], json!(id));
    }
}
