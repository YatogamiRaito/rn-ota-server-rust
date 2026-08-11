//! Router construction plus the schema-level guarantees the migrations are supposed to
//! provide.
//!
//! The first test is pure and runs anywhere. The rest need a real MySQL 8 server and are
//! gated exactly like the other integration suites — see the module docs in
//! `tests/common/mod.rs`. CI must run `OTA_REQUIRE_DOCKER_TESTS=1 cargo test --all-features`.

mod common;

use common::TestApp;
use rn_ota_server_rust::{
    config::{AppConfig, AppStorageConfig, Config, DEFAULT_STORAGE_REGION},
    routes::configure_routes,
    AppState,
};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn test_router_initialization_does_not_panic() {
    let app_config = AppConfig {
        name: "test-app".to_string(),
        auth_token: "test-token".to_string(),
        storage: AppStorageConfig {
            endpoint: Some("test-endpoint".to_string()),
            access_key_id: "test-key".to_string(),
            secret_access_key: "test-secret".to_string(),
            bucket_name: "test-bucket".to_string(),
            region: DEFAULT_STORAGE_REGION.to_string(),
            force_path_style: true,
        },
    };

    let config = Arc::new(Config {
        database_url: "mysql://localhost/test".to_string(),
        host: "127.0.0.1".to_string(),
        port: 3000,
        apps: HashMap::from([("test-app".to_string(), app_config)]),
    });

    // Use a lazy pool so we don't attempt to connect to a real MySQL instance during the unit test.
    let db_pool =
        MySqlPool::connect_lazy("mysql://localhost/test").expect("Failed to create lazy pool");

    let state = AppState {
        config,
        db: db_pool,
    };

    // This will panic at runtime if there are any invalid route patterns under Axum 0.8.
    let _app = configure_routes(state);
}

/// `TestApp::spawn` runs the production startup path (`db::init_db`) against a brand new,
/// empty MySQL 8 database, so simply getting here proves every file in `migrations/`
/// applies cleanly. This test additionally asserts the *effects* of the two migrations
/// whose headers warn that they were authored without access to a live MySQL instance.
#[tokio::test]
async fn migrations_apply_cleanly_to_an_empty_mysql_8_database() {
    let Some(app) = TestApp::spawn(&["test-app"]).await else {
        return;
    };

    // MySQL 8 types information_schema identifier columns as VARBINARY, hence the CAST.
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT CAST(table_name AS CHAR) FROM information_schema.tables \
         WHERE table_schema = DATABASE() ORDER BY table_name",
    )
    .fetch_all(&app.pool)
    .await
    .expect("failed to list tables");
    for expected in [
        "_sqlx_migrations",
        "bundle_patches",
        "bundles",
        "hot_updater_settings",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "table {expected} is missing; got {tables:?}"
        );
    }

    let applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&app.pool)
            .await
            .expect("failed to read _sqlx_migrations");
    assert_eq!(applied, 5, "every migration must be recorded as applied");

    let version: String =
        sqlx::query_scalar("SELECT `value` FROM hot_updater_settings WHERE `key` = 'version'")
            .fetch_one(&app.pool)
            .await
            .expect("the init migration must seed the schema version");
    assert_eq!(version, "0.31.0");
}

/// The id/FK collation migration. Two guarantees have to hold *at the same time*, which is
/// why they are pinned in one test rather than two — satisfying either alone is easy, and
/// the project has already shipped a migration that bought one by breaking the other.
///
/// **(1) The rows the server reads must decode into `models::Bundle` / `models::BundlePatch`,
/// whose id fields are Rust `String`s.** This is the regression lock for a defect that made
/// the whole server unusable: MySQL sets the wire-protocol `BINARY` column flag for *any*
/// `_bin` collation, and sqlx maps a flagged column to the `BINARY` SQL type, which `String`
/// refuses to decode (`sqlx-mysql/src/protocol/text/column.rs`, `ColumnType::name` —
/// `let is_binary = flags.contains(ColumnFlags::BINARY)`). With `ascii_bin` on `bundles.id`
/// every `query_as::<Bundle>` in the server returned `ColumnDecode`, so `GET /bundles`,
/// `GET /bundles/{id}`, `PATCH` and both update-check queries answered 500. Any future
/// collation change that reintroduces a `_bin` collation on these columns brings that back,
/// which is why the collation itself is asserted alongside the decode.
///
/// **(2) Ids that differ only in hex-letter case must collide on the primary key.** Under
/// `ascii_general_ci` the database is case-*insensitive*, so `AAAA…` and `aaaa…` are the
/// same key and the second insert is rejected. That is a deliberate consequence of the
/// collation choice, not an accident: `decide_update` compares ids byte-wise in Rust
/// (`b.id.as_str() > client_bundle_id`) whereas upstream orders them with ICU
/// `localeCompare`, and those two disagree precisely on case (documented deviation, §3.3 of
/// docs/upstream-parity.md). Making case-variant ids unrepresentable in the schema shrinks
/// that deviation to scenarios the database cannot hold. Pinning it here means a later
/// collation change cannot silently widen the deviation again.
#[tokio::test]
async fn id_columns_are_case_insensitive_and_decode_as_strings() {
    let Some(app) = TestApp::spawn(&["test-app"]).await else {
        return;
    };

    // (1a) No id/FK column may sit on a `_bin` collation.
    for (table, column) in [
        ("bundles", "id"),
        ("bundle_patches", "id"),
        ("bundle_patches", "bundle_id"),
        ("bundle_patches", "base_bundle_id"),
    ] {
        // MySQL 8 types information_schema identifier columns as VARBINARY, hence the CAST.
        let collation: String = sqlx::query_scalar(
            "SELECT CAST(collation_name AS CHAR) FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&app.pool)
        .await
        .unwrap_or_else(|err| panic!("failed to read collation of {table}.{column}: {err}"));

        assert!(
            !collation.ends_with("_bin"),
            "{table}.{column} is on {collation}: a `_bin` collation sets the protocol BINARY \
             flag, and sqlx then refuses to decode the column into a Rust String"
        );
    }

    let first = "AAAAAAAA-0000-0000-0000-000000000000";
    let case_variant = "aaaaaaaa-0000-0000-0000-000000000000";
    let insert = |id: &'static str| {
        let pool = app.pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO bundles (id, app_name, platform, file_hash, storage_uri, target_app_version) \
                 VALUES (?, 'test-app', 'ios', 'h', 's3://test-bucket/k', '1.0.0')",
            )
            .bind(id)
            .execute(&pool)
            .await
        }
    };

    insert(first)
        .await
        .unwrap_or_else(|err| panic!("inserting {first} failed: {err}"));

    // (2) The case variant is the *same* key and must be rejected.
    let err = insert(case_variant)
        .await
        .expect_err("an id differing only in hex-letter case must collide on the primary key");
    let message = err.to_string();
    assert!(
        message.contains("1062") || message.to_lowercase().contains("duplicate"),
        "expected a duplicate-key error, got: {message}"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bundles")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the case variant must not have created a row");

    // (1b) The behavioural half of the decode guarantee, for both row types the server reads.
    let bundles = sqlx::query_as::<_, rn_ota_server_rust::models::Bundle>("SELECT * FROM bundles")
        .fetch_all(&app.pool)
        .await
        .unwrap_or_else(|err| {
            panic!("`SELECT * FROM bundles` must decode into models::Bundle: {err}")
        });
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].id, first);

    sqlx::query(
        "INSERT INTO bundle_patches (id, app_name, bundle_id, base_bundle_id, base_file_hash, patch_file_hash, patch_storage_uri) \
         VALUES (?, ?, ?, ?, 'bh', 'ph', 's3://test-bucket/p')",
    )
    .bind(format!("{first}:{first}"))
    .bind("test-app")
    .bind(first)
    .bind(first)
    .execute(&app.pool)
    .await
    .expect("failed to seed a bundle_patches row");

    let patches = sqlx::query_as::<_, rn_ota_server_rust::models::BundlePatch>(
        "SELECT * FROM bundle_patches",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap_or_else(|err| {
        panic!("`SELECT * FROM bundle_patches` must decode into models::BundlePatch: {err}")
    });
    assert_eq!(patches.len(), 1);
}

/// `20260722000000_bundle_check_constraints.sql` — MySQL 8.0.16+ actually enforces CHECK
/// constraints, so these must reject the rows the API-level validation also rejects.
#[tokio::test]
async fn bundle_check_constraints_are_enforced() {
    let Some(app) = TestApp::spawn(&["test-app"]).await else {
        return;
    };

    let insert = |id: &str, target: Option<&str>, rollout: i32| {
        let id = id.to_string();
        let target = target.map(|t| t.to_string());
        let pool = app.pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO bundles (id, app_name, platform, file_hash, storage_uri, target_app_version, rollout_cohort_count) \
                 VALUES (?, 'test-app', 'ios', 'h', 's3://b/k', ?, ?)",
            )
            .bind(id)
            .bind(target)
            .bind(rollout)
            .execute(&pool)
            .await
        }
    };

    assert!(
        insert(&common::bundle_id(1), Some("1.0.0"), 1000)
            .await
            .is_ok(),
        "a valid row must be accepted"
    );
    assert!(
        insert(&common::bundle_id(2), None, 1000).await.is_err(),
        "check_version_or_fingerprint must reject a row with neither targetAppVersion nor fingerprintHash"
    );
    assert!(
        insert(&common::bundle_id(3), Some("1.0.0"), 1001)
            .await
            .is_err(),
        "bundles_rollout_cohort_count_check must reject rolloutCohortCount > 1000"
    );
    assert!(
        insert(&common::bundle_id(4), Some("1.0.0"), -1)
            .await
            .is_err(),
        "bundles_rollout_cohort_count_check must reject a negative rolloutCohortCount"
    );
}

/// The tenant boundary lives in the schema, not only in the queries above it.
///
/// `20260811000000_tenant_scoped_primary_keys` moved `bundles` to a `(app_name, id)`
/// primary key and made both `bundle_patches` foreign keys composite. These are the two
/// properties that buys, and they are asserted against a real MySQL because neither is
/// visible in Rust: a future migration could quietly revert them and every unit test
/// would still pass.
#[tokio::test]
async fn the_schema_enforces_the_tenant_boundary() {
    let Some(app) = TestApp::spawn(&["app-one", "app-two"]).await else {
        return;
    };

    let seed = |app_name: &'static str, id: &'static str, uri: &'static str| {
        let pool = app.pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO bundles (id, app_name, platform, file_hash, storage_uri, target_app_version) \
                 VALUES (?, ?, 'ios', 'h', ?, '1.0.0')",
            )
            .bind(id)
            .bind(app_name)
            .bind(uri)
            .execute(&pool)
            .await
        }
    };

    const SHARED_ID: &str = "aaaa0000-0000-4000-8000-000000000001";

    // (1) The same bundle id in two apps is two rows. Before the composite key this was a
    // duplicate-key error, which is what forced the tenant check into application code.
    seed("app-one", SHARED_ID, "s3://one/bundle")
        .await
        .expect("app-one must be able to use this id");
    seed("app-two", SHARED_ID, "s3://two/bundle")
        .await
        .expect("app-two must be able to use the SAME id -- ids are per-app now");

    let uris: Vec<String> =
        sqlx::query_scalar("SELECT storage_uri FROM bundles WHERE id = ? ORDER BY app_name")
            .bind(SHARED_ID)
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        uris,
        vec!["s3://one/bundle".to_string(), "s3://two/bundle".to_string()],
        "both tenants must keep their own row and their own storage_uri"
    );

    // (2) A patch may not reference another app's bundle. This was previously enforced
    // only by a lookup in create_bundles; now the foreign key itself refuses it.
    seed(
        "app-one",
        "aaaa0000-0000-4000-8000-000000000002",
        "s3://one/base",
    )
    .await
    .unwrap();

    let err = sqlx::query(
        "INSERT INTO bundle_patches (id, app_name, bundle_id, base_bundle_id, base_file_hash, patch_file_hash, patch_storage_uri) \
         VALUES ('x:y', 'app-two', ?, 'aaaa0000-0000-4000-8000-000000000002', 'bh', 'ph', 's3://two/p')",
    )
    .bind(SHARED_ID)
    .execute(&app.pool)
    .await
    .expect_err("app-two must not be able to base a patch on app-one's bundle");

    let message = err.to_string();
    assert!(
        message.contains("1452") || message.to_lowercase().contains("foreign key"),
        "expected a foreign-key violation from the composite FK, got: {message}"
    );
}
