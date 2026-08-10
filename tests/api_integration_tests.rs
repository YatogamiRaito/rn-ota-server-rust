//! End-to-end tests for the authenticated CLI API (`/{app}/hot-updater/api/bundles*`)
//! against a real MySQL 8 database.
//!
//! These tests are gated on the availability of a MySQL backend — see the module docs in
//! `tests/common/mod.rs` for the env vars and for the exact command CI must run
//! (`OTA_REQUIRE_DOCKER_TESTS=1 cargo test --all-features`).
//!
//! Everything here asserts the documented HTTP contract (status codes and response
//! bodies) rather than any internal helper, so the suite stays valid while `src/routes/api.rs`
//! is refactored underneath it.

mod common;

use common::{auth_token, bundle_id, SeedBundle, TestApp, BUCKET};
use serde_json::json;

const APP_A: &str = "app-a";
const APP_B: &str = "app-b";

fn bundles_url(app: &str) -> String {
    format!("/{app}/hot-updater/api/bundles")
}

fn bundle_url(app: &str, id: &str) -> String {
    format!("/{app}/hot-updater/api/bundles/{id}")
}

fn channels_url(app: &str) -> String {
    format!("/{app}/hot-updater/api/bundles/channels")
}

async fn patch_count(app: &TestApp) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bundle_patches")
        .fetch_one(&app.pool)
        .await
        .expect("failed to count bundle_patches")
}

/// Upstream *omits* `nextCursor` / `previousCursor` when there is none rather than
/// serialising `null`. Indexing a `serde_json::Value` with a missing key also yields
/// `Value::Null`, so `body["pagination"]["nextCursor"] == null` cannot tell the two apart
/// — the key has to be probed on the object itself.
fn assert_cursor_absent(body: &serde_json::Value, key: &str) {
    let pagination = body["pagination"]
        .as_object()
        .expect("`pagination` must be an object");
    assert!(
        !pagination.contains_key(key),
        "{key} must be omitted, not present; pagination = {}",
        serde_json::to_string(pagination).unwrap()
    );
}

fn assert_cursor_is(body: &serde_json::Value, key: &str, expected: &str) {
    let pagination = body["pagination"]
        .as_object()
        .expect("`pagination` must be an object");
    assert_eq!(
        pagination.get(key).and_then(|v| v.as_str()),
        Some(expected),
        "unexpected {key}; pagination = {}",
        serde_json::to_string(pagination).unwrap()
    );
}

/// Helper: the `id`s in the order the list endpoint returned them.
fn listed_ids(body: &serde_json::Value) -> Vec<String> {
    body["data"]
        .as_array()
        .expect("`data` must be an array")
        .iter()
        .map(|b| {
            b["id"]
                .as_str()
                .expect("bundle id must be a string")
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_authorization_header_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app.get_anon(&bundles_url(APP_A)).await;
    assert_eq!(res.status, 401, "body: {}", res.text());
}

#[tokio::test]
async fn malformed_authorization_header_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let headers = [
        String::new(),
        "Bearer".to_string(),
        "Basic dXNlcjpwYXNz".to_string(),
        auth_token(APP_A),                        // token with no scheme
        format!("bearer {}", auth_token(APP_A)),  // wrong-case scheme
        format!("Bearer  {}", auth_token(APP_A)), // double space
    ];

    for header in headers {
        let res = app.get_with_auth(&bundles_url(APP_A), &header).await;
        assert_eq!(
            res.status,
            401,
            "Authorization: {header:?} should not authenticate; body: {}",
            res.text()
        );
    }
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .get_with_auth(&bundles_url(APP_A), "Bearer definitely-not-the-token")
        .await;
    assert_eq!(res.status, 401, "body: {}", res.text());
}

#[tokio::test]
async fn correct_token_is_accepted_on_every_cli_endpoint() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;

    assert_eq!(app.get_as(APP_A, &bundles_url(APP_A)).await.status, 200);
    assert_eq!(app.get_as(APP_A, &channels_url(APP_A)).await.status, 200);
    assert_eq!(app.get_as(APP_A, &bundle_url(APP_A, &id)).await.status, 200);
    assert_eq!(
        app.patch_as(APP_A, &bundle_url(APP_A, &id), &json!({ "message": "hi" }))
            .await
            .status,
        200
    );
    assert_eq!(
        app.delete_as(APP_A, &bundle_url(APP_A, &id)).await.status,
        200
    );
    assert_eq!(
        app.post_as(
            APP_A,
            &bundles_url(APP_A),
            &json!({
                "id": bundle_id(2),
                "platform": "ios",
                "fileHash": "h",
                "storageUri": format!("s3://{BUCKET}/x.zip"),
                "targetAppVersion": "1.0.0",
            })
        )
        .await
        .status,
        201
    );
}

#[tokio::test]
async fn token_for_one_app_does_not_authenticate_another_app() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };

    // A's token presented on B's routes, and vice versa.
    for (token_app, target_app) in [(APP_A, APP_B), (APP_B, APP_A)] {
        for uri in [
            bundles_url(target_app),
            channels_url(target_app),
            bundle_url(target_app, &bundle_id(1)),
        ] {
            let res = app
                .get_with_auth(&uri, &format!("Bearer {}", auth_token(token_app)))
                .await;
            assert_eq!(
                res.status,
                401,
                "{token_app}'s token must not work on {uri}; body: {}",
                res.text()
            );
        }
    }
}

#[tokio::test]
async fn unknown_app_is_not_found() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .get_with_auth(
            &bundles_url("no-such-app"),
            &format!("Bearer {}", auth_token(APP_A)),
        )
        .await;
    assert_eq!(res.status, 404, "body: {}", res.text());
}

// ---------------------------------------------------------------------------
// 2. Cross-app tenant isolation
// ---------------------------------------------------------------------------

async fn seed_two_tenants(app: &TestApp) -> (String, String) {
    let a_id = bundle_id(10);
    let b_id = bundle_id(20);
    app.seed(&SeedBundle::new(&a_id, APP_A).channel("production"))
        .await;
    app.seed(&SeedBundle::new(&b_id, APP_B).channel("b-only-channel"))
        .await;
    (a_id, b_id)
}

#[tokio::test]
async fn list_never_leaks_another_apps_bundles() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };
    let (a_id, b_id) = seed_two_tenants(&app).await;

    let res = app.get_as(APP_A, &bundles_url(APP_A)).await;
    assert_eq!(res.status, 200);
    assert_eq!(listed_ids(&res.json()), vec![a_id]);

    let res = app.get_as(APP_B, &bundles_url(APP_B)).await;
    assert_eq!(res.status, 200);
    assert_eq!(listed_ids(&res.json()), vec![b_id]);
}

#[tokio::test]
async fn channels_never_leak_another_apps_channels() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };
    seed_two_tenants(&app).await;

    let res = app.get_as(APP_A, &channels_url(APP_A)).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["data"]["channels"], json!(["production"]));

    let res = app.get_as(APP_B, &channels_url(APP_B)).await;
    assert_eq!(res.json()["data"]["channels"], json!(["b-only-channel"]));
}

#[tokio::test]
async fn get_cannot_read_another_apps_bundle() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };
    let (a_id, b_id) = seed_two_tenants(&app).await;

    let res = app.get_as(APP_A, &bundle_url(APP_A, &b_id)).await;
    assert_eq!(res.status, 404, "body: {}", res.text());

    let res = app.get_as(APP_B, &bundle_url(APP_B, &a_id)).await;
    assert_eq!(res.status, 404, "body: {}", res.text());
}

#[tokio::test]
async fn patch_cannot_modify_another_apps_bundle() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };
    let (_a_id, b_id) = seed_two_tenants(&app).await;
    let before = app.file_hash_of(APP_B, &b_id).await;

    let res = app
        .patch_as(
            APP_A,
            &bundle_url(APP_A, &b_id),
            &json!({ "fileHash": "hijacked", "enabled": false }),
        )
        .await;
    assert_eq!(res.status, 404, "body: {}", res.text());
    assert_eq!(
        app.file_hash_of(APP_B, &b_id).await,
        before,
        "app-a's PATCH must not touch app-b's row"
    );
}

#[tokio::test]
async fn delete_cannot_remove_another_apps_bundle() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };
    let (_a_id, b_id) = seed_two_tenants(&app).await;

    // Note: the handler answers 200 {"success":true} even when nothing matched. What
    // matters for the tenant boundary is that app-b's row survives.
    app.delete_as(APP_A, &bundle_url(APP_A, &b_id)).await;
    assert_eq!(
        app.bundle_ids_of(APP_B).await,
        vec![b_id],
        "app-a's DELETE must not remove app-b's bundle"
    );
}

#[tokio::test]
async fn create_cannot_overwrite_another_apps_bundle() {
    let Some(app) = TestApp::spawn(&[APP_A, APP_B]).await else {
        return;
    };

    let a_id = bundle_id(10);
    app.seed(&SeedBundle::new(&a_id, APP_A)).await;
    let original_hash = app
        .file_hash_of(APP_A, &a_id)
        .await
        .expect("seeded bundle must exist");

    // app-b POSTs a bundle whose id collides with app-a's. `bundles.id` is the primary key
    // and the INSERT is an `ON DUPLICATE KEY UPDATE` upsert that never re-checks app_name,
    // so this is the cross-tenant write path.
    app.post_as(
        APP_B,
        &bundles_url(APP_B),
        &json!({
            "id": a_id,
            "platform": "android",
            "fileHash": "hijacked-by-app-b",
            "storageUri": format!("s3://{BUCKET}/evil.zip"),
            "targetAppVersion": "9.9.9",
            "channel": "hijacked",
        }),
    )
    .await;

    assert_eq!(
        app.file_hash_of(APP_A, &a_id).await.as_deref(),
        Some(original_hash.as_str()),
        "app-b must not be able to overwrite app-a's bundle by reusing its id"
    );
}

// ---------------------------------------------------------------------------
// 3. The six CLI endpoints, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channels_returns_distinct_channels() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    app.seed(&SeedBundle::new(&bundle_id(1), APP_A).channel("production"))
        .await;
    app.seed(&SeedBundle::new(&bundle_id(2), APP_A).channel("production"))
        .await;
    app.seed(&SeedBundle::new(&bundle_id(3), APP_A).channel("beta"))
        .await;

    let res = app.get_as(APP_A, &channels_url(APP_A)).await;
    assert_eq!(res.status, 200);
    let mut channels: Vec<String> = res.json()["data"]["channels"]
        .as_array()
        .expect("channels must be an array")
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    channels.sort();
    assert_eq!(channels, vec!["beta".to_string(), "production".to_string()]);
}

#[tokio::test]
async fn channels_is_empty_for_an_app_with_no_bundles() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app.get_as(APP_A, &channels_url(APP_A)).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json()["data"]["channels"], json!([]));
}

#[tokio::test]
async fn create_then_get_round_trips_every_field() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    let res = app
        .post_as(
            APP_A,
            &bundles_url(APP_A),
            &json!({
                "id": id,
                "platform": "android",
                "shouldForceUpdate": true,
                "enabled": false,
                "fileHash": "file-hash-1",
                "gitCommitHash": "deadbeef",
                "message": "first release",
                "channel": "beta",
                "storageUri": format!("s3://{BUCKET}/{id}/bundle.zip"),
                "targetAppVersion": "1.2.3",
                "metadata": { "buildNumber": 42 },
                "rolloutCohortCount": 500,
                "targetCohorts": ["beta-testers"],
                "manifestStorageUri": format!("s3://{BUCKET}/{id}/manifest.json"),
                "manifestFileHash": "manifest-hash-1",
                "assetBaseStorageUri": format!("s3://{BUCKET}/assets"),
            }),
        )
        .await;
    assert_eq!(res.status, 201, "body: {}", res.text());
    assert_eq!(res.json(), json!({ "success": true }));

    let res = app.get_as(APP_A, &bundle_url(APP_A, &id)).await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    let body = res.json();
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["platform"], json!("android"));
    assert_eq!(body["shouldForceUpdate"], json!(true));
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["fileHash"], json!("file-hash-1"));
    assert_eq!(body["gitCommitHash"], json!("deadbeef"));
    assert_eq!(body["message"], json!("first release"));
    assert_eq!(body["channel"], json!("beta"));
    assert_eq!(body["targetAppVersion"], json!("1.2.3"));
    assert_eq!(body["fingerprintHash"], json!(null));
    assert_eq!(body["metadata"], json!({ "buildNumber": 42 }));
    assert_eq!(body["rolloutCohortCount"], json!(500));
    assert_eq!(body["targetCohorts"], json!(["beta-testers"]));
    assert_eq!(body["manifestFileHash"], json!("manifest-hash-1"));
    assert_eq!(body["patches"], json!([]));
    assert_eq!(body["patchBaseBundleId"], json!(null));
}

#[tokio::test]
async fn create_accepts_an_array_and_persists_patches() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let base = bundle_id(1);
    let target = bundle_id(2);
    let res = app
        .post_as(
            APP_A,
            &bundles_url(APP_A),
            &json!([
                {
                    "id": base,
                    "platform": "ios",
                    "fileHash": "base-hash",
                    "storageUri": format!("s3://{BUCKET}/{base}/bundle.zip"),
                    "targetAppVersion": "1.0.0",
                },
                {
                    "id": target,
                    "platform": "ios",
                    "fileHash": "target-hash",
                    "storageUri": format!("s3://{BUCKET}/{target}/bundle.zip"),
                    "targetAppVersion": "1.0.0",
                    "patches": [{
                        "baseBundleId": base,
                        "baseFileHash": "base-hash",
                        "patchFileHash": "patch-hash",
                        "patchStorageUri": format!("s3://{BUCKET}/{target}/patch.bin"),
                    }],
                }
            ]),
        )
        .await;
    assert_eq!(res.status, 201, "body: {}", res.text());

    let res = app.get_as(APP_A, &bundle_url(APP_A, &target)).await;
    let body = res.json();
    assert_eq!(
        body["patches"],
        json!([{
            "baseBundleId": base,
            "baseFileHash": "base-hash",
            "patchFileHash": "patch-hash",
            "patchStorageUri": format!("s3://{BUCKET}/{target}/patch.bin"),
        }])
    );
    // The flattened "primary patch" mirror fields must agree with patches[0].
    assert_eq!(body["patchBaseBundleId"], json!(base));
    assert_eq!(body["patchBaseFileHash"], json!("base-hash"));
    assert_eq!(body["patchFileHash"], json!("patch-hash"));
}

#[tokio::test]
async fn create_is_an_upsert_and_replaces_patches() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    let body = json!({
        "id": id,
        "platform": "ios",
        "fileHash": "v1",
        "storageUri": format!("s3://{BUCKET}/{id}/bundle.zip"),
        "targetAppVersion": "1.0.0",
    });
    assert_eq!(
        app.post_as(APP_A, &bundles_url(APP_A), &body).await.status,
        201
    );

    let mut second = body.clone();
    second["fileHash"] = json!("v2");
    second["message"] = json!("updated");
    assert_eq!(
        app.post_as(APP_A, &bundles_url(APP_A), &second)
            .await
            .status,
        201
    );

    assert_eq!(app.bundle_ids_of(APP_A).await, vec![id.clone()]);
    let got = app.get_as(APP_A, &bundle_url(APP_A, &id)).await.json();
    assert_eq!(got["fileHash"], json!("v2"));
    assert_eq!(got["message"], json!("updated"));
}

#[tokio::test]
async fn create_rejects_a_bundle_with_neither_version_nor_fingerprint() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    let res = app
        .post_as(
            APP_A,
            &bundles_url(APP_A),
            &json!({
                "id": id,
                "platform": "ios",
                "fileHash": "h",
                "storageUri": format!("s3://{BUCKET}/{id}/bundle.zip"),
            }),
        )
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());
    assert!(
        res.text().contains("targetAppVersion"),
        "unexpected message: {}",
        res.text()
    );
    assert!(app.bundle_ids_of(APP_A).await.is_empty());
}

#[tokio::test]
async fn create_rejects_an_out_of_range_rollout_cohort_count() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for count in [-1, 1001] {
        let id = bundle_id(1);
        let res = app
            .post_as(
                APP_A,
                &bundles_url(APP_A),
                &json!({
                    "id": id,
                    "platform": "ios",
                    "fileHash": "h",
                    "storageUri": format!("s3://{BUCKET}/{id}/bundle.zip"),
                    "targetAppVersion": "1.0.0",
                    "rolloutCohortCount": count,
                }),
            )
            .await;
        assert_eq!(
            res.status,
            400,
            "rolloutCohortCount={count}: {}",
            res.text()
        );
    }
    assert!(app.bundle_ids_of(APP_A).await.is_empty());
}

#[tokio::test]
async fn create_rejects_an_invalid_target_cohort() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    let res = app
        .post_as(
            APP_A,
            &bundles_url(APP_A),
            &json!({
                "id": id,
                "platform": "ios",
                "fileHash": "h",
                "storageUri": format!("s3://{BUCKET}/{id}/bundle.zip"),
                "targetAppVersion": "1.0.0",
                "targetCohorts": ["Not A Valid Cohort"],
            }),
        )
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());
    assert!(app.bundle_ids_of(APP_A).await.is_empty());
}

#[tokio::test]
async fn get_unknown_bundle_is_not_found() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app.get_as(APP_A, &bundle_url(APP_A, &bundle_id(999))).await;
    assert_eq!(res.status, 404, "body: {}", res.text());
}

#[tokio::test]
async fn patch_updates_only_the_submitted_fields() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, APP_A)
            .channel("production")
            .message("original message"),
    )
    .await;

    let res = app
        .patch_as(
            APP_A,
            &bundle_url(APP_A, &id),
            &json!({ "enabled": false, "channel": "beta" }),
        )
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!({ "success": true }));

    let got = app.get_as(APP_A, &bundle_url(APP_A, &id)).await.json();
    assert_eq!(got["enabled"], json!(false));
    assert_eq!(got["channel"], json!("beta"));
    // Untouched fields must survive.
    assert_eq!(got["message"], json!("original message"));
    assert_eq!(got["platform"], json!("ios"));
    assert_eq!(got["targetAppVersion"], json!("1.0.0"));
}

#[tokio::test]
async fn patch_with_a_mismatched_id_is_rejected() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;

    let res = app
        .patch_as(
            APP_A,
            &bundle_url(APP_A, &id),
            &json!({ "id": bundle_id(2), "enabled": false }),
        )
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());

    let got = app.get_as(APP_A, &bundle_url(APP_A, &id)).await.json();
    assert_eq!(got["enabled"], json!(true), "the row must be untouched");
}

#[tokio::test]
async fn patch_of_an_unknown_bundle_is_not_found() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let res = app
        .patch_as(
            APP_A,
            &bundle_url(APP_A, &bundle_id(999)),
            &json!({ "enabled": false }),
        )
        .await;
    assert_eq!(res.status, 404, "body: {}", res.text());
}

#[tokio::test]
async fn patch_with_an_empty_body_is_a_successful_no_op() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A).message("keep me"))
        .await;

    let res = app
        .patch_as(APP_A, &bundle_url(APP_A, &id), &json!({}))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!({ "success": true }));

    let got = app.get_as(APP_A, &bundle_url(APP_A, &id)).await.json();
    assert_eq!(got["message"], json!("keep me"));
}

#[tokio::test]
async fn patch_rejects_a_merged_row_that_violates_the_constraints() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(&SeedBundle::new(&id, APP_A)).await;

    let res = app
        .patch_as(
            APP_A,
            &bundle_url(APP_A, &id),
            &json!({ "rolloutCohortCount": 5000 }),
        )
        .await;
    assert_eq!(res.status, 400, "body: {}", res.text());

    let got = app.get_as(APP_A, &bundle_url(APP_A, &id)).await.json();
    assert_eq!(got["rolloutCohortCount"], json!(1000));
}

#[tokio::test]
async fn delete_removes_the_bundle_and_cascades_to_its_patches() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let base = bundle_id(1);
    let target = bundle_id(2);
    app.post_as(
        APP_A,
        &bundles_url(APP_A),
        &json!([
            {
                "id": base,
                "platform": "ios",
                "fileHash": "base-hash",
                "storageUri": format!("s3://{BUCKET}/{base}/bundle.zip"),
                "targetAppVersion": "1.0.0",
            },
            {
                "id": target,
                "platform": "ios",
                "fileHash": "target-hash",
                "storageUri": format!("s3://{BUCKET}/{target}/bundle.zip"),
                "targetAppVersion": "1.0.0",
                "patches": [{
                    "baseBundleId": base,
                    "baseFileHash": "base-hash",
                    "patchFileHash": "patch-hash",
                    "patchStorageUri": format!("s3://{BUCKET}/{target}/patch.bin"),
                }],
            }
        ]),
    )
    .await;

    assert_eq!(patch_count(&app).await, 1);

    let res = app.delete_as(APP_A, &bundle_url(APP_A, &target)).await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json(), json!({ "success": true }));

    assert_eq!(app.bundle_ids_of(APP_A).await, vec![base]);
    assert_eq!(
        patch_count(&app).await,
        0,
        "ON DELETE CASCADE must remove the patch row"
    );
    assert_eq!(
        app.get_as(APP_A, &bundle_url(APP_A, &target)).await.status,
        404
    );
}

// ---------------------------------------------------------------------------
// 4. Filters and pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_applies_channel_platform_and_enabled_filters() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    let prod_ios = bundle_id(1);
    let beta_ios = bundle_id(2);
    let prod_android = bundle_id(3);
    let prod_ios_disabled = bundle_id(4);
    app.seed(&SeedBundle::new(&prod_ios, APP_A)).await;
    app.seed(&SeedBundle::new(&beta_ios, APP_A).channel("beta"))
        .await;
    app.seed(&SeedBundle::new(&prod_android, APP_A).platform("android"))
        .await;
    app.seed(&SeedBundle::new(&prod_ios_disabled, APP_A).enabled(false))
        .await;

    let base = bundles_url(APP_A);
    let res = app.get_as(APP_A, &format!("{base}?channel=beta")).await;
    assert_eq!(listed_ids(&res.json()), vec![beta_ios]);

    let res = app.get_as(APP_A, &format!("{base}?platform=android")).await;
    assert_eq!(listed_ids(&res.json()), vec![prod_android]);

    let res = app.get_as(APP_A, &format!("{base}?enabled=false")).await;
    assert_eq!(listed_ids(&res.json()), vec![prod_ios_disabled.clone()]);

    let res = app
        .get_as(
            APP_A,
            &format!("{base}?channel=production&platform=ios&enabled=true"),
        )
        .await;
    assert_eq!(listed_ids(&res.json()), vec![prod_ios]);

    // idIn / idEq
    let res = app
        .get_as(APP_A, &format!("{base}?idEq={prod_ios_disabled}"))
        .await;
    assert_eq!(listed_ids(&res.json()), vec![prod_ios_disabled]);
}

/// 25 bundles paged 10 at a time: pages must be disjoint, in stable id-DESC order, and
/// together reconstruct the full list exactly once.
#[tokio::test]
async fn offset_pagination_pages_are_stable_and_disjoint() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=25u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let expected: Vec<String> = (1..=25u32).rev().map(bundle_id).collect();
    let base = bundles_url(APP_A);

    let mut seen = Vec::new();
    for page in 1..=3 {
        let res = app
            .get_as(APP_A, &format!("{base}?limit=10&page={page}"))
            .await;
        assert_eq!(res.status, 200, "body: {}", res.text());
        let body = res.json();

        assert_eq!(body["pagination"]["total"], json!(25));
        assert_eq!(body["pagination"]["totalPages"], json!(3));
        assert_eq!(body["pagination"]["currentPage"], json!(page));
        assert_eq!(body["pagination"]["hasPreviousPage"], json!(page > 1));
        assert_eq!(body["pagination"]["hasNextPage"], json!(page < 3));

        seen.extend(listed_ids(&body));
    }

    assert_eq!(seen, expected, "pages must tile the full id-DESC ordering");
}

#[tokio::test]
async fn offset_pagination_boundaries() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=25u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    // Last (partial) page.
    let body = app
        .get_as(APP_A, &format!("{base}?limit=10&page=3"))
        .await
        .json();
    assert_eq!(listed_ids(&body).len(), 5);
    assert_eq!(body["pagination"]["hasNextPage"], json!(false));
    assert_cursor_absent(&body, "nextCursor");

    // A page past the end is clamped to the LAST page and re-queried, rather than
    // answered with an empty array (createDatabasePlugin.mjs:228-234).
    let body = app
        .get_as(APP_A, &format!("{base}?limit=10&page=9"))
        .await
        .json();
    assert_eq!(
        listed_ids(&body),
        (1..=5u32).rev().map(bundle_id).collect::<Vec<_>>(),
        "an over-large page must clamp to the last page"
    );
    assert_eq!(body["pagination"]["total"], json!(25));
    assert_eq!(body["pagination"]["currentPage"], json!(3));
    assert_eq!(body["pagination"]["hasNextPage"], json!(false));
    assert_eq!(body["pagination"]["hasPreviousPage"], json!(true));
}

/// Upstream validates `limit` / `page` / `offset` and answers 400; it does not silently
/// clamp them, and it must never turn them into a 5xx (`handler.mjs:64-70,155-157`).
#[tokio::test]
async fn invalid_pagination_parameters_are_rejected_with_400() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=25u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    for query in [
        "limit=0",
        "limit=-1",
        "limit=101",  // above MAX_PAGE_SIZE
        "limit=1000", // was silently clamped to 100 before the parity fix
        "limit=abc",
        "limit=1.5",
        "limit=",
        "page=0",
        "page=-1",
        "page=abc",
        "page=2.5",
        "offset=0", // upstream removed offset pagination and rejects the parameter
        "offset=10",
    ] {
        let res = app.get_as(APP_A, &format!("{base}?{query}")).await;
        assert_eq!(
            res.status,
            400,
            "?{query} must be a 400; got {} {}",
            res.status,
            res.text()
        );
    }

    // A huge but *valid* positive integer page is not an error: it clamps to the last page.
    let res = app
        .get_as(APP_A, &format!("{base}?limit=10&page=9223372036854775807"))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    assert_eq!(res.json()["pagination"]["currentPage"], json!(3));
}

/// When `page` is supplied the cursor is ignored entirely
/// (`createDatabasePlugin.mjs:225-236`).
#[tokio::test]
async fn page_takes_precedence_over_a_cursor() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=25u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    let with_cursor = app
        .get_as(
            APP_A,
            &format!("{base}?limit=10&page=2&after={}", bundle_id(3)),
        )
        .await
        .json();
    let without_cursor = app
        .get_as(APP_A, &format!("{base}?limit=10&page=2"))
        .await
        .json();

    assert_eq!(
        listed_ids(&with_cursor),
        listed_ids(&without_cursor),
        "`after` must be ignored when `page` is present"
    );
    assert_eq!(with_cursor["pagination"], without_cursor["pagination"]);
}

/// `after` is an id cursor over the same id-DESC ordering: walking it must produce the
/// same sequence as offset paging, with no gaps or repeats. The pagination envelope is
/// checked on every hop, because `currentPage` / `hasPreviousPage` come from the page's
/// absolute index in the full ordering, not from a `page` parameter (there is none here).
#[tokio::test]
async fn cursor_pagination_walks_the_same_ordering() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=25u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);
    let expected: Vec<String> = (1..=25u32).rev().map(bundle_id).collect();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for hop in 1..=3 {
        let uri = match &cursor {
            Some(c) => format!("{base}?limit=10&after={c}"),
            None => format!("{base}?limit=10"),
        };
        let res = app.get_as(APP_A, &uri).await;
        assert_eq!(res.status, 200, "body: {}", res.text());
        let body = res.json();
        let page = listed_ids(&body);

        // A cursor must never narrow `total` — it is counted from the base filters only.
        assert_eq!(
            body["pagination"]["total"],
            json!(25),
            "hop {hop}: a cursor must not shrink `total`"
        );
        assert_eq!(body["pagination"]["totalPages"], json!(3), "hop {hop}");
        assert_eq!(
            body["pagination"]["currentPage"],
            json!(hop),
            "hop {hop}: currentPage comes from the page's absolute index"
        );
        assert_eq!(
            body["pagination"]["hasPreviousPage"],
            json!(hop > 1),
            "hop {hop}"
        );
        assert_eq!(
            body["pagination"]["hasNextPage"],
            json!(hop < 3),
            "hop {hop}"
        );
        if hop < 3 {
            assert_cursor_is(&body, "nextCursor", page.last().unwrap());
        } else {
            assert_cursor_absent(&body, "nextCursor");
        }

        seen.extend(page.iter().cloned());
        cursor = Some(page.last().unwrap().clone());
    }

    assert_eq!(seen, expected, "cursor walk must tile the id-DESC ordering");
}

/// `before` must return the page immediately *preceding* the cursor, which means walking
/// backwards from it and reversing — not "every row above the cursor, capped at `limit`",
/// which silently returns the first page instead (`createDatabasePlugin.mjs:32-52`).
#[tokio::test]
async fn before_cursor_returns_the_preceding_page_not_the_first_page() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    // ids 1..=7, so the DESC ordering is 7,6,5,4,3,2,1 and limit=2 gives the pages
    // [7,6] [5,4] [3,2] [1].
    for seq in 1..=7u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    // The discriminating case: the rows immediately above id=1 are [3,2], whereas taking
    // the first `limit` rows greater than id=1 in DESC order would give [7,6].
    let body = app
        .get_as(APP_A, &format!("{base}?limit=2&before={}", bundle_id(1)))
        .await
        .json();
    assert_eq!(
        listed_ids(&body),
        vec![bundle_id(3), bundle_id(2)],
        "`before` must return the preceding page, not the first page"
    );
    assert_eq!(body["pagination"]["total"], json!(7));
    assert_eq!(body["pagination"]["hasPreviousPage"], json!(true));
    assert_eq!(body["pagination"]["hasNextPage"], json!(true));

    // Walking `before` back from the last page must retrace the pages in reverse.
    let body = app
        .get_as(APP_A, &format!("{base}?limit=2&before={}", bundle_id(3)))
        .await
        .json();
    assert_eq!(listed_ids(&body), vec![bundle_id(5), bundle_id(4)]);

    let body = app
        .get_as(APP_A, &format!("{base}?limit=2&before={}", bundle_id(5)))
        .await
        .json();
    assert_eq!(listed_ids(&body), vec![bundle_id(7), bundle_id(6)]);
    assert_eq!(body["pagination"]["currentPage"], json!(1));
    assert_eq!(body["pagination"]["hasPreviousPage"], json!(false));
    assert_cursor_absent(&body, "previousCursor");
}

#[tokio::test]
async fn cursor_pagination_boundaries() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=5u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    // `after` is exclusive of the cursor row itself.
    let body = app
        .get_as(APP_A, &format!("{base}?after={}", bundle_id(3)))
        .await
        .json();
    assert_eq!(listed_ids(&body), vec![bundle_id(2), bundle_id(1)]);
    // `total` still counts every row, not just those past the cursor.
    assert_eq!(body["pagination"]["total"], json!(5));

    // A cursor at the very end of the ordering yields nothing more, but is not an error.
    let res = app
        .get_as(APP_A, &format!("{base}?after={}", bundle_id(1)))
        .await;
    assert_eq!(res.status, 200, "body: {}", res.text());
    let body = res.json();
    assert_eq!(body["data"], json!([]));
    assert_eq!(body["pagination"]["total"], json!(5));
}

#[tokio::test]
async fn garbage_cursor_is_handled_without_an_error() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    for seq in 1..=3u32 {
        app.seed(&SeedBundle::new(&bundle_id(seq), APP_A)).await;
    }
    let base = bundles_url(APP_A);

    for cursor in [
        "not-a-uuid",
        "%20%20",
        "'; DROP TABLE bundles; --",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    ] {
        let encoded = cursor
            .replace(' ', "%20")
            .replace(';', "%3B")
            .replace('\'', "%27");
        let res = app.get_as(APP_A, &format!("{base}?after={encoded}")).await;
        assert!(
            res.status.is_success(),
            "after={cursor:?} produced {}: {}",
            res.status,
            res.text()
        );
        // The table must still be there — the cursor is a bound parameter, not SQL.
        assert!(res.json()["data"].is_array());
    }

    assert_eq!(app.bundle_ids_of(APP_A).await.len(), 3);
}

/// Whatever the validation decides, hostile pagination input must never reach MySQL as a
/// malformed `LIMIT`/`OFFSET`, panic on an arithmetic overflow, or otherwise 5xx.
#[tokio::test]
async fn hostile_pagination_parameters_never_produce_a_server_error() {
    let Some(app) = TestApp::spawn(&[APP_A]).await else {
        return;
    };

    app.seed(&SeedBundle::new(&bundle_id(1), APP_A)).await;
    let base = bundles_url(APP_A);

    for query in [
        "limit=-1",
        "limit=0",
        "limit=-9223372036854775808",
        "page=9223372036854775807",
        "page=99999999999999999999999999",
        "limit=50&page=9223372036854775807",
        "limit=NaN",
        "page=Infinity",
        "limit=%20%2010%20",
    ] {
        let res = app.get_as(APP_A, &format!("{base}?{query}")).await;
        assert!(
            !res.status.is_server_error(),
            "?{query} produced {}: {}",
            res.status,
            res.text()
        );
    }
}
