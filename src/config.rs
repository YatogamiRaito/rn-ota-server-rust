use std::collections::HashMap;
use std::env;
use std::fmt::Display;
use std::str::FromStr;
use std::time::Duration;

/// Comma-separated list of app names the server serves, e.g. `APPS=main-app,beta-app`.
pub const APPS_ENV_VAR: &str = "APPS";
pub const DEFAULT_DATABASE_URL: &str = "mysql://root:password@127.0.0.1:3306/ota_server";

#[derive(Clone, Debug)]
pub struct AppStorageConfig {
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket_name: String,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub name: String,
    pub auth_token: String,
    pub storage: AppStorageConfig,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub host: String,
    pub port: u16,
    /// Keyed by app name exactly as it appears in the `APPS` list and in request paths.
    pub apps: HashMap<String, AppConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

        let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3010);

        let apps_raw = env::var(APPS_ENV_VAR).map_err(|_| {
            format!(
                "Missing env: {APPS_ENV_VAR} (comma-separated app names, e.g. {APPS_ENV_VAR}=main-app,beta-app)"
            )
        })?;

        // Shared fallback endpoint, used when an app does not define its own.
        let global_endpoint = env::var("R2_ENDPOINT").ok();

        let mut apps: HashMap<String, AppConfig> = HashMap::new();
        for raw_name in apps_raw.split(',') {
            let name = raw_name.trim();
            if name.is_empty() {
                continue;
            }
            validate_app_name(name)?;

            let prefix = env_key_prefix(name);
            if apps.contains_key(name) {
                return Err(format!(
                    "Duplicate app name '{name}' in {APPS_ENV_VAR}: app names must be unique."
                ));
            }

            let auth_token = require_env(&format!("AUTH_TOKEN_{prefix}"))
                .or_else(|_| require_env(&format!("HOT_UPDATER_AUTH_TOKEN_{prefix}")))?;
            let access_key_id = require_env(&format!("R2_ACCESS_KEY_ID_{prefix}"))?;
            let secret_access_key = require_env(&format!("R2_SECRET_ACCESS_KEY_{prefix}"))?;
            let bucket_name = require_env(&format!("R2_BUCKET_NAME_{prefix}"))?;
            let endpoint = env::var(format!("R2_ENDPOINT_{prefix}"))
                .ok()
                .or_else(|| global_endpoint.clone());

            apps.insert(
                name.to_string(),
                AppConfig {
                    name: name.to_string(),
                    auth_token,
                    storage: AppStorageConfig {
                        endpoint,
                        access_key_id,
                        secret_access_key,
                        bucket_name,
                    },
                },
            );
        }

        if apps.is_empty() {
            return Err(format!(
                "{APPS_ENV_VAR} is set but contains no usable app names."
            ));
        }

        Ok(Config {
            database_url,
            host,
            port,
            apps,
        })
    }

    pub fn get_app_config(&self, app_name: &str) -> Option<&AppConfig> {
        self.apps.get(app_name)
    }
}

/// Tuning for the MySQL connection pool.
///
/// Every field is optional in the environment and defaults to the value this server
/// used when the pool was hardcoded, so an unchanged `.env` keeps behaving identically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbPoolConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Default for DbPoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 2,
            acquire_timeout: Duration::from_secs(3),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

impl DbPoolConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        let cfg = Self {
            max_connections: parse_env("DB_MAX_CONNECTIONS", defaults.max_connections)?,
            min_connections: parse_env("DB_MIN_CONNECTIONS", defaults.min_connections)?,
            acquire_timeout: Duration::from_secs(parse_env(
                "DB_ACQUIRE_TIMEOUT_SECS",
                defaults.acquire_timeout.as_secs(),
            )?),
            idle_timeout: Duration::from_secs(parse_env(
                "DB_IDLE_TIMEOUT_SECS",
                defaults.idle_timeout.as_secs(),
            )?),
        };

        if cfg.max_connections == 0 {
            return Err("Invalid DB_MAX_CONNECTIONS: must be at least 1.".to_string());
        }
        if cfg.min_connections > cfg.max_connections {
            return Err(format!(
                "Invalid DB_MIN_CONNECTIONS={}: must not exceed DB_MAX_CONNECTIONS={}.",
                cfg.min_connections, cfg.max_connections
            ));
        }
        if cfg.acquire_timeout.is_zero() {
            return Err("Invalid DB_ACQUIRE_TIMEOUT_SECS: must be at least 1.".to_string());
        }

        Ok(cfg)
    }
}

/// How much per-request logging the HTTP layer emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpLogLevel {
    /// No request spans and no access log at all — the cheapest possible hot path.
    Off,
    On(tracing::Level),
}

/// Everything the observability layer needs. All optional, all defaulted.
#[derive(Clone, Debug)]
pub struct ObservabilityConfig {
    /// Level of the per-request access log (`HTTP_LOG_LEVEL`).
    pub http_log_level: HttpLogLevel,
    /// Whether `GET /metrics` is served (`METRICS_ENABLED`).
    pub metrics_enabled: bool,
    /// Allowed CORS origins (`CORS_ALLOWED_ORIGINS`). Empty = no CORS layer at all.
    pub cors_allowed_origins: Vec<String>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            http_log_level: HttpLogLevel::On(tracing::Level::INFO),
            metrics_enabled: true,
            // Restrictive by default: the hot-updater SDK and CLI are native HTTP
            // clients, not browsers, so nothing in the normal flow needs CORS. Sending
            // no CORS headers at all keeps browsers from scripting the CLI API on
            // behalf of whoever happens to be visiting a page. Opt in per deployment.
            cors_allowed_origins: Vec::new(),
        }
    }
}

impl ObservabilityConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();

        let http_log_level = match env::var("HTTP_LOG_LEVEL") {
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "off" | "none" => HttpLogLevel::Off,
                "error" => HttpLogLevel::On(tracing::Level::ERROR),
                "warn" => HttpLogLevel::On(tracing::Level::WARN),
                "info" => HttpLogLevel::On(tracing::Level::INFO),
                "debug" => HttpLogLevel::On(tracing::Level::DEBUG),
                "trace" => HttpLogLevel::On(tracing::Level::TRACE),
                other => {
                    return Err(format!(
                        "Invalid HTTP_LOG_LEVEL='{other}': expected one of off, error, warn, info, debug, trace."
                    ))
                }
            },
            Err(_) => defaults.http_log_level,
        };

        let cors_allowed_origins = env::var("CORS_ALLOWED_ORIGINS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or(defaults.cors_allowed_origins);

        Ok(Self {
            http_log_level,
            metrics_enabled: parse_bool_env("METRICS_ENABLED", defaults.metrics_enabled)?,
            cors_allowed_origins,
        })
    }
}

/// Opt-in rate limiting for the unauthenticated update-check routes.
///
/// Disabled by default: an OTA server normally sits behind a reverse proxy that already
/// does this, and silently throttling device update-checks on upgrade would be a nasty
/// surprise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub enabled: bool,
    /// Sustained requests per second allowed per client key.
    pub per_second: u32,
    /// How many requests a client may fire back to back before the sustained rate applies.
    pub burst: u32,
    /// Take the client IP from `X-Forwarded-For`/`X-Real-IP`/`Forwarded` instead of the
    /// socket peer. Only enable when a trusted proxy sets (and overwrites) those headers.
    pub trust_proxy_headers: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            per_second: 10,
            burst: 20,
            trust_proxy_headers: false,
        }
    }
}

impl RateLimitConfig {
    /// The explicit "no rate limiting" configuration.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        let cfg = Self {
            enabled: parse_bool_env("RATE_LIMIT_ENABLED", defaults.enabled)?,
            per_second: parse_env("RATE_LIMIT_UPDATE_CHECK_PER_SECOND", defaults.per_second)?,
            burst: parse_env("RATE_LIMIT_UPDATE_CHECK_BURST", defaults.burst)?,
            trust_proxy_headers: parse_bool_env(
                "RATE_LIMIT_TRUST_PROXY_HEADERS",
                defaults.trust_proxy_headers,
            )?,
        };

        if cfg.enabled && cfg.per_second == 0 {
            return Err(
                "Invalid RATE_LIMIT_UPDATE_CHECK_PER_SECOND: must be at least 1.".to_string(),
            );
        }
        if cfg.enabled && cfg.burst == 0 {
            return Err("Invalid RATE_LIMIT_UPDATE_CHECK_BURST: must be at least 1.".to_string());
        }

        Ok(cfg)
    }
}

/// Parse an optional env var, falling back to `default` when unset, and failing with a
/// message that names the variable when it is set to something unparseable.
fn parse_env<T>(key: &str, default: T) -> Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    match env::var(key) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|err| format!("Invalid {key}='{}': {err}", raw.trim())),
        Err(_) => Ok(default),
    }
}

fn parse_bool_env(key: &str, default: bool) -> Result<bool, String> {
    match env::var(key) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(format!(
                "Invalid {key}='{other}': expected a boolean (true/false)."
            )),
        },
        Err(_) => Ok(default),
    }
}

fn require_env(key: &str) -> Result<String, String> {
    env::var(key).map_err(|_| format!("Missing env: {key}"))
}

/// Convert an app name to the UPPER_SNAKE_CASE prefix used in env var names.
/// "mobile-business" -> "MOBILE_BUSINESS"; "myApp" -> "MYAPP".
fn env_key_prefix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// App names appear in URL paths, so whitespace and path separators are rejected.
fn validate_app_name(name: &str) -> Result<(), String> {
    if name.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(format!(
            "Invalid app name '{name}': app names must not contain whitespace or '/' (they appear in URL paths)."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_prefix_converts_dashes_to_underscores() {
        assert_eq!(env_key_prefix("mobile-business"), "MOBILE_BUSINESS");
        assert_eq!(env_key_prefix("myApp"), "MYAPP");
        assert_eq!(env_key_prefix("my_app"), "MY_APP");
    }

    #[test]
    fn validate_app_name_rejects_whitespace_and_slashes() {
        assert!(validate_app_name("my app").is_err());
        assert!(validate_app_name("my/app").is_err());
        assert!(validate_app_name("my-app").is_ok());
    }
}
