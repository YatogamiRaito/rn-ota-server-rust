//! Shared harness for the MySQL-backed integration tests.
//!
//! # How these tests are gated
//!
//! Everything in `tests/api_integration_tests.rs` and `tests/check_integration_tests.rs`
//! needs a real MySQL 8 server. By default the harness starts one with testcontainers,
//! which needs a working Docker daemon.
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
    config::{AppConfig, AppStorageConfig, Config},
    db::init_db,
    routes::configure_routes,
    AppState,
};
use sqlx::{Connection, Executor, MySqlConnection, MySqlPool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use testcontainers_modules::mysql::Mysql;
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

        let apps: HashMap<String, AppConfig> = app_names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    AppConfig {
                        name: (*name).to_string(),
                        auth_token: auth_token(name),
                        storage: AppStorageConfig {
                            endpoint: Some(ENDPOINT.to_string()),
                            access_key_id: "test-access-key".to_string(),
                            secret_access_key: "test-secret-key".to_string(),
                            bucket_name: BUCKET.to_string(),
                        },
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
               ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)"#,
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
        }
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
