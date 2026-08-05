use std::collections::HashMap;
use std::env;

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
