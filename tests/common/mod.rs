//! Shared harness for the MySQL- and object-storage-backed integration tests.
//!
//! # How these tests are gated
//!
//! Everything in `tests/api_integration_tests.rs` and `tests/check_integration_tests.rs`
//! needs a real MySQL 8 server. `tests/storage_integration_tests.rs` needs an
//! S3-compatible object store (a MinIO container). By default the harness starts both
//! with testcontainers, which needs a working Docker daemon.
//!
//! CONTRIBUTING.md promises that a plain `cargo test` works on any machine, so the
//! harness *skips* (returns `None` from [`TestApp::spawn`], and the test returns early)
//! when no MySQL backend can be obtained. The knobs:
//!
//! | Env var                       | Effect                                                                   |
//! | ----------------------------- | ------------------------------------------------------------------------ |
//! | *(none)*                      | Try Docker. If it is unavailable, print a notice and skip.               |
//! | `OTA_REQUIRE_DOCKER_TESTS=1`  | A backend is mandatory: failing to get one **panics**. Use this in CI.   |
//! | `OTA_TEST_MYSQL_URL=<url>`    | Use an already-running server instead of Docker. No trailing `/dbname` — |
//! |                               | e.g. `mysql://root@127.0.0.1:3306`. Needs `CREATE DATABASE` rights.     |
//! | `OTA_TEST_MYSQL_TAG=<tag>`    | Docker image tag to run (default `8.0`).                                 |
//!
//! The object store has the same three knobs, named to match:
//!
//! | Env var                             | Effect                                                             |
//! | ----------------------------------- | ------------------------------------------------------------------ |
//! | *(none)*                            | Try Docker (MinIO). If it is unavailable, print a notice and skip. |
//! | `OTA_REQUIRE_DOCKER_TESTS=1`        | Same flag as MySQL: no store means **panic**, not skip.            |
//! | `OTA_TEST_S3_ENDPOINT=<url>`        | Use an already-running S3-compatible server, e.g.                  |
//! |                                     | `http://127.0.0.1:9000`. Needs `CreateBucket` rights.              |
//! | `OTA_TEST_S3_ACCESS_KEY_ID=<id>`    | Credentials for that server (default `minioadmin`).                |
//! | `OTA_TEST_S3_SECRET_ACCESS_KEY=<k>` | Credentials for that server (default `minioadmin`).                |
//! | `OTA_TEST_S3_TAG=<tag>`             | MinIO image tag to run (default: the one testcontainers pins).     |
//!
//! One thing an external endpoint cannot do: [`TestBucket::create_with_own_user`] provisions a
//! MinIO user whose policy only covers its own bucket, which is done by running `mc` *inside*
//! the container. Against an external endpoint that returns `None` and the per-app credential
//! isolation test skips itself with a notice (it still runs in CI, which uses the container).
//!
//! So CI must run the suite as:
//!
//! ```text
//! OTA_REQUIRE_DOCKER_TESTS=1 cargo test --all-features
//! ```
//!
//! Without `OTA_REQUIRE_DOCKER_TESTS=1` a Docker outage would turn into a silent green
//! build, which is exactly the failure mode the flag exists to prevent.
//!
//! # Isolation
//!
//! One MySQL container is shared by every test in a binary (held through a `Weak`, so it
//! is removed once the last test using it finishes), but **each test gets its own freshly
//! created database** and runs the real `migrations/` against it. There is therefore no
//! shared mutable state between tests and they are safe under `cargo test`'s parallelism.
//!
//! `Config` is built by hand rather than through `Config::from_env()` precisely so that no
//! test ever touches process-global environment variables. Nothing here calls
//! `std::env::set_var`; the three `OTA_*` variables above are read-only, read once. That is
//! what makes the suite safe to run at any `--test-threads` value — there is no global
//! mutable state to race over, and per-test databases mean no shared rows either.
//!
//! One caveat on `--test-threads=1`: with no overlap between tests the shared container's
//! last reference is dropped at the end of every test, so a fresh container is started for
//! the next one (~7 s each). The suite still passes, it just takes minutes instead of
//! seconds. Use the default parallelism, or point `OTA_TEST_MYSQL_URL` at a long-lived
//! server (which is also the cheapest way to run this in CI).

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use rn_ota_server_rust::{
    config::{AppConfig, AppStorageConfig, Config, DEFAULT_STORAGE_REGION},
    db::init_db,
    routes::configure_routes,
    AppState,
};
use sqlx::{Connection, Executor, MySqlConnection, MySqlPool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use testcontainers_modules::minio::MinIO;
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::testcontainers::core::{CmdWaitFor, ExecCommand};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Bucket name every test app is configured with. Seeded `storage_uri`s must use it or
/// presigning fails and `fileUrl` comes back null.
pub const BUCKET: &str = "test-bucket";
/// Endpoint every test app is configured with. Presigning is a purely local signing
/// operation, so nothing is ever sent to this host.
pub const ENDPOINT: &str = "https://s3.example.invalid";
pub const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// Deterministic, lexicographically ordered, 36-character (CHAR(36)) bundle ids.
///
/// `bundle_id(2) > bundle_id(1) > NIL_UUID` holds identically for the `ORDER BY id DESC`
/// MySQL applies under `ascii_general_ci` and for the byte-wise `str` comparison
/// `decide_update` applies in Rust. The two agree here because these ids — like the UUIDs
/// the CLI generates — are drawn from the lowercase-hex-and-dash alphabet, over which
/// `ascii_general_ci` (which differs from byte order only by folding case) and byte order
/// are the same ordering. Do not introduce upper-case ids into fixtures: that is exactly
/// the region where the two orderings diverge, and it is the documented deviation from
/// upstream's ICU `localeCompare` ordering.
pub fn bundle_id(seq: u32) -> String {
    format!("00000000-0000-0000-0000-{seq:012}")
}

/// An `AppStorageConfig` with the same defaults `Config::from_env` applies when nothing
/// configures a region or an addressing style: R2's `auto`, and path-style addressing exactly
/// when an endpoint is set.
pub fn storage_config(
    endpoint: Option<&str>,
    access_key_id: &str,
    secret_access_key: &str,
    bucket_name: &str,
) -> AppStorageConfig {
    AppStorageConfig {
        endpoint: endpoint.map(str::to_string),
        access_key_id: access_key_id.to_string(),
        secret_access_key: secret_access_key.to_string(),
        bucket_name: bucket_name.to_string(),
        region: DEFAULT_STORAGE_REGION.to_string(),
        force_path_style: endpoint.is_some(),
    }
}

pub fn auth_token(app: &str) -> String {
    format!("secret-token-for-{app}")
}

fn require_backend() -> bool {
    matches!(
        std::env::var("OTA_REQUIRE_DOCKER_TESTS").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// A MySQL server the tests can create databases on: either a container we started or an
/// externally supplied one.
struct Backend {
    /// `mysql://user:pass@host:port`, without a database component.
    base_url: String,
    _container: Option<ContainerAsync<Mysql>>,
}

impl Backend {
    async fn create_database(&self) -> Result<String, sqlx::Error> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "ota_test_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut conn = MySqlConnection::connect(&format!("{}/mysql", self.base_url)).await?;
        conn.execute(format!("CREATE DATABASE `{name}`").as_str())
            .await?;
        conn.close().await?;
        Ok(name)
    }
}

static BACKEND: LazyLock<Mutex<Weak<Backend>>> = LazyLock::new(|| Mutex::new(Weak::new()));
static BACKEND_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

async fn backend() -> Option<Arc<Backend>> {
    if BACKEND_UNAVAILABLE.load(Ordering::Relaxed) {
        return None;
    }

    let mut slot = BACKEND.lock().await;
    if let Some(existing) = slot.upgrade() {
        return Some(existing);
    }

    if let Ok(url) = std::env::var("OTA_TEST_MYSQL_URL") {
        let backend = Arc::new(Backend {
            base_url: url.trim_end_matches('/').to_string(),
            _container: None,
        });
        *slot = Arc::downgrade(&backend);
        return Some(backend);
    }

    let tag = std::env::var("OTA_TEST_MYSQL_TAG").unwrap_or_else(|_| "8.0".to_string());
    // The default limit of 151 connections is easy to reach: every test builds its own
    // pool (max 10) and the pools of finished tests linger until their idle timeout.
    let started = Mysql::default()
        .with_tag(tag)
        .with_cmd(["mysqld", "--max-connections=500"])
        .start()
        .await;

    let container = match started {
        Ok(c) => c,
        Err(err) => {
            if require_backend() {
                panic!(
                    "OTA_REQUIRE_DOCKER_TESTS is set but no MySQL container could be started: {err}"
                );
            }
            BACKEND_UNAVAILABLE.store(true, Ordering::Relaxed);
            eprintln!(
                "SKIPPING MySQL integration tests: could not start a MySQL container ({err}). \
                 Start Docker, or set OTA_TEST_MYSQL_URL to an existing server. \
                 Set OTA_REQUIRE_DOCKER_TESTS=1 to make this a hard failure."
            );
            return None;
        }
    };

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("container port");

    let backend = Arc::new(Backend {
        base_url: format!("mysql://root@{host}:{port}"),
        _container: Some(container),
    });
    *slot = Arc::downgrade(&backend);
    Some(backend)
}

/// A fully wired server: real migrated MySQL database + the real Axum router.
pub struct TestApp {
    pub router: Router,
    pub pool: MySqlPool,
    pub database_url: String,
    _backend: Arc<Backend>,
}

impl TestApp {
    /// Returns `None` when no MySQL backend is available (see the module docs). Callers
    /// should `let Some(app) = TestApp::spawn(..).await else { return };`.
    pub async fn spawn(app_names: &[&str]) -> Option<TestApp> {
        let apps: Vec<(&str, AppStorageConfig)> = app_names
            .iter()
            .map(|name| {
                (
                    *name,
                    storage_config(Some(ENDPOINT), "test-access-key", "test-secret-key", BUCKET),
                )
            })
            .collect();
        TestApp::spawn_with_storage(&apps).await
    }

    /// Like [`TestApp::spawn`], but each app gets the storage configuration given for it —
    /// used by the storage tests to point apps at a real (MinIO) bucket, and to give two
    /// apps genuinely different buckets and credentials.
    pub async fn spawn_with_storage(apps: &[(&str, AppStorageConfig)]) -> Option<TestApp> {
        let backend = backend().await?;
        let db_name = backend
            .create_database()
            .await
            .expect("failed to create a test database");
        let database_url = format!("{}/{}", backend.base_url, db_name);

        // This is the production startup path: it runs migrations/ against an empty
        // MySQL 8 database. A migration bug fails every test in the suite, loudly.
        let pool = init_db(&database_url)
            .await
            .expect("migrations failed to apply to an empty MySQL 8 database");

        let apps: HashMap<String, AppConfig> = apps
            .iter()
            .map(|(name, storage)| {
                (
                    (*name).to_string(),
                    AppConfig {
                        name: (*name).to_string(),
                        auth_token: auth_token(name),
                        storage: storage.clone(),
                    },
                )
            })
            .collect();

        let config = Arc::new(Config {
            database_url: database_url.clone(),
            host: "127.0.0.1".to_string(),
            port: 0,
            apps,
        });

        let state = AppState {
            config,
            db: pool.clone(),
        };

        Some(TestApp {
            router: configure_routes(state),
            pool,
            database_url,
            _backend: backend,
        })
    }

    pub async fn send(&self, request: Request<Body>) -> Resp {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router call failed");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("failed to read response body")
            .to_bytes()
            .to_vec();
        Resp { status, body }
    }

    /// GET with no `Authorization` header (the update-check routes, or a negative auth case).
    pub async fn get_anon(&self, uri: &str) -> Resp {
        self.send(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// GET with a single arbitrary header (e.g. `Hot-Updater-SDK-Version`).
    pub async fn get_with_header(&self, uri: &str, name: &str, value: &str) -> Resp {
        self.send(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(name, value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// GET with a verbatim `Authorization` header value (for malformed-header cases).
    pub async fn get_with_auth(&self, uri: &str, authorization: &str) -> Resp {
        self.send(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("Authorization", authorization)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// GET authenticated as `app`'s CLI token.
    pub async fn get_as(&self, app: &str, uri: &str) -> Resp {
        self.get_with_auth(uri, &format!("Bearer {}", auth_token(app)))
            .await
    }

    async fn json_request(
        &self,
        method: &str,
        app: &str,
        uri: &str,
        body: &serde_json::Value,
    ) -> Resp {
        self.send(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", format!("Bearer {}", auth_token(app)))
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
    }

    pub async fn post_as(&self, app: &str, uri: &str, body: &serde_json::Value) -> Resp {
        self.json_request("POST", app, uri, body).await
    }

    pub async fn patch_as(&self, app: &str, uri: &str, body: &serde_json::Value) -> Resp {
        self.json_request("PATCH", app, uri, body).await
    }

    pub async fn delete_as(&self, app: &str, uri: &str) -> Resp {
        self.send(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("Authorization", format!("Bearer {}", auth_token(app)))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Insert a row straight into `bundles`, bypassing the API, so fixtures are independent
    /// of the endpoint under test.
    pub async fn seed(&self, bundle: &SeedBundle) {
        sqlx::query(
            r#"INSERT INTO bundles (
                   id, app_name, platform, should_force_update, enabled, file_hash,
                   git_commit_hash, message, channel, storage_uri, target_app_version,
                   fingerprint_hash, metadata, rollout_cohort_count, target_cohorts,
                   manifest_storage_uri, manifest_file_hash, asset_base_storage_uri
               ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&bundle.id)
        .bind(&bundle.app_name)
        .bind(&bundle.platform)
        .bind(i8::from(bundle.should_force_update))
        .bind(i8::from(bundle.enabled))
        .bind(&bundle.file_hash)
        .bind(&bundle.message)
        .bind(&bundle.channel)
        .bind(&bundle.storage_uri)
        .bind(&bundle.target_app_version)
        .bind(&bundle.fingerprint_hash)
        .bind(serde_json::json!({}))
        .bind(bundle.rollout_cohort_count)
        .bind(
            bundle
                .target_cohorts
                .as_ref()
                .map(|c| serde_json::to_value(c).unwrap()),
        )
        .bind(&bundle.manifest_storage_uri)
        .bind(&bundle.manifest_file_hash)
        .bind(&bundle.asset_base_storage_uri)
        .execute(&self.pool)
        .await
        .expect("failed to seed bundle");
    }

    pub async fn bundle_ids_of(&self, app: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM bundles WHERE app_name = ? ORDER BY id DESC",
        )
        .bind(app)
        .fetch_all(&self.pool)
        .await
        .expect("failed to read bundle ids")
    }

    pub async fn file_hash_of(&self, app: &str, id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT file_hash FROM bundles WHERE app_name = ? AND id = ?",
        )
        .bind(app)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .expect("failed to read file_hash")
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "response body was not JSON ({err}); status={} body={}",
                self.status,
                self.text()
            )
        })
    }
}

/// A `bundles` row for [`TestApp::seed`], with parity-sane defaults: enabled, production
/// channel, ios, fully rolled out, targeting app version `1.0.0`.
#[derive(Clone, Debug)]
pub struct SeedBundle {
    pub id: String,
    pub app_name: String,
    pub platform: String,
    pub channel: String,
    pub enabled: bool,
    pub should_force_update: bool,
    pub file_hash: String,
    pub message: Option<String>,
    pub storage_uri: String,
    pub target_app_version: Option<String>,
    pub fingerprint_hash: Option<String>,
    pub rollout_cohort_count: i32,
    pub target_cohorts: Option<Vec<String>>,
    /// The three manifest columns default to NULL, which is what every non-storage test
    /// wants: `resolve_manifest_artifacts` returns early and never touches S3.
    pub manifest_storage_uri: Option<String>,
    pub manifest_file_hash: Option<String>,
    pub asset_base_storage_uri: Option<String>,
}

impl SeedBundle {
    pub fn new(id: &str, app_name: &str) -> Self {
        SeedBundle {
            id: id.to_string(),
            app_name: app_name.to_string(),
            platform: "ios".to_string(),
            channel: "production".to_string(),
            enabled: true,
            should_force_update: false,
            file_hash: format!("hash-{id}"),
            message: None,
            storage_uri: format!("s3://{BUCKET}/{id}/bundle.zip"),
            target_app_version: Some("1.0.0".to_string()),
            fingerprint_hash: None,
            rollout_cohort_count: 1000,
            target_cohorts: None,
            manifest_storage_uri: None,
            manifest_file_hash: None,
            asset_base_storage_uri: None,
        }
    }

    pub fn storage_uri(mut self, uri: &str) -> Self {
        self.storage_uri = uri.to_string();
        self
    }

    /// Set all three manifest columns at once — `resolve_manifest_artifacts` requires the
    /// full set and returns `None` if any one of them is NULL.
    pub fn manifest(mut self, manifest_uri: &str, file_hash: &str, asset_base_uri: &str) -> Self {
        self.manifest_storage_uri = Some(manifest_uri.to_string());
        self.manifest_file_hash = Some(file_hash.to_string());
        self.asset_base_storage_uri = Some(asset_base_uri.to_string());
        self
    }

    pub fn platform(mut self, platform: &str) -> Self {
        self.platform = platform.to_string();
        self
    }

    pub fn channel(mut self, channel: &str) -> Self {
        self.channel = channel.to_string();
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn force_update(mut self, force: bool) -> Self {
        self.should_force_update = force;
        self
    }

    pub fn message(mut self, message: &str) -> Self {
        self.message = Some(message.to_string());
        self
    }

    pub fn target_app_version(mut self, version: Option<&str>) -> Self {
        self.target_app_version = version.map(|v| v.to_string());
        self
    }

    pub fn fingerprint(mut self, hash: &str) -> Self {
        self.fingerprint_hash = Some(hash.to_string());
        self
    }

    pub fn rollout(mut self, count: i32) -> Self {
        self.rollout_cohort_count = count;
        self
    }

    pub fn target_cohorts(mut self, cohorts: &[&str]) -> Self {
        self.target_cohorts = Some(cohorts.iter().map(|c| (*c).to_string()).collect());
        self
    }
}

// ---------------------------------------------------------------------------
// Object storage (MinIO)
// ---------------------------------------------------------------------------

/// Default MinIO root credentials, as baked into the image the testcontainers module runs.
pub const MINIO_ROOT_USER: &str = "minioadmin";
pub const MINIO_ROOT_PASSWORD: &str = "minioadmin";

/// An S3-compatible server the tests can create buckets on: either a MinIO container we
/// started or an externally supplied endpoint.
struct ObjectStore {
    /// `http://host:port`, no trailing slash, no bucket component.
    endpoint: String,
    root_access_key: String,
    root_secret_key: String,
    /// `None` for an external endpoint, which also means `mc` cannot be run and per-app
    /// users cannot be provisioned.
    container: Option<ContainerAsync<MinIO>>,
}

impl ObjectStore {
    /// A client with the root credentials, used to set fixtures up (create buckets, put
    /// objects). Deliberately separate from anything `src/storage.rs` builds, so the code
    /// under test never shares a client with the fixture code.
    fn root_client(&self) -> aws_sdk_s3::Client {
        s3_client(&self.endpoint, &self.root_access_key, &self.root_secret_key)
    }

    /// Run a `/bin/sh` script inside the container (MinIO's image ships `mc`). Returns the
    /// combined output on success. `None` when there is no container to exec into.
    async fn sh(&self, script: &str) -> Option<String> {
        let container = self.container.as_ref()?;
        let mut result = container
            .exec(
                ExecCommand::new(["/bin/sh", "-c", script])
                    .with_cmd_ready_condition(CmdWaitFor::exit()),
            )
            .await
            .expect("failed to exec in the MinIO container");
        let stdout = String::from_utf8_lossy(&result.stdout_to_vec().await.unwrap()).into_owned();
        let stderr = String::from_utf8_lossy(&result.stderr_to_vec().await.unwrap()).into_owned();
        let code = result.exit_code().await.expect("failed to read exit code");
        assert_eq!(
            code,
            Some(0),
            "mc script failed (exit {code:?})\nscript:\n{script}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        Some(format!("{stdout}{stderr}"))
    }
}

/// Build an S3 client the same way `src/storage.rs` does (path style + a `Region` the
/// server never really uses), so fixture setup and the code under test address objects
/// identically.
fn s3_client(endpoint: &str, access_key: &str, secret_key: &str) -> aws_sdk_s3::Client {
    let credentials = aws_credential_types::Credentials::new(
        access_key.to_string(),
        secret_key.to_string(),
        None,
        None,
        "TestCredentials",
    );
    let conf = aws_sdk_s3::config::Builder::new()
        .credentials_provider(credentials)
        .region(aws_config::Region::new("auto"))
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

static OBJECT_STORE: LazyLock<Mutex<Weak<ObjectStore>>> = LazyLock::new(|| Mutex::new(Weak::new()));
static OBJECT_STORE_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

async fn object_store() -> Option<Arc<ObjectStore>> {
    if OBJECT_STORE_UNAVAILABLE.load(Ordering::Relaxed) {
        return None;
    }

    let mut slot = OBJECT_STORE.lock().await;
    if let Some(existing) = slot.upgrade() {
        return Some(existing);
    }

    let access_key =
        std::env::var("OTA_TEST_S3_ACCESS_KEY_ID").unwrap_or_else(|_| MINIO_ROOT_USER.to_string());
    let secret_key = std::env::var("OTA_TEST_S3_SECRET_ACCESS_KEY")
        .unwrap_or_else(|_| MINIO_ROOT_PASSWORD.to_string());

    if let Ok(endpoint) = std::env::var("OTA_TEST_S3_ENDPOINT") {
        let store = Arc::new(ObjectStore {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            root_access_key: access_key,
            root_secret_key: secret_key,
            container: None,
        });
        *slot = Arc::downgrade(&store);
        return Some(store);
    }

    let image = MinIO::default();
    let started = match std::env::var("OTA_TEST_S3_TAG") {
        Ok(tag) => image.with_tag(tag).start().await,
        Err(_) => image.start().await,
    };

    let container = match started {
        Ok(c) => c,
        Err(err) => {
            if require_backend() {
                panic!(
                    "OTA_REQUIRE_DOCKER_TESTS is set but no MinIO container could be started: {err}"
                );
            }
            OBJECT_STORE_UNAVAILABLE.store(true, Ordering::Relaxed);
            eprintln!(
                "SKIPPING object storage integration tests: could not start a MinIO container \
                 ({err}). Start Docker, or set OTA_TEST_S3_ENDPOINT to an existing S3-compatible \
                 server. Set OTA_REQUIRE_DOCKER_TESTS=1 to make this a hard failure."
            );
            return None;
        }
    };

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(9000)
        .await
        .expect("container port");

    let store = Arc::new(ObjectStore {
        endpoint: format!("http://{host}:{port}"),
        root_access_key: access_key,
        root_secret_key: secret_key,
        container: Some(container),
    });
    *slot = Arc::downgrade(&store);
    Some(store)
}

/// A freshly created, uniquely named bucket plus the credentials an app would be configured
/// with for it. One per test, so tests never share objects and are safe in parallel.
pub struct TestBucket {
    pub name: String,
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    store: Arc<ObjectStore>,
}

/// Unique, S3-legal (lowercase, 3-63 chars, no underscores) name for a bucket or a user.
fn unique_name(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

impl TestBucket {
    /// Returns `None` when no object store is available (see the module docs). Callers should
    /// `let Some(bucket) = TestBucket::create().await else { return };`.
    ///
    /// The returned credentials are the store's root ones: fine for everything except the
    /// credential-isolation test, which needs [`TestBucket::create_with_own_user`].
    pub async fn create() -> Option<TestBucket> {
        let store = object_store().await?;
        let name = unique_name("ota-test");
        store
            .root_client()
            .create_bucket()
            .bucket(&name)
            .send()
            .await
            .expect("failed to create a test bucket");
        Some(TestBucket {
            name,
            endpoint: store.endpoint.clone(),
            access_key_id: store.root_access_key.clone(),
            secret_access_key: store.root_secret_key.clone(),
            store,
        })
    }

    /// Like [`TestBucket::create`], but the credentials belong to a MinIO user whose policy
    /// grants access to *this bucket only* — the way a per-app R2 token is scoped in
    /// production. Returns `None` when the store is external (no container to run `mc` in).
    pub async fn create_with_own_user() -> Option<TestBucket> {
        let mut bucket = TestBucket::create().await?;
        if bucket.store.container.is_none() {
            eprintln!(
                "SKIPPING: per-app credential scoping needs the MinIO container (it provisions a \
                 user with `mc`); OTA_TEST_S3_ENDPOINT points at an external server."
            );
            return None;
        }

        let user = unique_name("user");
        // MinIO requires a secret of at least 8 characters.
        let secret = format!("{user}-secret");
        let policy = user.clone();
        let script = format!(
            r#"set -e
mc alias set local http://127.0.0.1:9000 {root_user} {root_pass} > /dev/null
cat > /tmp/{policy}.json <<'EOF'
{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Action":["s3:*"],"Resource":["arn:aws:s3:::{bucket}","arn:aws:s3:::{bucket}/*"]}}]}}
EOF
mc admin user add local {user} {secret}
mc admin policy create local {policy} /tmp/{policy}.json
mc admin policy attach local {policy} --user {user}
"#,
            root_user = bucket.store.root_access_key,
            root_pass = bucket.store.root_secret_key,
            bucket = bucket.name,
        );
        bucket.store.sh(&script).await?;

        bucket.access_key_id = user;
        bucket.secret_access_key = secret;
        Some(bucket)
    }

    /// The `AppStorageConfig` an app would carry for this bucket.
    pub fn storage_config(&self) -> AppStorageConfig {
        storage_config(
            Some(&self.endpoint),
            &self.access_key_id,
            &self.secret_access_key,
            &self.name,
        )
    }

    /// `s3://<bucket>/<key>` with the key spliced in verbatim — no escaping, because the
    /// point of several tests is exactly what `parse_s3_uri` does to the raw text.
    pub fn uri(&self, key: &str) -> String {
        format!("s3://{}/{}", self.name, key)
    }

    /// Upload an object under the *literal* key given (the SDK escapes it on the wire; the
    /// stored key is the string passed here).
    pub async fn put(&self, key: &str, body: &[u8]) {
        self.store
            .root_client()
            .put_object()
            .bucket(&self.name)
            .key(key)
            .body(body.to_vec().into())
            .send()
            .await
            .unwrap_or_else(|err| panic!("failed to put test object {key:?}: {err:?}"));
    }

    /// Every key currently in the bucket — used to show which key an operation really hit.
    pub async fn keys(&self) -> Vec<String> {
        self.store
            .root_client()
            .list_objects_v2()
            .bucket(&self.name)
            .send()
            .await
            .expect("failed to list bucket")
            .contents()
            .iter()
            .filter_map(|o| o.key().map(str::to_string))
            .collect()
    }
}

/// GET a URL and return (status, body). Used to prove a presigned URL actually downloads.
pub async fn http_get(url: &str) -> (u16, Vec<u8>) {
    let response = reqwest::get(url)
        .await
        .unwrap_or_else(|err| panic!("GET {url} failed at the transport level: {err}"));
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .expect("failed to read body")
        .to_vec();
    (status, body)
}
