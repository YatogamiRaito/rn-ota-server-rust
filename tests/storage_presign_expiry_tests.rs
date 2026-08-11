//! That a configured presign lifetime is the one the signature actually carries.
//!
//! This lives in its own test binary on purpose. `install_presign_config` writes to a
//! process-global `OnceLock` (the storage functions are reached from the request path with no
//! access to `Config`), so the first writer wins for the whole process. Setting a non-default
//! value inside the main storage suite would leak into every other test there and make the
//! result depend on scheduling order. A separate binary is a separate process, which is what
//! makes this safe under `cargo test`'s parallelism without any shared mutable state.
//!
//! Needs no object store: presigning is local signing.

use rn_ota_server_rust::config::{AppStorageConfig, PresignConfig, DEFAULT_STORAGE_REGION};
use rn_ota_server_rust::storage::{get_presigned_url, install_presign_config};
use std::time::Duration;

const LIFETIME_SECS: u64 = 120;

#[tokio::test]
async fn a_configured_lifetime_reaches_the_signature() {
    install_presign_config(PresignConfig {
        expires_in: Duration::from_secs(LIFETIME_SECS),
    });

    let storage = AppStorageConfig {
        endpoint: Some("https://account.r2.cloudflarestorage.com".to_string()),
        access_key_id: "AKIAEXAMPLE".to_string(),
        secret_access_key: "secret".to_string(),
        bucket_name: "my-bucket".to_string(),
        region: DEFAULT_STORAGE_REGION.to_string(),
        force_path_style: true,
    };

    let raw = get_presigned_url(&storage, "s3://my-bucket/bundle.zip")
        .await
        .expect("presign");
    let url = url::Url::parse(&raw).expect("not a URL");
    let expires = url
        .query_pairs()
        .find(|(k, _)| k == "X-Amz-Expires")
        .map(|(_, v)| v.into_owned())
        .expect("no X-Amz-Expires");

    assert_eq!(
        expires,
        LIFETIME_SECS.to_string(),
        "the configured lifetime must be the one signed into the URL, not the default: {raw}"
    );
    assert_ne!(
        LIFETIME_SECS,
        PresignConfig::default().expires_in.as_secs(),
        "this test would prove nothing if the value it configures were the default"
    );
}
