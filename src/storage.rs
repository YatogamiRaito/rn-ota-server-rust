use crate::config::{AppStorageConfig, PresignConfig, StorageTimeoutConfig};
use anyhow::{anyhow, Result};
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::presigning::PresigningConfig;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static TIMEOUTS: OnceLock<StorageTimeoutConfig> = OnceLock::new();
static PRESIGN: OnceLock<PresignConfig> = OnceLock::new();

/// Publish the validated timeout configuration to this module. Called once by
/// `Config::from_env` at startup.
///
/// The storage functions are reached from the request path with no access to `Config` (their
/// callers in `src/routes/check.rs` pass loose credential arguments), so the value travels
/// through this cell rather than through an argument. Later calls are ignored: the first one
/// wins, and anything that never calls it — tests, or a library user — gets
/// [`StorageTimeoutConfig::default`], which is bounded too. There is deliberately no way to
/// end up with no timeouts at all.
pub fn install_timeout_config(config: StorageTimeoutConfig) {
    let _ = TIMEOUTS.set(config);
}

fn timeouts() -> &'static StorageTimeoutConfig {
    TIMEOUTS.get_or_init(StorageTimeoutConfig::default)
}

/// Publish the validated presign lifetime. Called once by `Config::from_env` at startup; see
/// [`install_timeout_config`] for why the value travels through a cell rather than an argument.
pub fn install_presign_config(config: PresignConfig) {
    let _ = PRESIGN.set(config);
}

fn presign_config() -> &'static PresignConfig {
    PRESIGN.get_or_init(PresignConfig::default)
}

/// The SDK needs an `AsyncSleep` to implement any timeout, and a client built from a bare
/// `aws_sdk_s3::config::Builder::new()` — as this module does, deliberately, to avoid the
/// environment/IMDS probing `aws_config` performs — has none installed. Without it a
/// `TimeoutConfig` is accepted and then silently never fires, which is how "we set timeouts"
/// could still mean "waits forever". Tokio is already the runtime; this just hands it over.
#[derive(Debug, Clone)]
struct TokioSleep;

impl aws_sdk_s3::config::AsyncSleep for TokioSleep {
    fn sleep(&self, duration: Duration) -> aws_sdk_s3::config::Sleep {
        aws_sdk_s3::config::Sleep::new(tokio::time::sleep(duration))
    }
}

/// Split an `s3://<bucket>/<key>` URI into its two parts.
///
/// The key is returned **exactly as written**, byte for byte. That is the whole point of doing
/// this by hand: S3 object keys are opaque byte strings in which spaces, non-ASCII characters,
/// `?`, `#`, `%`, empty segments and literal `.`/`..` segments are all legal, and the AWS SDK
/// percent-encodes the key it is given when it builds and signs the request.
///
/// This used to delegate to `url::Url` and hand `url.path()` on, which applies the WHATWG URL
/// path rules: a space came back as `%20`, non-ASCII came back percent-encoded, `?`/`#`
/// silently truncated the key, repeated leading slashes were collapsed and `..` segments were
/// resolved away. The SDK then encoded that result a second time — `%20` became `%2520` — and
/// the request addressed an object that did not exist. React Native asset filenames containing
/// a space are ordinary, so this reached devices as a 404 in place of an asset.
/// `tests/storage_uri_tests.rs` and `tests/storage_integration_tests.rs` lock both halves down.
///
/// Only the bucket is validated, and only for characters that cannot appear in a bucket name
/// and therefore mean the URI is malformed rather than unusual.
pub fn parse_s3_uri(uri: &str) -> Option<(String, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("s3") {
        return None;
    }

    // Only the FIRST slash separates bucket from key; every later one belongs to the key and
    // stays a path separator (the SDK does not escape `/` inside a key).
    let (bucket, key) = match rest.split_once('/') {
        Some((bucket, key)) => (bucket, key),
        None => (rest, ""),
    };

    if bucket.is_empty() || bucket.contains(|c: char| "@:?#%\\ \t\n\r".contains(c)) {
        return None;
    }

    Some((bucket.to_string(), key.to_string()))
}

/// Reject a URI whose bucket is not the one this app is configured with, before any network
/// call. This is the tenant boundary of the storage layer: a `bundles` row naming another
/// app's bucket must never be turned into a download link.
fn resolve_key(bucket_name: &str, storage_uri: &str) -> Result<String> {
    let (uri_bucket, key) = parse_s3_uri(storage_uri)
        .ok_or_else(|| anyhow!("Invalid S3 storage URI: {}", storage_uri))?;

    if uri_bucket != bucket_name {
        return Err(anyhow!(
            "Bucket name mismatch: expected '{}', but found '{}'",
            bucket_name,
            uri_bucket
        ));
    }

    Ok(key)
}

/// Everything about an [`AppStorageConfig`] that changes the client — deliberately NOT the
/// bucket, which is per-operation, so two buckets reached with the same credentials share one
/// client and one connection pool.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    endpoint: Option<String>,
    region: String,
    force_path_style: bool,
    access_key_id: String,
    secret_access_key: String,
}

impl From<&AppStorageConfig> for ClientKey {
    fn from(storage: &AppStorageConfig) -> Self {
        Self {
            endpoint: storage.endpoint.clone(),
            region: storage.region.clone(),
            force_path_style: storage.force_path_style,
            access_key_id: storage.access_key_id.clone(),
            secret_access_key: storage.secret_access_key.clone(),
        }
    }
}

static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, aws_sdk_s3::Client>>> = OnceLock::new();

/// One client per distinct storage configuration, reused for the life of the process.
///
/// Building a client is not free — it constructs a fresh HTTP connector, and with it a fresh
/// connection pool, so a client per call meant a new TCP (and TLS) handshake per call and no
/// connection reuse at all. An update-check for a bundle with N changed assets makes N+3 of
/// these calls, on the device hot path.
///
/// The map is keyed on configuration, never on anything from a request, so it is bounded by the
/// number of apps in `APPS`. `aws_sdk_s3::Client` is internally `Arc`-based, so the clone here
/// is cheap and shares the pool. The lock is held only for the lookup/insert, never across an
/// await.
fn client_for(storage: &AppStorageConfig) -> aws_sdk_s3::Client {
    let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = ClientKey::from(storage);

    // A poisoned lock here would mean a panic while holding it, which this critical section
    // cannot produce; recovering keeps a cache panic from taking the update-check path with it.
    let mut map = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = map.get(&key) {
        return existing.clone();
    }
    let client = build_client(storage);
    map.insert(key, client.clone());
    client
}

fn build_client(storage: &AppStorageConfig) -> aws_sdk_s3::Client {
    let credentials = aws_credential_types::Credentials::new(
        storage.access_key_id.clone(),
        storage.secret_access_key.clone(),
        None,
        None,
        "StaticCredentials",
    );

    let cfg = timeouts();
    let mut config_builder = aws_sdk_s3::config::Builder::new()
        .credentials_provider(credentials)
        // The signing region. `auto` is R2's convention and stays the default; AWS S3 needs
        // its real region both in the credential scope and, with no endpoint override, in the
        // hostname the SDK builds.
        .region(aws_config::Region::new(storage.region.clone()))
        .force_path_style(storage.force_path_style)
        .sleep_impl(TokioSleep)
        // Without this the SDK applies no operation timeout at all and a silent endpoint
        // blocks the caller forever. `operation_timeout` bounds the retried whole,
        // `attempt_timeout` each individual try, `read_timeout` the wait for the first byte
        // and `connect_timeout` the socket setup.
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(cfg.connect_timeout)
                .read_timeout(cfg.read_timeout)
                .operation_attempt_timeout(cfg.attempt_timeout)
                .operation_timeout(cfg.operation_timeout)
                .build(),
        );

    if let Some(ep) = &storage.endpoint {
        // Cloudflare R2 (S3-compatible API) and MinIO are addressed through an endpoint, and
        // both want path-style addressing (https://<host>/<bucket>/<key>) — see
        // `AppStorageConfig::force_path_style`, which defaults to exactly that whenever an
        // endpoint is set. With no endpoint the SDK builds an AWS hostname from the region and
        // its own virtual-hosted default applies.
        config_builder = config_builder.endpoint_url(ep);
    }

    aws_sdk_s3::Client::from_conf(config_builder.build())
}

pub async fn get_presigned_url(storage: &AppStorageConfig, storage_uri: &str) -> Result<String> {
    let key = resolve_key(&storage.bucket_name, storage_uri)?;
    let client = client_for(storage);

    // Purely local signing — no request is sent, so the timeouts above never apply here.
    // The lifetime is validated at startup (`PresignConfig::from_env`), so `expires_in` cannot
    // fail here on a value an operator supplied.
    let presigned_req = client
        .get_object()
        .bucket(&storage.bucket_name)
        .key(key)
        .presigned(PresigningConfig::expires_in(presign_config().expires_in)?)
        .await?;

    Ok(presigned_req.uri().to_string())
}

pub async fn read_s3_file(storage: &AppStorageConfig, storage_uri: &str) -> Result<String> {
    let key = resolve_key(&storage.bucket_name, storage_uri)?;
    let client = client_for(storage);

    let response = client
        .get_object()
        .bucket(&storage.bucket_name)
        .key(key)
        .send()
        .await?;

    // The SDK's operation timeout covers `send()`, which for a streaming output like GetObject
    // returns as soon as the response HEADERS arrive; collecting the body happens outside it.
    // The SDK's stalled-stream protection does watch the body, but it is a THROUGHPUT floor
    // (1 B/s), not a deadline: a peer trickling a few bytes a second satisfies it forever.
    // Measured — a server dripping one byte every 200 ms streams indefinitely under stalled-
    // stream protection and is cut off here instead. So this wrapper is not a substitute for
    // the SDK's timeouts, it closes the one gap they leave, and it reuses the same
    // operation-level budget so there is still a single number to reason about.
    let data = tokio::time::timeout(timeouts().operation_timeout, response.body.collect())
        .await
        .map_err(|_| {
            anyhow!(
                "Timed out after {:?} while downloading the body of {}",
                timeouts().operation_timeout,
                storage_uri
            )
        })??
        .into_bytes();

    let text = String::from_utf8(data.to_vec())?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_s3_uri() {
        let uri = "s3://my-bucket/path/to/my-bundle.zip";
        let (bucket, key) = parse_s3_uri(uri).unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "path/to/my-bundle.zip");
    }
}
