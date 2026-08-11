use std::collections::HashMap;
use std::env;
use std::fmt::Display;
use std::str::FromStr;
use std::time::Duration;

/// Comma-separated list of app names the server serves, e.g. `APPS=main-app,beta-app`.
pub const APPS_ENV_VAR: &str = "APPS";
pub const DEFAULT_DATABASE_URL: &str = "mysql://root:password@127.0.0.1:3306/ota_server";

/// Everything needed to address one app's bucket. Passed to `src/storage.rs` as a unit rather
/// than as loose arguments, so adding a knob here does not ripple through every call site in
/// `src/routes/check.rs`.
#[derive(Clone, Debug)]
pub struct AppStorageConfig {
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket_name: String,
    /// SigV4 signing region. Defaults to `auto`, which is Cloudflare R2's convention.
    ///
    /// AWS S3 needs the real region here (`eu-central-1`, …): the credential scope in the
    /// signature has to name it, and with no endpoint configured the SDK also builds the
    /// hostname from it — `auto` produced `<bucket>.s3.auto.amazonaws.com`, which does not
    /// resolve, so stock AWS S3 could not be used at all despite being advertised.
    pub region: String,
    /// Put the bucket in the URL path (`https://host/<bucket>/<key>`) instead of the hostname.
    ///
    /// Defaults to whether a custom endpoint is set, which is the right answer for each backend
    /// this server targets: R2 and MinIO are addressed through an endpoint and want path style
    /// (MinIO cannot do virtual-hosted style without wildcard DNS), while stock AWS S3 has no
    /// endpoint override and prefers virtual-hosted style, which path style is the legacy
    /// alternative to. Override it when your deployment disagrees.
    pub force_path_style: bool,
}

/// The region assumed when nothing configures one: Cloudflare R2's convention, so existing R2
/// deployments are unaffected by the region becoming configurable.
pub const DEFAULT_STORAGE_REGION: &str = "auto";

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

        // Shared fallbacks, used when an app does not define its own.
        let global_endpoint = env::var("R2_ENDPOINT").ok();
        let global_region = env::var("R2_REGION").ok();
        let global_force_path_style = optional_bool_env("R2_FORCE_PATH_STYLE")?;

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
            let region = env::var(format!("R2_REGION_{prefix}"))
                .ok()
                .or_else(|| global_region.clone())
                .unwrap_or_else(|| DEFAULT_STORAGE_REGION.to_string());
            if region.trim().is_empty() {
                return Err(format!(
                    "Invalid R2_REGION_{prefix}: must not be empty (leave it unset for '{DEFAULT_STORAGE_REGION}')."
                ));
            }
            // Unset means "follow the endpoint": path style for R2/MinIO, virtual-hosted style
            // for stock AWS S3. See `AppStorageConfig::force_path_style`.
            let force_path_style = optional_bool_env(&format!("R2_FORCE_PATH_STYLE_{prefix}"))?
                .or(global_force_path_style)
                .unwrap_or_else(|| endpoint.is_some());

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
                        region,
                        force_path_style,
                    },
                },
            );
        }

        if apps.is_empty() {
            return Err(format!(
                "{APPS_ENV_VAR} is set but contains no usable app names."
            ));
        }

        // Storage timeouts are read here rather than in `main` so that a bad value fails
        // startup like every other configuration error. `src/storage.rs` is called from the
        // request path with no access to this struct, so the validated value is published to it
        // through `install_timeout_config`.
        crate::storage::install_timeout_config(StorageTimeoutConfig::from_env()?);
        crate::storage::install_presign_config(PresignConfig::from_env()?);

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

/// Time limits for every call this server makes to S3/R2.
///
/// These exist because there were none. The AWS SDK applies no operation timeout of its own,
/// so an endpoint that accepted the connection and then said nothing left `read_s3_file`
/// waiting indefinitely — and with it the device update-check that was awaiting it, holding a
/// request, a task and a connection for as long as the peer kept the socket open. A single
/// misbehaving storage endpoint could pin the server's resources. Every value below is a bound
/// on that.
///
/// Unlike [`DbPoolConfig`], the defaults deliberately do NOT reproduce previous behaviour:
/// the previous behaviour was "wait forever", which is the defect. They are sized for the
/// objects this server actually fetches — `manifest.json` files of a few kilobytes — with
/// enough headroom for a slow but healthy R2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageTimeoutConfig {
    /// Limit on establishing the TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Limit on the time to the FIRST BYTE of the response, measured from when the request was
    /// initiated. This is what a black-holing endpoint runs into.
    pub read_timeout: Duration,
    /// Limit on one attempt, retries counted individually.
    pub attempt_timeout: Duration,
    /// Limit on the whole operation, including every retry AND streaming the response body.
    ///
    /// The body part is enforced by `src/storage.rs`, not by the SDK: for a streaming output
    /// like `GetObject` the SDK's operation timeout ends when the response headers arrive, so
    /// a peer that sends headers and then stalls mid-body is outside it.
    pub operation_timeout: Duration,
}

impl Default for StorageTimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            read_timeout: Duration::from_secs(5),
            attempt_timeout: Duration::from_secs(10),
            operation_timeout: Duration::from_secs(20),
        }
    }
}

impl StorageTimeoutConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        let cfg = Self {
            connect_timeout: Duration::from_secs(parse_env(
                "STORAGE_CONNECT_TIMEOUT_SECS",
                defaults.connect_timeout.as_secs(),
            )?),
            read_timeout: Duration::from_secs(parse_env(
                "STORAGE_READ_TIMEOUT_SECS",
                defaults.read_timeout.as_secs(),
            )?),
            attempt_timeout: Duration::from_secs(parse_env(
                "STORAGE_ATTEMPT_TIMEOUT_SECS",
                defaults.attempt_timeout.as_secs(),
            )?),
            operation_timeout: Duration::from_secs(parse_env(
                "STORAGE_OPERATION_TIMEOUT_SECS",
                defaults.operation_timeout.as_secs(),
            )?),
        };

        // Zero would mean "time out instantly", i.e. no storage at all. There is no spelling
        // for "no limit" on purpose: that is the state this configuration exists to end.
        for (name, value) in [
            ("STORAGE_CONNECT_TIMEOUT_SECS", cfg.connect_timeout),
            ("STORAGE_READ_TIMEOUT_SECS", cfg.read_timeout),
            ("STORAGE_ATTEMPT_TIMEOUT_SECS", cfg.attempt_timeout),
            ("STORAGE_OPERATION_TIMEOUT_SECS", cfg.operation_timeout),
        ] {
            if value.is_zero() {
                return Err(format!("Invalid {name}: must be at least 1."));
            }
        }

        // An inner limit larger than the limit containing it can never fire, which would
        // silently give back the unbounded behaviour for that layer.
        if cfg.connect_timeout > cfg.attempt_timeout {
            return Err(format!(
                "Invalid STORAGE_CONNECT_TIMEOUT_SECS={}: must not exceed STORAGE_ATTEMPT_TIMEOUT_SECS={}.",
                cfg.connect_timeout.as_secs(),
                cfg.attempt_timeout.as_secs()
            ));
        }
        if cfg.read_timeout > cfg.attempt_timeout {
            return Err(format!(
                "Invalid STORAGE_READ_TIMEOUT_SECS={}: must not exceed STORAGE_ATTEMPT_TIMEOUT_SECS={}.",
                cfg.read_timeout.as_secs(),
                cfg.attempt_timeout.as_secs()
            ));
        }
        if cfg.attempt_timeout > cfg.operation_timeout {
            return Err(format!(
                "Invalid STORAGE_ATTEMPT_TIMEOUT_SECS={}: must not exceed STORAGE_OPERATION_TIMEOUT_SECS={}.",
                cfg.attempt_timeout.as_secs(),
                cfg.operation_timeout.as_secs()
            ));
        }

        Ok(cfg)
    }
}

/// How long a presigned download URL stays valid.
///
/// Deliberately separate from [`StorageTimeoutConfig`]: a timeout bounds how long *this server*
/// waits on the store, while this bounds how long a URL already handed to a device keeps working.
/// The two have no reason to move together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresignConfig {
    pub expires_in: Duration,
}

impl Default for PresignConfig {
    fn default() -> Self {
        Self {
            expires_in: Duration::from_secs(3600),
        }
    }
}

impl PresignConfig {
    /// Below this a URL is effectively broken rather than merely short-lived: an update-check
    /// response can sit in a client queue, be retried, or reach a device on a slow network
    /// before the download begins.
    pub const MIN_EXPIRY_SECS: u64 = 60;
    /// The SigV4 protocol maximum, which both AWS S3 and R2 enforce.
    ///
    /// DO NOT relax this without following the chain it interrupts. `PresigningConfig::expires_in`
    /// rejects anything larger, so a value above the maximum does not fail once at startup — it
    /// fails **every presign, on every update-check, for every device**, and a presign failure is
    /// what `src/routes/check.rs::make_response` now turns into a 500. So a single typo in
    /// `STORAGE_PRESIGN_EXPIRY_SECS` would take the whole fleet's update path down until someone
    /// read the logs.
    ///
    /// Worse before that 500 existed, and worth remembering because it is what the bound is really
    /// protecting against: the same failure used to produce `UPDATE` with `fileUrl: null`, which
    /// the client reads as "reset to the built-in bundle" — every device deleting its downloaded
    /// bundles, and looping under `shouldForceUpdate`. See `docs/upstream-parity.md` §3.4.
    ///
    /// Checking the value here, where the error can name the variable, closes that from the
    /// configuration side.
    pub const MAX_EXPIRY_SECS: u64 = 7 * 24 * 60 * 60;

    pub fn from_env() -> Result<Self, String> {
        let default = Self::default();
        let expires_in = parse_env("STORAGE_PRESIGN_EXPIRY_SECS", default.expires_in.as_secs())?;

        if !(Self::MIN_EXPIRY_SECS..=Self::MAX_EXPIRY_SECS).contains(&expires_in) {
            return Err(format!(
                "Invalid STORAGE_PRESIGN_EXPIRY_SECS={expires_in}: must be between {} and {} \
                 seconds ({} days is the SigV4 maximum both S3 and R2 enforce).",
                Self::MIN_EXPIRY_SECS,
                Self::MAX_EXPIRY_SECS,
                Self::MAX_EXPIRY_SECS / 86_400
            ));
        }

        Ok(Self {
            expires_in: Duration::from_secs(expires_in),
        })
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

/// Like [`parse_bool_env`], but distinguishes "unset" from "set to false" — needed where the
/// default depends on other configuration rather than being a fixed value.
fn optional_bool_env(key: &str) -> Result<Option<bool>, String> {
    match env::var(key) {
        Ok(_) => parse_bool_env(key, false).map(Some),
        Err(_) => Ok(None),
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
    fn the_default_presign_lifetime_is_one_hour_and_within_its_own_bounds() {
        let cfg = PresignConfig::default();
        assert_eq!(cfg.expires_in, Duration::from_secs(3600));
        assert!(cfg.expires_in.as_secs() >= PresignConfig::MIN_EXPIRY_SECS);
        assert!(cfg.expires_in.as_secs() <= PresignConfig::MAX_EXPIRY_SECS);
    }

    /// The defaults have to satisfy the same nesting rule `from_env` enforces on operators,
    /// or an unconfigured deployment would run with a layer that can never fire.
    #[test]
    fn default_storage_timeouts_nest_correctly() {
        let cfg = StorageTimeoutConfig::default();
        assert!(!cfg.connect_timeout.is_zero());
        assert!(!cfg.read_timeout.is_zero());
        assert!(cfg.connect_timeout <= cfg.attempt_timeout);
        assert!(cfg.read_timeout <= cfg.attempt_timeout);
        assert!(cfg.attempt_timeout <= cfg.operation_timeout);
    }

    #[test]
    fn validate_app_name_rejects_whitespace_and_slashes() {
        assert!(validate_app_name("my app").is_err());
        assert!(validate_app_name("my/app").is_err());
        assert!(validate_app_name("my-app").is_ok());
    }
}
