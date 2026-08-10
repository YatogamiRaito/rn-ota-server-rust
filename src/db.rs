use crate::config::DbPoolConfig;
use sqlx::mysql::MySqlPoolOptions;
use sqlx::MySqlPool;
use tracing::info;

/// Connect with the default pool settings. Kept for callers that do not care about
/// tuning (tests, tools); the server itself uses [`init_db_with_options`].
pub async fn init_db(database_url: &str) -> Result<MySqlPool, sqlx::Error> {
    init_db_with_options(database_url, &DbPoolConfig::default()).await
}

pub async fn init_db_with_options(
    database_url: &str,
    pool_config: &DbPoolConfig,
) -> Result<MySqlPool, sqlx::Error> {
    info!(
        "Connecting to MySQL database at: {}",
        redact_credentials(database_url)
    );
    info!(
        max_connections = pool_config.max_connections,
        min_connections = pool_config.min_connections,
        acquire_timeout_secs = pool_config.acquire_timeout.as_secs(),
        idle_timeout_secs = pool_config.idle_timeout.as_secs(),
        "Connection pool configuration"
    );

    let pool = MySqlPoolOptions::new()
        .max_connections(pool_config.max_connections)
        .min_connections(pool_config.min_connections)
        .acquire_timeout(pool_config.acquire_timeout)
        .idle_timeout(pool_config.idle_timeout)
        .connect(database_url)
        .await?;

    info!("Database connection established. Running migrations...");
    sqlx::migrate!("./migrations").run(&pool).await?;

    info!("Database migrations completed successfully.");
    Ok(pool)
}

/// Replace the password in a `scheme://user:password@host/db` URL with `***` so the
/// startup log can be shipped to a log aggregator without leaking the DB credentials.
fn redact_credentials(database_url: &str) -> String {
    let Some((scheme, rest)) = database_url.split_once("://") else {
        return database_url.to_string();
    };
    // Only the authority section may hold credentials; the path/query never does.
    let (authority, tail) = match rest.find('/') {
        Some(idx) => rest.split_at(idx),
        None => (rest, ""),
    };
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return database_url.to_string();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    format!("{scheme}://{user}:***@{host}{tail}")
}

#[cfg(test)]
mod tests {
    use super::redact_credentials;

    #[test]
    fn redact_credentials_hides_the_password() {
        assert_eq!(
            redact_credentials("mysql://root:s3cret@db.internal:3306/ota"),
            "mysql://root:***@db.internal:3306/ota"
        );
    }

    #[test]
    fn redact_credentials_leaves_urls_without_credentials_alone() {
        assert_eq!(
            redact_credentials("mysql://127.0.0.1:3306/ota"),
            "mysql://127.0.0.1:3306/ota"
        );
        assert_eq!(redact_credentials("not-a-url"), "not-a-url");
    }
}
