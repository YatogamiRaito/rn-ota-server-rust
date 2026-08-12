//! The S3/R2 storage layer against a real, S3-compatible object store (MinIO in a container).
//!
//! Presigning is a purely local operation, so the only way to know a presigned URL is *correct*
//! is to fetch it. Every test here that claims a URL works does an actual HTTP GET against the
//! store; a URL that parses but 403s or 404s is the failure mode these tests exist to catch.
//!
//! See `tests/common/mod.rs` for how the store is started and how the suite skips (or, with
//! `OTA_REQUIRE_DOCKER_TESTS=1`, hard-fails) when it is unavailable.

mod common;

use aws_sdk_s3::presigning::PresigningConfig;
use axum::http::StatusCode;
use common::{bundle_id, http_get, storage_config, SeedBundle, TestApp, TestBucket, NIL_UUID};
use rn_ota_server_rust::config::{PresignConfig, StorageTimeoutConfig};
use rn_ota_server_rust::storage::{get_presigned_url, read_s3_file};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BUNDLE_BYTES: &[u8] = b"PK\x03\x04 pretend this is a bundle.zip";

/// The five arguments `src/routes/check.rs` passes, taken from an app's `AppStorageConfig`.
async fn presign(bucket: &TestBucket, storage_uri: &str) -> anyhow::Result<String> {
    get_presigned_url(&bucket.storage_config(), storage_uri).await
}

async fn read(bucket: &TestBucket, storage_uri: &str) -> anyhow::Result<String> {
    read_s3_file(&bucket.storage_config(), storage_uri).await
}

// ---------------------------------------------------------------------------
// Presigned URL generation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presigned_url_actually_downloads_the_object() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let key = "0189abcd/bundle.zip";
    bucket.put(key, BUNDLE_BYTES).await;

    let url = presign(&bucket, &bucket.uri(key)).await.expect("presign");
    let (status, body) = http_get(&url).await;

    assert_eq!(status, 200, "presigned URL did not resolve: {url}");
    assert_eq!(
        body, BUNDLE_BYTES,
        "presigned URL resolved to the wrong content"
    );
}

#[tokio::test]
async fn presigned_url_is_well_formed_and_path_style() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    bucket.put("bundle.zip", BUNDLE_BYTES).await;

    let raw = presign(&bucket, &bucket.uri("bundle.zip"))
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap_or_else(|err| panic!("not a URL ({err}): {raw}"));

    assert_eq!(url.scheme(), "http", "endpoint scheme must be preserved");
    // force_path_style: the bucket belongs in the PATH, never in the host. Injecting it into
    // the subdomain (the SDK default) produces an unresolvable name against R2/MinIO.
    assert!(
        !url.host_str().unwrap().starts_with(&bucket.name),
        "bucket must not be virtual-hosted into the endpoint host: {raw}"
    );
    assert_eq!(url.path(), format!("/{}/bundle.zip", bucket.name));

    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("X-Amz-Algorithm").map(String::as_str),
        Some("AWS4-HMAC-SHA256")
    );
    assert!(query.contains_key("X-Amz-Signature"), "no signature: {raw}");
    assert!(query.contains_key("X-Amz-Date"), "no date: {raw}");
    assert!(
        query
            .get("X-Amz-Credential")
            .is_some_and(|c| c.starts_with(&bucket.access_key_id)),
        "credential scope must name the app's access key: {raw}"
    );
}

#[tokio::test]
async fn presigned_url_downloads_keys_with_awkward_but_common_characters() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    // This is the guard against overcorrecting the double-encoding fix. `parse_s3_uri` now
    // hands the key through untouched and the SDK does the one and only escaping — so every
    // character class below has to survive the round trip, and `/` has to keep working as a
    // path separator rather than becoming `%2F`.
    for key in [
        "0189/assets/icon@2x.png",
        "0189/assets/a_b-c.d.png",
        "0189/assets/a+b.png",
        "0189/assets/50%off.png",
        "0189/assets/sha256/ab/abcdef0123456789.png",
        "0189/assets/(1).png",
        "0189/assets/a'b.png",
        // Previously broken by the percent-encoding round trip:
        "0189/assets/my file.png",
        "0189/assets/görsel.png",
        "0189/assets/naïve tétée.png",
        "0189/assets/a?b.png",
        "0189/assets/a#b.png",
        "0189/assets/a&b=c.png",
        "0189/assets/a b/c d.png",
        "0189/assets/100%25.png",
        // A literal `..` segment is legal in S3 but MinIO refuses to store one
        // (XMinioInvalidResourceName), so it cannot be exercised end to end here. That the
        // parser leaves it alone is covered by
        // `tests/storage_uri_tests.rs::parse_s3_uri_preserves_the_key_verbatim`.
    ] {
        bucket.put(key, key.as_bytes()).await;
        let url = presign(&bucket, &bucket.uri(key))
            .await
            .unwrap_or_else(|err| panic!("presign failed for {key:?}: {err}"));
        let (status, body) = http_get(&url).await;
        assert_eq!(status, 200, "GET failed for key {key:?}: {url}");
        assert_eq!(body, key.as_bytes(), "wrong object returned for {key:?}");
    }
}

/// Regression lock. The name describes the defect this holds shut, not the current behaviour.
///
/// `parse_s3_uri` used to run the URI through `url::Url` and hand `url.path()` on as the key, so
/// a space came back as `%20`; the AWS SDK then escaped that a second time and the request went
/// to `my%2520file.png` — a 404 where the asset should have been. It now returns the key
/// verbatim and the SDK does the one and only escaping.
///
/// Both halves matter. The first is that the literal key downloads. The second is that
/// `assets/my%20file.png` addresses a DIFFERENT object (it is a key that happens to contain a
/// percent sign, and nothing here exists under that name) — pinning that is what stops a future
/// "fix" from re-introducing decoding on the way in. See
/// `tests/storage_uri_tests.rs::parse_s3_uri_preserves_the_key_verbatim`.
#[tokio::test]
async fn bug_presigned_url_for_a_key_with_a_space_hits_the_wrong_object() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let key = "assets/my file.png";
    bucket.put(key, b"the real asset").await;
    assert_eq!(
        bucket.keys().await,
        vec![key.to_string()],
        "sanity: the store holds the key with a literal space"
    );

    let raw_url = presign(&bucket, &bucket.uri(key)).await.expect("presign");
    let (raw_status, raw_body) = http_get(&raw_url).await;

    let encoded_url = presign(&bucket, &bucket.uri("assets/my%20file.png"))
        .await
        .expect("presign");
    let (encoded_status, _) = http_get(&encoded_url).await;

    assert_eq!(
        encoded_status, 404,
        "`assets/my%20file.png` is a distinct key that nothing was stored under; resolving it \
         to the space-containing object would mean the URI is being decoded on the way in: \
         {encoded_url}"
    );
    assert_eq!(
        raw_status, 200,
        "a presigned URL for a key containing a space must download that object, \
         but the store answered {raw_status} for {raw_url}"
    );
    assert_eq!(raw_body, b"the real asset");
}

#[tokio::test]
async fn presigned_url_for_a_missing_object_is_produced_anyway_and_404s() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    // Presigning never contacts the store, so a storage_uri pointing at nothing cannot fail
    // here. The device is handed a URL that 404s at download time; the update-check response
    // is a normal 200 UPDATE. That is the intended shape (the server has no cheap way to know)
    // but it means a bundle row whose object was deleted looks healthy from the server side.
    let url = presign(&bucket, &bucket.uri("does/not/exist.zip"))
        .await
        .expect("presigning a missing key still succeeds");
    let (status, _) = http_get(&url).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn presign_rejects_an_empty_key() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    // `s3://bucket` parses with an empty key; GetObject on an empty key is invalid. The error
    // has to surface as an Err (→ null fileUrl) rather than as a URL that 400s on the device.
    let result = presign(&bucket, &format!("s3://{}", bucket.name)).await;
    assert!(
        result.is_err(),
        "expected an error for an empty key, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presigned_url_expires_in_one_hour() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    bucket.put("bundle.zip", BUNDLE_BYTES).await;

    let raw = presign(&bucket, &bucket.uri("bundle.zip"))
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap();
    let expires = url
        .query_pairs()
        .find(|(k, _)| k == "X-Amz-Expires")
        .map(|(_, v)| v.into_owned())
        .expect("no X-Amz-Expires");

    // The lifetime comes from `PresignConfig` (`STORAGE_PRESIGN_EXPIRY_SECS`), which nothing
    // here sets, so this is the default. Asserted against the config rather than a literal, so
    // the two cannot drift; that the default itself is 3600 is pinned in `src/config.rs`.
    assert_eq!(
        expires,
        PresignConfig::default().expires_in.as_secs().to_string()
    );
}

#[tokio::test]
async fn an_expired_presigned_url_is_rejected_by_the_store() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    bucket.put("bundle.zip", BUNDLE_BYTES).await;

    // `get_presigned_url` cannot produce a short-lived URL (the hour is hardcoded), so the
    // signature is built here with the same credentials and an already-elapsed window. Waiting
    // out a real expiry is not an option in a test suite; backdating `start_time` is exact and
    // needs no sleep. What this proves is that the store enforces X-Amz-Expires at all — i.e.
    // that the lifetime `get_presigned_url` stamps into the URL is a real limit and not
    // decoration.
    let client = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .credentials_provider(aws_credential_types::Credentials::new(
                bucket.access_key_id.clone(),
                bucket.secret_access_key.clone(),
                None,
                None,
                "TestCredentials",
            ))
            .region(aws_config::Region::new("auto"))
            .endpoint_url(&bucket.endpoint)
            .force_path_style(true)
            .build(),
    );
    let config = PresigningConfig::builder()
        .start_time(SystemTime::now() - Duration::from_secs(7200))
        .expires_in(Duration::from_secs(3600))
        .build()
        .expect("presigning config");
    let expired = client
        .get_object()
        .bucket(&bucket.name)
        .key("bundle.zip")
        .presigned(config)
        .await
        .expect("presign")
        .uri()
        .to_string();

    let (status, body) = http_get(&expired).await;
    assert_eq!(
        status,
        403,
        "an expired presigned URL must be refused, got {status}: {}",
        String::from_utf8_lossy(&body)
    );
}

// ---------------------------------------------------------------------------
// Per-app isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn presign_refuses_a_storage_uri_naming_another_apps_bucket() {
    let Some(app_a) = TestBucket::create().await else {
        return;
    };
    let Some(app_b) = TestBucket::create().await else {
        return;
    };
    app_b.put("secret.zip", b"app B's bundle").await;

    // A `bundles` row belonging to app A that names app B's bucket must never be signed, even
    // though app A's credentials would (here, as root) be able to read it. This is the guard in
    // `get_presigned_url`, and it is the only thing standing between a mis-seeded row and a
    // cross-tenant download link.
    let result = presign(&app_a, &app_b.uri("secret.zip")).await;
    let err = result
        .expect_err("cross-bucket presign must fail")
        .to_string();
    assert!(
        err.contains("Bucket name mismatch"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn app_credentials_cannot_read_another_apps_bucket() {
    let Some(app_a) = TestBucket::create_with_own_user().await else {
        return;
    };
    let Some(app_b) = TestBucket::create_with_own_user().await else {
        return;
    };
    app_a.put("bundle.zip", b"app A's bundle").await;
    app_b.put("bundle.zip", b"app B's bundle").await;

    // Each app has a MinIO user whose policy covers only its own bucket, exactly as a per-app
    // R2 token is scoped. Sanity: each app can read its own object.
    let own = presign(&app_a, &app_a.uri("bundle.zip"))
        .await
        .expect("presign own");
    assert_eq!(http_get(&own).await, (200, b"app A's bundle".to_vec()));

    // Now the misconfiguration the bucket-name guard cannot catch: app A configured with app
    // B's bucket name (a copy-paste in the env file), so the URI and the configured bucket
    // agree and a URL IS produced — signed with app A's key. The store must refuse it. If this
    // ever returns 200, per-app credentials are not actually scoped and the bucket-name check
    // is the only tenant boundary in the storage layer.
    let mut mixed_up = app_a.storage_config();
    mixed_up.bucket_name = app_b.name.clone();
    let cross = get_presigned_url(&mixed_up, &app_b.uri("bundle.zip"))
        .await
        .expect("presigning does not contact the store, so it succeeds");

    let (status, body) = http_get(&cross).await;
    assert_eq!(
        status,
        403,
        "app A's credentials read app B's bucket: {}",
        String::from_utf8_lossy(&body)
    );
}

// ---------------------------------------------------------------------------
// read_s3_file (the manifest path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_s3_file_returns_the_object_body() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let manifest = r#"{"bundleId":"0189","assets":{}}"#;
    bucket.put("0189/manifest.json", manifest.as_bytes()).await;

    let text = read(&bucket, &bucket.uri("0189/manifest.json"))
        .await
        .expect("read_s3_file");
    assert_eq!(text, manifest);
}

#[tokio::test]
async fn read_s3_file_errors_on_a_missing_object() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let err = read(&bucket, &bucket.uri("nope.json"))
        .await
        .expect_err("missing object must be an error");
    assert!(
        format!("{err:?}").contains("NoSuchKey"),
        "expected a NoSuchKey error, got: {err:?}"
    );
}

#[tokio::test]
async fn read_s3_file_errors_on_a_nonexistent_bucket() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    // The app is configured with a bucket that was never created — a typo in
    // `R2_BUCKET_NAME_<APP>`. The URI matches it, so the name guard passes and the call goes
    // out to the store.
    let missing = format!("{}-does-not-exist", bucket.name);
    let mut storage = bucket.storage_config();
    storage.bucket_name = missing.clone();
    let err = read_s3_file(&storage, &format!("s3://{missing}/manifest.json"))
        .await
        .expect_err("a nonexistent bucket must be an error");
    assert!(
        format!("{err:?}").contains("NoSuchBucket") || format!("{err:?}").contains("AccessDenied"),
        "expected a bucket-level error, got: {err:?}"
    );
}

#[tokio::test]
async fn read_s3_file_errors_promptly_on_an_unreachable_endpoint() {
    // Port 1 refuses immediately; this is the "R2 is down / endpoint misconfigured" path. The
    // point is that it returns an Err rather than hanging: the update-check request is waiting
    // on it, and `resolve_manifest_artifacts` can only degrade once this returns.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        read_s3_file(
            &storage_config(Some("http://127.0.0.1:1"), "key", "secret", "bucket"),
            "s3://bucket/manifest.json",
        ),
    )
    .await
    .expect("read_s3_file hung on an unreachable endpoint");
    assert!(
        result.is_err(),
        "expected a transport error, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// End to end: what a device actually receives
// ---------------------------------------------------------------------------

/// Two apps, two buckets, real MySQL, real MinIO: the whole path a device takes.
async fn spawn_two_app_server(app_a: &TestBucket, app_b: &TestBucket) -> Option<TestApp> {
    TestApp::spawn_with_storage(&[
        ("app-a", app_a.storage_config()),
        ("app-b", app_b.storage_config()),
    ])
    .await
}

fn check_url(app: &str, bundle: &str) -> String {
    format!("/{app}/hot-updater/app-version/ios/1.0.0/production/{NIL_UUID}/{bundle}")
}

#[tokio::test]
async fn update_check_hands_the_device_a_file_url_that_downloads() {
    let Some(app_a) = TestBucket::create().await else {
        return;
    };
    let Some(app_b) = TestBucket::create().await else {
        return;
    };
    let Some(app) = spawn_two_app_server(&app_a, &app_b).await else {
        return;
    };

    let id = bundle_id(1);
    app_a.put(&format!("{id}/bundle.zip"), BUNDLE_BYTES).await;
    app.seed(&SeedBundle::new(&id, "app-a").storage_uri(&app_a.uri(&format!("{id}/bundle.zip"))))
        .await;

    let body = app.get_anon(&check_url("app-a", NIL_UUID)).await.json();
    assert_eq!(body["status"], "UPDATE");
    let file_url = body["fileUrl"]
        .as_str()
        .unwrap_or_else(|| panic!("fileUrl was not a string: {body}"));

    let (status, bytes) = http_get(file_url).await;
    assert_eq!(status, 200, "the device could not download {file_url}");
    assert_eq!(bytes, BUNDLE_BYTES);
}

#[tokio::test]
async fn update_check_serves_the_manifest_and_asset_urls_from_the_apps_own_bucket() {
    let Some(app_a) = TestBucket::create().await else {
        return;
    };
    let Some(app_b) = TestBucket::create().await else {
        return;
    };
    let Some(app) = spawn_two_app_server(&app_a, &app_b).await else {
        return;
    };

    let id = bundle_id(1);
    let manifest = serde_json::json!({
        "bundleId": id,
        "assets": { "assets/logo.png": { "fileHash": "abcdef0123456789" } },
    })
    .to_string();

    app_a.put(&format!("{id}/bundle.zip"), BUNDLE_BYTES).await;
    app_a
        .put(&format!("{id}/manifest.json"), manifest.as_bytes())
        .await;
    app_a
        .put(&format!("{id}/assets/logo.png"), b"logo bytes")
        .await;

    app.seed(
        &SeedBundle::new(&id, "app-a")
            .storage_uri(&app_a.uri(&format!("{id}/bundle.zip")))
            .manifest(
                &app_a.uri(&format!("{id}/manifest.json")),
                "manifest-hash",
                &app_a.uri(&id),
            ),
    )
    .await;

    let body = app.get_anon(&check_url("app-a", NIL_UUID)).await.json();
    assert_eq!(body["status"], "UPDATE");

    let manifest_url = body["manifestUrl"].as_str().unwrap_or_else(|| {
        panic!("manifestUrl is null — the manifest could not be read from the store: {body}")
    });
    assert_eq!(
        http_get(manifest_url).await,
        (200, manifest.clone().into_bytes())
    );
    assert_eq!(body["manifestFileHash"], "manifest-hash");

    // The device has no current bundle, so every asset counts as changed.
    let asset_url = body["changedAssets"]["assets/logo.png"]["file"]["url"]
        .as_str()
        .unwrap_or_else(|| panic!("no asset url: {body}"));
    assert_eq!(http_get(asset_url).await, (200, b"logo bytes".to_vec()));
}

#[tokio::test]
async fn update_check_fails_loudly_when_a_bundle_points_at_another_apps_bucket() {
    let Some(app_a) = TestBucket::create().await else {
        return;
    };
    let Some(app_b) = TestBucket::create().await else {
        return;
    };
    let Some(app) = spawn_two_app_server(&app_a, &app_b).await else {
        return;
    };

    let id = bundle_id(1);
    app_b
        .put(&format!("{id}/bundle.zip"), b"app B's bundle")
        .await;
    // App A's row naming app B's bucket — a copy-paste in `.env`, or a mis-seeded bundle.
    app.seed(&SeedBundle::new(&id, "app-a").storage_uri(&app_b.uri(&format!("{id}/bundle.zip"))))
        .await;

    let resp = app.get_anon(&check_url("app-a", NIL_UUID)).await;

    // This used to answer 200 UPDATE with a null fileUrl. That is not a soft failure: the client
    // reads a null url as "reset to the built-in bundle", wipes every downloaded bundle and
    // reports success, and a forced update then reloads into the same answer forever. A 5xx is
    // retried harmlessly, and it is also what upstream does (`resolveFileUrl` throws).
    assert_eq!(
        resp.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a misconfigured bucket must fail the check, not tell the device to update with nothing \
         to download: {}",
        resp.text()
    );
    // And nothing about the other tenant leaks into the error.
    let body = resp.text();
    assert!(
        !body.contains(&app_b.name),
        "leaked the other bucket: {body}"
    );
}

/// The durable invariant behind the test above, checked across every shape that still answers
/// 200: **an `UPDATE` response always carries a real `fileUrl`.** Deleting this is the mistake
/// that would let the reset-to-built-in shape back in.
#[tokio::test]
async fn an_update_response_never_carries_a_null_file_url() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };

    // A healthy store, and a store that cannot be reached at all. Presigning is local signing,
    // so the second one still produces a URL — what must never happen is UPDATE with null.
    let mut unreachable = bucket.storage_config();
    unreachable.endpoint = Some("http://127.0.0.1:1".to_string());

    for (label, storage) in [
        ("healthy store", bucket.storage_config()),
        ("unreachable store", unreachable),
    ] {
        let Some(app) = TestApp::spawn_with_storage(&[("app-a", storage)]).await else {
            return;
        };
        let id = bundle_id(1);
        bucket.put(&format!("{id}/bundle.zip"), BUNDLE_BYTES).await;
        app.seed(
            &SeedBundle::new(&id, "app-a").storage_uri(&bucket.uri(&format!("{id}/bundle.zip"))),
        )
        .await;

        let resp = app.get_anon(&check_url("app-a", NIL_UUID)).await;
        assert_eq!(resp.status, StatusCode::OK, "{label}: {}", resp.text());
        let body = resp.json();
        if body["status"] == "UPDATE" {
            assert!(
                body["fileUrl"].is_string(),
                "{label}: an UPDATE must carry a fileUrl; a null one tells the device to wipe \
                 its bundles: {body}"
            );
        }
    }
}

/// The refused-connection case: the store is dead and says so immediately, so nothing here
/// depends on a timeout. Kept distinct from
/// `update_check_fails_within_the_storage_budget_when_the_store_is_silent`, which is the case a
/// timeout is what ends.
///
/// **This test asserted the opposite until the artifact failure policy was aligned with
/// upstream.** It required 200 with `manifestUrl`/`changedAssets` degraded to null, on the
/// reasoning that artifacts are only a download optimisation and a 5xx stalls a rollout.
/// Upstream propagates instead — `resolveManifestArtifacts` awaits `readStorageText` and
/// `resolveFileUrl` with no `try` — and exact compatibility was chosen over the degradation.
/// Recorded as artifact fixture cases F04/F05.
///
/// What it still guards, and what it was always really for: the request must TERMINATE rather
/// than hang, and it must never answer `UPDATE` with a null `fileUrl`, which the device reads as
/// "wipe every downloaded bundle" (see `docs/upstream-parity.md` §3.4).
#[tokio::test]
async fn update_check_fails_when_the_object_store_is_unreachable() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let mut storage = bucket.storage_config();
    storage.endpoint = Some("http://127.0.0.1:1".to_string());
    let Some(app) = TestApp::spawn_with_storage(&[("app-a", storage)]).await else {
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, "app-a")
            .storage_uri(&bucket.uri(&format!("{id}/bundle.zip")))
            .manifest(
                &bucket.uri(&format!("{id}/manifest.json")),
                "manifest-hash",
                &bucket.uri(&id),
            ),
    )
    .await;

    let resp = tokio::time::timeout(
        Duration::from_secs(60),
        app.get_anon(&check_url("app-a", NIL_UUID)),
    )
    .await
    .expect("update-check hung while the store was unreachable");

    // The manifest cannot be read, and upstream turns an unreadable manifest into a failed
    // check rather than an update with no diff.
    assert_eq!(
        resp.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreadable manifest must fail the check, as upstream does: {}",
        resp.text()
    );
    // A failed check is retried harmlessly. What must never happen is a 200 carrying an UPDATE
    // with no download URL.
    assert!(
        !resp.text().contains("\"fileUrl\":null"),
        "a failed check must not answer with a null fileUrl: {}",
        resp.text()
    );
}

/// What a device gets while the store is silent rather than dead — the case the timeout is what
/// ends. The manifest read cannot complete, so the whole request rides on the storage budget.
///
/// **The status this asserts was inverted** when the artifact failure policy was aligned with
/// upstream: it used to require 200-with-degraded-artifacts, it now requires a clean 5xx. The
/// part that has not changed, and is the whole reason this test exists, is the BOUND — the check
/// must answer within the configured storage budget instead of hanging while the peer holds the
/// socket open. Before `StorageTimeoutConfig` existed it could not answer at all.
#[tokio::test]
async fn update_check_fails_within_the_storage_budget_when_the_store_is_silent() {
    let Some(bucket) = TestBucket::create().await else {
        return;
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let silent_endpoint = format!("http://{}", listener.local_addr().unwrap());
    let accepting = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let mut storage = bucket.storage_config();
    storage.endpoint = Some(silent_endpoint);
    let Some(app) = TestApp::spawn_with_storage(&[("app-a", storage)]).await else {
        accepting.abort();
        return;
    };

    let id = bundle_id(1);
    app.seed(
        &SeedBundle::new(&id, "app-a")
            .storage_uri(&bucket.uri(&format!("{id}/bundle.zip")))
            .manifest(
                &bucket.uri(&format!("{id}/manifest.json")),
                "manifest-hash",
                &bucket.uri(&id),
            ),
    )
    .await;

    let budget = StorageTimeoutConfig::default().operation_timeout + Duration::from_secs(20);
    let resp = tokio::time::timeout(budget, app.get_anon(&check_url("app-a", NIL_UUID)))
        .await
        .unwrap_or_else(|_| {
            panic!("the update-check did not return within {budget:?} against a silent store")
        });
    accepting.abort();

    assert_eq!(
        resp.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreadable manifest must fail the check, as upstream does: {}",
        resp.text()
    );
    assert!(
        !resp.text().contains("\"fileUrl\":null"),
        "a failed check must not answer with a null fileUrl: {}",
        resp.text()
    );
}

/// Regression lock. The name describes the defect this holds shut, not the current behaviour.
///
/// Needs no container: the "store" here is a local listener that accepts the connection and
/// then never answers, which is how a degraded R2/S3 endpoint behaves (a dead one refuses
/// instantly and is covered by `read_s3_file_errors_promptly_on_an_unreachable_endpoint`).
///
/// This used to hang indefinitely — measured at over 45 s and still going — because the AWS SDK
/// applies no operation timeout unless one is configured, and with it hung the device's
/// update-check: an Axum handler awaiting this holds its connection and task for as long as the
/// peer keeps the socket open. `StorageTimeoutConfig` now bounds it.
///
/// The bound below is the configured overall budget plus generous slack, so this test asserts
/// the contract the configuration actually promises rather than a number of its own. In practice
/// the read timeout fires first, per attempt, and the SDK's retries bring the total to ~17 s at
/// the default settings.
#[tokio::test]
async fn bug_read_s3_file_hangs_forever_against_a_black_holing_endpoint() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    // Accept and hold: connections must stay open and unanswered.
    let accepting = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let budget = StorageTimeoutConfig::default().operation_timeout + Duration::from_secs(15);
    let result = tokio::time::timeout(
        budget,
        read_s3_file(
            &storage_config(Some(&endpoint), "key", "secret", "bucket"),
            "s3://bucket/manifest.json",
        ),
    )
    .await;
    accepting.abort();

    let Ok(result) = result else {
        panic!(
            "read_s3_file did not return within {budget:?} against an endpoint that accepts but \
             never answers; without an operation timeout the device's update-check hangs with it"
        );
    };
    assert!(
        result.is_err(),
        "a silent endpoint must surface as an error"
    );
}

/// The gap the SDK's own configuration does not close, and the reason `read_s3_file` bounds the
/// body collection itself.
///
/// This server answers immediately and then drips one byte every 200 ms — five bytes a second,
/// comfortably above the 1 B/s floor the SDK's stalled-stream protection enforces, so that
/// protection never trips. The response headers arrive, so the SDK considers the operation
/// complete and its operation timeout is already spent. Only the explicit bound around
/// `body.collect()` ends this. Verified before the bound existed: the transfer ran for as long
/// as the peer kept dripping.
#[tokio::test]
async fn a_body_that_trickles_forever_is_cut_off_at_the_operation_budget() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let serving = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // A body far larger than anything that will be sent, so the peer never
                // completes and the connection is never closed.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n")
                    .await;
                while socket.write_all(b"x").await.is_ok() {
                    let _ = socket.flush().await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
        }
    });

    let budget = StorageTimeoutConfig::default().operation_timeout + Duration::from_secs(15);
    let result = tokio::time::timeout(
        budget,
        read_s3_file(
            &storage_config(Some(&endpoint), "key", "secret", "bucket"),
            "s3://bucket/manifest.json",
        ),
    )
    .await;
    serving.abort();

    let Ok(result) = result else {
        panic!("read_s3_file streamed a trickling body past {budget:?} without giving up")
    };
    let err = result.expect_err("a body that never finishes must surface as an error");
    assert!(
        err.to_string().contains("Timed out"),
        "expected the body-download timeout, got: {err}"
    );
}

/// One client per storage configuration, measured rather than asserted.
///
/// Building an `aws_sdk_s3::Client` constructs a fresh HTTP connector and with it a fresh
/// connection pool, so a client per call meant a new TCP handshake per call — on the device hot
/// path, N+3 times for a manifest with N changed assets. `client_for` now caches by
/// configuration, so consecutive calls share the pool and the second one reuses the connection.
///
/// The counter below is the actual number of accepted TCP connections. Measured on this test
/// against the previous code (cache bypassed): 2. With the cache: 1.
#[tokio::test]
async fn consecutive_reads_reuse_one_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // A minimal keep-alive HTTP server: answers every request on a connection with the same
    // body, and keeps the connection open so it CAN be reused. If it closed after one response,
    // this test would measure the server's behaviour instead of the client's.
    let counter = accepted.clone();
    let serving = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let body = b"{}";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                                body.len()
                            );
                            if socket.write_all(response.as_bytes()).await.is_err()
                                || socket.write_all(body).await.is_err()
                            {
                                break;
                            }
                            let _ = socket.flush().await;
                        }
                    }
                }
            });
        }
    });

    let storage = storage_config(Some(&endpoint), "key", "secret", "bucket");
    for _ in 0..2 {
        read_s3_file(&storage, "s3://bucket/manifest.json")
            .await
            .expect("read");
    }
    serving.abort();

    assert_eq!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "two reads with the same storage configuration must share one client, and therefore one \
         connection"
    );
}
