//! The parts of `src/storage.rs` that need no object store: `parse_s3_uri`, the bucket-name
//! guard, and the shape of a presigned URL. Pure and offline — runs everywhere.
//!
//! The end-to-end counterpart (does the presigned URL actually download the object?) lives in
//! `tests/storage_integration_tests.rs` and needs MinIO.

use rn_ota_server_rust::config::{AppStorageConfig, DEFAULT_STORAGE_REGION};
use rn_ota_server_rust::storage::{get_presigned_url, parse_s3_uri, read_s3_file};

/// The configuration an app carries, with the defaults `Config::from_env` applies when nothing
/// sets a region or an addressing style.
fn storage(endpoint: Option<&str>, bucket: &str) -> AppStorageConfig {
    AppStorageConfig {
        endpoint: endpoint.map(str::to_string),
        access_key_id: "AKIAEXAMPLE".to_string(),
        secret_access_key: "secret".to_string(),
        bucket_name: bucket.to_string(),
        region: DEFAULT_STORAGE_REGION.to_string(),
        force_path_style: endpoint.is_some(),
    }
}

fn parsed(uri: &str) -> (String, String) {
    parse_s3_uri(uri).unwrap_or_else(|| panic!("expected {uri:?} to parse"))
}

#[test]
fn splits_bucket_from_key() {
    assert_eq!(
        parsed("s3://my-bucket/bundle.zip"),
        ("my-bucket".to_string(), "bundle.zip".to_string())
    );
}

#[test]
fn keeps_slashes_inside_the_key() {
    // The CLI writes bundles as `s3://<bucket>/<bundle-id>/bundle.zip`, and assets go several
    // levels deeper (`.../assets/sha256/ab/<hash>.png`). Only the FIRST slash separates the
    // bucket from the key; every later one belongs to the key.
    assert_eq!(
        parsed("s3://my-bucket/0189/assets/sha256/ab/abcd.png"),
        (
            "my-bucket".to_string(),
            "0189/assets/sha256/ab/abcd.png".to_string()
        )
    );
}

#[test]
fn accepts_legal_key_characters_that_need_no_escaping() {
    // React Native asset filenames routinely contain these.
    for key in [
        "assets/icon@2x.png",
        "assets/a_b-c.d.png",
        "assets/(1).png",
        "assets/a+b.png",
        "assets/a~b.png",
        "assets/!$&'()*,;=.png",
    ] {
        let (bucket, parsed_key) = parsed(&format!("s3://my-bucket/{key}"));
        assert_eq!(bucket, "my-bucket");
        assert_eq!(
            parsed_key, key,
            "the key must survive parsing byte for byte: {key:?}"
        );
    }
}

#[test]
fn scheme_is_case_insensitive() {
    // URL scheme comparison is case-insensitive per RFC 3986; `url` lowercases it for us.
    assert_eq!(
        parsed("S3://my-bucket/bundle.zip"),
        ("my-bucket".to_string(), "bundle.zip".to_string())
    );
}

#[test]
fn rejects_non_s3_and_malformed_input() {
    // Anything that is not an `s3://` URI must be refused rather than half-interpreted:
    // `get_presigned_url` turns `None` into an error and the caller degrades (null fileUrl).
    for uri in [
        "",
        "   ",
        "s3:/my-bucket/bundle.zip",
        "s3://",
        "s3:///bundle.zip", // empty host
        "https://my-bucket.s3.amazonaws.com/bundle.zip",
        "http://my-bucket/bundle.zip",
        "file:///tmp/bundle.zip",
        "/my-bucket/bundle.zip",
        "my-bucket/bundle.zip",
        "not a uri at all",
    ] {
        assert!(
            parse_s3_uri(uri).is_none(),
            "{uri:?} must not parse as an s3:// URI, got {:?}",
            parse_s3_uri(uri)
        );
    }
}

#[test]
fn a_bucket_with_no_key_yields_an_empty_key() {
    // Not an error today. It becomes one downstream: S3 rejects GetObject with an empty key.
    // Pinned so a change in this shape is a deliberate one.
    assert_eq!(
        parsed("s3://my-bucket"),
        ("my-bucket".to_string(), String::new())
    );
    assert_eq!(
        parsed("s3://my-bucket/"),
        ("my-bucket".to_string(), String::new())
    );
}

/// FAILING — genuine defect, reported rather than fixed (see the report).
///
/// `parse_s3_uri` runs the URI through `url::Url` and hands `url.path()` on as the S3 key. But
/// `Url` is not a transparent container: it applies the WHATWG URL path rules, so
///
///   * a space becomes `%20`, and any non-ASCII byte becomes its percent-encoded form;
///   * `?` and `#` start a query/fragment and silently TRUNCATE the key;
///   * repeated leading slashes are collapsed (`trim_start_matches` strips all of them);
///   * `.`/`..` segments are resolved away.
///
/// S3 object keys are opaque byte strings in which every one of those characters is legal, and
/// the AWS SDK percent-encodes the key it is given — so a key that arrives here already encoded
/// gets encoded a second time (`%20` → `%2520`) and addresses a different object.
///
/// This is reachable in production: `resolve_manifest_asset_storage_uri` in `src/routes/check.rs`
/// builds asset URIs with `Url::set_path`, which percent-encodes, so an asset filename with a
/// space or a non-ASCII character (say `assets/görsel.png`) cannot be addressed correctly in
/// EITHER form — raw or pre-encoded.
///
/// `tests/storage_integration_tests.rs::bug_presigned_url_for_a_key_with_a_space_hits_the_wrong_object`
/// shows the same defect against a real object store: the URL is well-formed and correctly
/// signed, and returns 404.
#[test]
fn parse_s3_uri_preserves_the_key_verbatim() {
    let cases = [
        ("assets/my file.png", "a space must stay a space"),
        ("assets/görsel.png", "non-ASCII must stay non-ASCII"),
        ("assets/50%.png", "a literal percent must not be re-read"),
        ("assets/a?b.png", "a question mark is a legal key character"),
        ("assets/a#b.png", "a hash is a legal key character"),
        (
            "/leading-slash.png",
            "an empty first segment is a legal key",
        ),
        ("a/../b.png", "a literal `..` segment must not be resolved"),
    ];

    let mut broken = Vec::new();
    for (key, why) in cases {
        match parse_s3_uri(&format!("s3://my-bucket/{key}")) {
            Some((_, got)) if got == key => {}
            other => broken.push(format!("  {key:?} -> {other:?} ({why})")),
        }
    }

    assert!(
        broken.is_empty(),
        "parse_s3_uri mangled keys that S3 considers legal:\n{}",
        broken.join("\n")
    );
}

/// Both storage entry points must refuse a URI naming a bucket other than the app's own, and
/// must do it before touching the network — the check is the tenant boundary of this layer.
#[tokio::test]
async fn both_entry_points_refuse_a_uri_for_another_bucket() {
    // The endpoint is unroutable on purpose: if either function ever performed the request
    // before checking, this test would fail on a transport error instead of the guard message.
    let endpoint = Some("http://127.0.0.1:1");

    let presign_err = get_presigned_url(
        &storage(endpoint, "app-a-bucket"),
        "s3://app-b-bucket/bundle.zip",
    )
    .await
    .expect_err("cross-bucket presign must fail");
    assert!(
        presign_err.to_string().contains("Bucket name mismatch"),
        "unexpected error: {presign_err}"
    );

    let read_err = read_s3_file(
        &storage(endpoint, "app-a-bucket"),
        "s3://app-b-bucket/manifest.json",
    )
    .await
    .expect_err("cross-bucket read must fail");
    assert!(
        read_err.to_string().contains("Bucket name mismatch"),
        "unexpected error: {read_err}"
    );
}

/// The R2 default: no region configured, so `auto` is used — Cloudflare's convention — and the
/// URL is virtual-hosted style because nothing set an endpoint. Existing R2 deployments were
/// unaffected by the region becoming configurable, and this pins that.
#[tokio::test]
async fn the_default_region_is_still_r2s_auto() {
    let raw = get_presigned_url(&storage(None, "my-bucket"), "s3://my-bucket/b.zip")
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap();

    let credential = url
        .query_pairs()
        .find(|(k, _)| k == "X-Amz-Credential")
        .map(|(_, v)| v.into_owned())
        .expect("no X-Amz-Credential");
    assert!(
        credential.contains("/auto/s3/aws4_request"),
        "credential scope: {credential}"
    );
}

/// The fix for the AWS half of the story, verified against the SDK's own URL and signature
/// construction — NOT against live AWS, which no test here can reach.
///
/// With the region hardcoded to `auto` and no endpoint, the SDK built
/// `my-bucket.s3.auto.amazonaws.com`: there is no AWS region called `auto`, so that hostname
/// does not resolve and stock AWS S3 could not be used at all, though README advertised it.
/// Setting `R2_ENDPOINT` to a real AWS endpoint did not rescue it either, because the credential
/// scope still said `/auto/s3/` and AWS rejects a scope that does not name the endpoint's region.
///
/// With a real region configured the SDK produces the regional virtual-hosted hostname and a
/// credential scope naming that region — the two things AWS checks. `force_path_style` defaults
/// to false here because no endpoint is set, which is what AWS prefers; path style is its legacy
/// addressing mode.
#[tokio::test]
async fn an_aws_region_produces_a_regional_host_and_credential_scope() {
    let mut cfg = storage(None, "my-bucket");
    cfg.region = "eu-central-1".to_string();

    let raw = get_presigned_url(&cfg, "s3://my-bucket/b.zip")
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap();

    assert_eq!(
        url.host_str(),
        Some("my-bucket.s3.eu-central-1.amazonaws.com")
    );
    assert_eq!(
        url.path(),
        "/b.zip",
        "virtual-hosted style keeps the bucket out of the path"
    );
    let credential = url
        .query_pairs()
        .find(|(k, _)| k == "X-Amz-Credential")
        .map(|(_, v)| v.into_owned())
        .expect("no X-Amz-Credential");
    assert!(
        credential.contains("/eu-central-1/s3/aws4_request"),
        "the credential scope must name the configured region: {credential}"
    );
}

/// The addressing style follows the endpoint by default, and can be overridden either way.
///
/// This matters per backend: MinIO cannot serve virtual-hosted style without wildcard DNS, R2 is
/// documented for path style, and AWS treats path style as the legacy mode. The default —
/// "endpoint set means path style" — gives each of the three what it wants without configuration.
#[tokio::test]
async fn addressing_style_follows_the_endpoint_and_can_be_overridden() {
    // Endpoint set (R2/MinIO): the bucket belongs in the path.
    let cfg = storage(
        Some("https://account.r2.cloudflarestorage.com"),
        "my-bucket",
    );
    let raw = get_presigned_url(&cfg, "s3://my-bucket/b.zip")
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap();
    assert_eq!(url.host_str(), Some("account.r2.cloudflarestorage.com"));
    assert_eq!(url.path(), "/my-bucket/b.zip");

    // Same endpoint, addressing style overridden: the bucket moves into the hostname.
    let mut cfg = storage(
        Some("https://account.r2.cloudflarestorage.com"),
        "my-bucket",
    );
    cfg.force_path_style = false;
    let raw = get_presigned_url(&cfg, "s3://my-bucket/b.zip")
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).unwrap();
    assert_eq!(
        url.host_str(),
        Some("my-bucket.account.r2.cloudflarestorage.com")
    );
    assert_eq!(url.path(), "/b.zip");
}
