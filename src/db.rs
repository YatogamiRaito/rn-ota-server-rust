use sqlx::mysql::MySqlPoolOptions;
use sqlx::MySqlPool;
use std::time::Duration;
use tracing::info;

pub async fn init_db(database_url: &str) -> Result<MySqlPool, sqlx::Error> {
    info!("Connecting to MySQL database at: {}", database_url);

    let pool = MySqlPoolOptions::new()
        .max_connections(10)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(3))
        .idle_timeout(Duration::from_secs(60))
        .connect(database_url)
        .await?;

    info!("Database connection established. Running migrations...");
    sqlx::migrate!("./migrations").run(&pool).await?;

    info!("Database migrations completed successfully.");
    Ok(pool)
}
