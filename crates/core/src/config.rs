use std::collections::HashMap;
use std::time::Duration;

use crate::error::ValidationErrors;
use crate::scheduler::{
    MAX_BACKOFF_ELEMENT_SECS, MAX_COMPLETION_CYCLES_LIMIT, MAX_HANDLER_TIMEOUT_SECS,
    MAX_PER_CALLBACK_ATTEMPTS,
};

/// Dispatch-loop settings (domain type; no serde — converted from [`DispatchSettings`]).
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// How often to poll the database if no LISTEN notification is received.
    pub poll_interval: Duration,
    /// Maximum number of deliveries to fetch and dispatch concurrently per cycle.
    pub batch_size: i64,
    /// Default maximum retry attempts for a callback before dead-lettering.
    pub max_attempts: u32,
    /// Default backoff schedule for retries.
    pub backoff: Vec<Duration>,
    /// Default HTTP timeout for webhook delivery.
    pub handler_timeout: Duration,
    /// Buffer added to the lock duration to prevent concurrent dispatchers from racing.
    pub lock_buffer: Duration,
    /// How often the external-completion timeout sweeper runs.
    pub external_timeout_sweep_interval: Duration,
    /// Default maximum number of times an external callback can time out and be redelivered.
    pub max_completion_cycles: u32,
    /// Maximum size of an event payload in bytes. Events exceeding this are dead-lettered at schedule time.
    pub payload_size_limit_bytes: i64,
    /// The Postgres channel name for LISTEN/NOTIFY.
    pub notify_channel: String,
    /// If true, allows "http://" URLs in callback definitions.
    pub allow_insecure_urls: bool,
    /// If true, allows callbacks that omit the `signing_key_id` field.
    pub allow_unsigned_callbacks: bool,
    /// If true, allows callback URLs targeting private/loopback IP addresses (SSRF guard bypass).
    pub allow_private_ip_targets: bool,
    /// Maximum number of callbacks per event; excess callbacks are immediately dead-lettered.
    pub max_callbacks_per_event: u32,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            batch_size: 50,
            max_attempts: 6,
            backoff: vec![
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(600),
                Duration::from_secs(3600),
                Duration::from_secs(21600),
                Duration::from_secs(86400),
            ],
            handler_timeout: Duration::from_secs(30),
            lock_buffer: Duration::from_secs(10),
            external_timeout_sweep_interval: Duration::from_secs(60),
            max_completion_cycles: 20,
            payload_size_limit_bytes: 1_048_576,
            notify_channel: "outbox_events_new".to_string(),
            allow_insecure_urls: false,
            allow_unsigned_callbacks: false,
            allow_private_ip_targets: false,
            max_callbacks_per_event: DEFAULT_MAX_CALLBACKS_PER_EVENT,
        }
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

fn default_acquire_timeout_secs() -> u64 {
    10
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    /// How long to wait for a free connection before returning an error.
    #[serde(default = "default_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL embeds the database password; never expose it in debug output.
        f.debug_struct("DatabaseConfig")
            .field(
                "url",
                &if self.url.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("max_connections", &self.max_connections)
            .field("acquire_timeout_secs", &self.acquire_timeout_secs)
            .finish()
    }
}

// ── Logging ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Json,
    #[default]
    Pretty,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    pub format: LogFormat,
    /// Tracing filter string (e.g. `"info,outbox_dispatcher_core=debug"`).
    /// `RUST_LOG` env var overrides this at runtime.
    pub filter: String,
}

const DEFAULT_MAX_CALLBACKS_PER_EVENT: u32 = 32;

fn default_max_callbacks_per_event() -> u32 {
    DEFAULT_MAX_CALLBACKS_PER_EVENT
}

// ── Dispatch settings (TOML-friendly) ────────────────────────────────────────

/// Deserializable mirror of [`DispatchConfig`] that stores durations as seconds.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchSettings {
    pub poll_interval_secs: u64,
    pub batch_size: i64,
    pub max_attempts: u32,
    pub backoff_secs: Vec<u64>,
    pub handler_timeout_secs: u64,
    pub lock_buffer_secs: u64,
    pub external_timeout_sweep_interval_secs: u64,
    pub max_completion_cycles: u32,
    pub payload_size_limit_bytes: i64,
    pub notify_channel: String,
    pub allow_insecure_urls: bool,
    pub allow_unsigned_callbacks: bool,
    /// If true, allows callback URLs that target private/loopback IP addresses.
    /// Off by default; only enable in dev/integration-test environments.
    #[serde(default)]
    pub allow_private_ip_targets: bool,
    /// Maximum number of callbacks allowed per event. Events with more are immediately dead-lettered.
    #[serde(default = "default_max_callbacks_per_event")]
    pub max_callbacks_per_event: u32,
}

impl From<DispatchSettings> for DispatchConfig {
    fn from(s: DispatchSettings) -> Self {
        Self {
            poll_interval: Duration::from_secs(s.poll_interval_secs),
            batch_size: s.batch_size,
            max_attempts: s.max_attempts,
            backoff: s
                .backoff_secs
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            handler_timeout: Duration::from_secs(s.handler_timeout_secs),
            lock_buffer: Duration::from_secs(s.lock_buffer_secs),
            external_timeout_sweep_interval: Duration::from_secs(
                s.external_timeout_sweep_interval_secs,
            ),
            max_completion_cycles: s.max_completion_cycles,
            payload_size_limit_bytes: s.payload_size_limit_bytes,
            notify_channel: s.notify_channel,
            allow_insecure_urls: s.allow_insecure_urls,
            allow_unsigned_callbacks: s.allow_unsigned_callbacks,
            allow_private_ip_targets: s.allow_private_ip_targets,
            max_callbacks_per_event: s.max_callbacks_per_event,
        }
    }
}

// ── Signing keys ─────────────────────────────────────────────────────────────

/// One entry in the `signing_keys` map: names the env var that holds the HMAC secret.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningKeyConfig {
    /// Name of the environment variable that contains the base64-encoded HMAC secret.
    pub secret_env: String,
}

// ── Admin API ─────────────────────────────────────────────────────────────────

fn default_admin_bind() -> String {
    "0.0.0.0:9090".to_string()
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "default_admin_bind")]
    pub bind: String,
    /// Bearer token required by all admin endpoints. Set via `ADMIN_TOKEN` env var.
    #[serde(default)]
    pub auth_token: String,
}

impl std::fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // auth_token is a bearer secret; never expose it in debug output.
        f.debug_struct("AdminConfig")
            .field("bind", &self.bind)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            bind: default_admin_bind(),
            auth_token: String::new(),
        }
    }
}

// ── HTTP client ───────────────────────────────────────────────────────────────

fn default_connect_timeout_secs() -> u64 {
    5
}

fn default_user_agent() -> String {
    "outbox-dispatcher/1.0".to_string()
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpClientConfig {
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Disables TLS certificate verification. Dev-only; logs a loud warning at startup.
    #[serde(default)]
    pub allow_insecure_tls: bool,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: default_connect_timeout_secs(),
            user_agent: default_user_agent(),
            allow_insecure_tls: false,
        }
    }
}

// ── Retention ─────────────────────────────────────────────────────────────────

fn default_processed_retention_days() -> u64 {
    7
}

fn default_dead_letter_retention_days() -> u64 {
    30
}

fn default_cleanup_interval_secs() -> u64 {
    3600
}

fn default_batch_limit() -> u64 {
    1000
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_processed_retention_days")]
    pub processed_retention_days: u64,
    #[serde(default = "default_dead_letter_retention_days")]
    pub dead_letter_retention_days: u64,
    /// How often the retention worker runs, in seconds.
    #[serde(default = "default_cleanup_interval_secs")]
    pub cleanup_interval_secs: u64,
    /// Maximum rows deleted per cleanup cycle.
    #[serde(default = "default_batch_limit")]
    pub batch_limit: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            processed_retention_days: default_processed_retention_days(),
            dead_letter_retention_days: default_dead_letter_retention_days(),
            cleanup_interval_secs: default_cleanup_interval_secs(),
            batch_limit: default_batch_limit(),
        }
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub dispatch: DispatchSettings,
    pub log: LogConfig,
    /// Signing keyring: maps key id → env var name holding the base64-encoded HMAC secret.
    #[serde(default)]
    pub signing_keys: HashMap<String, SigningKeyConfig>,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub http_client: HttpClientConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
}

impl AppConfig {
    /// Load config by layering (later sources override earlier ones):
    /// 1. `{dir}/app_config.toml` — base defaults, required
    /// 2. `{dir}/app_config_{env}.toml` — environment overrides, optional
    /// 3. `.env.toml` — local secrets (gitignored), optional
    /// 4. `APP__*` environment variables (e.g. `APP__DATABASE__URL`)
    ///
    /// `dir` defaults to `"envs"` but can be overridden via `APP_CONFIG_DIR`.
    /// `DATABASE_URL` is also accepted as a conventional env var alias for `database.url`.
    /// `ADMIN_TOKEN` is also accepted as an alias for `admin.auth_token`.
    pub fn load(env: &str) -> Result<Self, config::ConfigError> {
        let dir = std::env::var("APP_CONFIG_DIR").unwrap_or_else(|_| "envs".into());
        config::Config::builder()
            .add_source(config::File::with_name(&format!("{dir}/app_config")))
            .add_source(config::File::with_name(&format!("{dir}/app_config_{env}")).required(false))
            .add_source(config::File::with_name(".env.toml").required(false))
            .add_source(config::Environment::with_prefix("APP").separator("__"))
            .set_override_option(
                "database.url",
                std::env::var("DATABASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty()),
            )?
            .set_override_option(
                "admin.auth_token",
                std::env::var("ADMIN_TOKEN")
                    .ok()
                    .filter(|s| !s.trim().is_empty()),
            )?
            .build()?
            .try_deserialize()
    }

    /// Validate the loaded config and return all problems as a list of human-readable messages.
    ///
    /// Call this immediately after [`Self::load`] before starting the service so operators
    /// get a clear, actionable error instead of a cryptic low-level failure later.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();

        if self.database.url.trim().is_empty() {
            errors.push(
                "database.url is not set — provide it via the DATABASE_URL env var \
                 or APP__DATABASE__URL"
                    .to_string(),
            );
        }
        if self.database.max_connections == 0 {
            errors.push("database.max_connections must be > 0".to_string());
        }
        if self.database.acquire_timeout_secs == 0 {
            errors.push("database.acquire_timeout_secs must be > 0".to_string());
        }
        if self.dispatch.poll_interval_secs == 0 {
            errors.push("dispatch.poll_interval_secs must be > 0".to_string());
        }
        if self.dispatch.batch_size <= 0 {
            errors.push("dispatch.batch_size must be > 0".to_string());
        }
        if self.dispatch.max_attempts == 0 {
            errors.push("dispatch.max_attempts must be > 0".to_string());
        }
        if self.dispatch.max_attempts as u64 > MAX_PER_CALLBACK_ATTEMPTS {
            errors.push(format!(
                "dispatch.max_attempts must be <= {MAX_PER_CALLBACK_ATTEMPTS} \
                 (per-callback cap)"
            ));
        }
        if self.dispatch.handler_timeout_secs == 0 {
            errors.push("dispatch.handler_timeout_secs must be > 0".to_string());
        }
        if self.dispatch.handler_timeout_secs > MAX_HANDLER_TIMEOUT_SECS {
            errors.push(format!(
                "dispatch.handler_timeout_secs must be <= {MAX_HANDLER_TIMEOUT_SECS}"
            ));
        }
        if self.dispatch.lock_buffer_secs == 0 {
            errors.push("dispatch.lock_buffer_secs must be > 0".to_string());
        }
        if self.dispatch.max_completion_cycles == 0 {
            errors.push("dispatch.max_completion_cycles must be > 0".to_string());
        }
        if self.dispatch.max_completion_cycles as u64 > MAX_COMPLETION_CYCLES_LIMIT {
            errors.push(format!(
                "dispatch.max_completion_cycles must be <= {MAX_COMPLETION_CYCLES_LIMIT}"
            ));
        }
        if self.dispatch.backoff_secs.is_empty() {
            errors.push("dispatch.backoff_secs must contain at least one value".to_string());
        } else if self.dispatch.backoff_secs.contains(&0) {
            errors.push("dispatch.backoff_secs entries must all be > 0".to_string());
        } else if self
            .dispatch
            .backoff_secs
            .iter()
            .any(|&s| s > MAX_BACKOFF_ELEMENT_SECS)
        {
            errors.push(format!(
                "dispatch.backoff_secs entries must all be <= {MAX_BACKOFF_ELEMENT_SECS}"
            ));
        }
        if self.dispatch.external_timeout_sweep_interval_secs < 10 {
            errors.push("dispatch.external_timeout_sweep_interval_secs must be >= 10".to_string());
        }
        if self.dispatch.payload_size_limit_bytes < 1024 {
            errors.push("dispatch.payload_size_limit_bytes must be >= 1024 (1 KB)".to_string());
        }
        if self.dispatch.payload_size_limit_bytes > 104_857_600 {
            errors.push(
                "dispatch.payload_size_limit_bytes must be <= 104857600 (100 MB)".to_string(),
            );
        }
        if self.dispatch.notify_channel.trim().is_empty() {
            errors.push("dispatch.notify_channel must not be empty".to_string());
        }
        if self.dispatch.max_callbacks_per_event == 0 {
            errors.push("dispatch.max_callbacks_per_event must be > 0".to_string());
        }
        if self.log.filter.trim().is_empty() {
            errors.push("log.filter must not be empty".to_string());
        }
        if self.admin.auth_token.trim().is_empty() {
            errors.push(
                "admin.auth_token is empty — set ADMIN_TOKEN env var or APP__ADMIN__AUTH_TOKEN; \
                 the admin API must not run without a bearer token"
                    .to_string(),
            );
        }
        if self.admin.bind.parse::<std::net::SocketAddr>().is_err() {
            errors.push(format!(
                "admin.bind '{}' is not a valid socket address (expected host:port, e.g. 0.0.0.0:9090)",
                self.admin.bind
            ));
        }
        if self.http_client.connect_timeout_secs == 0 {
            errors.push("http_client.connect_timeout_secs must be > 0".to_string());
        }
        if self.http_client.user_agent.trim().is_empty() {
            errors.push("http_client.user_agent must not be empty".to_string());
        } else if self
            .http_client
            .user_agent
            .bytes()
            .any(|b| (b < 0x20 && b != b'\t') || b >= 0x7f)
        {
            errors.push(
                "http_client.user_agent contains invalid characters \
                 (control characters and non-ASCII bytes are not allowed)"
                    .to_string(),
            );
        }
        if self.retention.enabled {
            if self.retention.processed_retention_days == 0 {
                errors.push("retention.processed_retention_days must be >= 1".to_string());
            }
            if self.retention.dead_letter_retention_days == 0 {
                errors.push("retention.dead_letter_retention_days must be >= 1".to_string());
            }
            if self.retention.cleanup_interval_secs < 60 {
                errors.push("retention.cleanup_interval_secs must be >= 60 (1 minute)".to_string());
            }
            if self.retention.batch_limit == 0 || self.retention.batch_limit > 10_000 {
                errors.push("retention.batch_limit must be between 1 and 10000".to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_config(toml: &str) -> AppConfig {
        config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .expect("config build failed")
            .try_deserialize()
            .expect("config deserialize failed")
    }

    fn full_toml() -> &'static str {
        r#"
[database]
url = "postgres://user:pass@localhost/db"
max_connections = 10
acquire_timeout_secs = 10

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30, 120, 600, 3600, 21600, 86400]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false
allow_private_ip_targets = false
max_callbacks_per_event = 32

[log]
format = "json"
filter = "info"

[admin]
bind = "0.0.0.0:9090"
auth_token = "test-token"
"#
    }

    #[test]
    fn test_database_config_from_toml() {
        let cfg = build_config(full_toml());
        assert_eq!(cfg.database.url, "postgres://user:pass@localhost/db");
        assert_eq!(cfg.database.max_connections, 10);
    }

    #[test]
    fn test_log_format_json() {
        let cfg = build_config(full_toml());
        assert_eq!(cfg.log.format, LogFormat::Json);
    }

    #[test]
    fn test_log_format_pretty() {
        let cfg = build_config(
            r#"
[database]
url = ""
max_connections = 5

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false

[log]
format = "pretty"
filter = "debug"
"#,
        );
        assert_eq!(cfg.log.format, LogFormat::Pretty);
    }

    #[test]
    fn test_log_config_filter() {
        let cfg = build_config(full_toml());
        assert_eq!(cfg.log.filter, "info");
    }

    #[test]
    fn test_dispatch_settings_all_fields() {
        let cfg = build_config(full_toml());
        let d = &cfg.dispatch;
        assert_eq!(d.poll_interval_secs, 5);
        assert_eq!(d.batch_size, 50);
        assert_eq!(d.max_attempts, 6);
        assert_eq!(d.backoff_secs, vec![30, 120, 600, 3600, 21600, 86400]);
        assert_eq!(d.handler_timeout_secs, 30);
        assert_eq!(d.lock_buffer_secs, 10);
        assert_eq!(d.external_timeout_sweep_interval_secs, 60);
        assert_eq!(d.max_completion_cycles, 20);
        assert_eq!(d.payload_size_limit_bytes, 1_048_576);
        assert_eq!(d.notify_channel, "outbox_events_new");
        assert!(!d.allow_insecure_urls);
        assert!(!d.allow_unsigned_callbacks);
        assert!(!d.allow_private_ip_targets);
        assert_eq!(d.max_callbacks_per_event, 32);
    }

    #[test]
    fn test_dispatch_settings_into_dispatch_config() {
        let cfg = build_config(full_toml());
        let dc = DispatchConfig::from(cfg.dispatch);
        assert_eq!(dc.poll_interval, Duration::from_secs(5));
        assert_eq!(dc.batch_size, 50);
        assert_eq!(dc.max_attempts, 6);
        assert_eq!(
            dc.backoff,
            vec![
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(600),
                Duration::from_secs(3600),
                Duration::from_secs(21600),
                Duration::from_secs(86400),
            ]
        );
        assert_eq!(dc.handler_timeout, Duration::from_secs(30));
        assert_eq!(dc.lock_buffer, Duration::from_secs(10));
        assert_eq!(dc.external_timeout_sweep_interval, Duration::from_secs(60));
        assert_eq!(dc.max_completion_cycles, 20);
        assert_eq!(dc.payload_size_limit_bytes, 1_048_576);
        assert_eq!(dc.notify_channel, "outbox_events_new");
        assert!(!dc.allow_insecure_urls);
        assert!(!dc.allow_unsigned_callbacks);
        assert!(!dc.allow_private_ip_targets);
        assert_eq!(dc.max_callbacks_per_event, 32);
    }

    #[test]
    fn test_app_config_full() {
        let cfg = build_config(full_toml());
        assert_eq!(cfg.database.max_connections, 10);
        assert_eq!(cfg.dispatch.batch_size, 50);
        assert_eq!(cfg.log.filter, "info");
    }

    #[test]
    fn test_env_layer_override() {
        let base = r#"
[database]
url = "postgres://base/db"
max_connections = 10

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false

[log]
format = "json"
filter = "info"
"#;
        let override_toml = r#"
[database]
max_connections = 5

[dispatch]
allow_insecure_urls = true

[log]
format = "pretty"
filter = "debug,sqlx=warn"
"#;

        let cfg: AppConfig = config::Config::builder()
            .add_source(config::File::from_str(base, config::FileFormat::Toml))
            .add_source(config::File::from_str(
                override_toml,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();

        // overridden values
        assert_eq!(cfg.database.max_connections, 5);
        assert!(cfg.dispatch.allow_insecure_urls);
        assert_eq!(cfg.log.format, LogFormat::Pretty);
        assert_eq!(cfg.log.filter, "debug,sqlx=warn");

        // base values preserved
        assert_eq!(cfg.database.url, "postgres://base/db");
        assert_eq!(cfg.dispatch.batch_size, 50);
        assert!(!cfg.dispatch.allow_unsigned_callbacks);
    }

    #[test]
    fn test_database_url_builder_override() {
        // Simulates what AppConfig::load does with DATABASE_URL: set_override_option.
        let toml = r#"
[database]
url = "postgres://from-file/db"
max_connections = 10

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false

[log]
format = "json"
filter = "info"
"#;

        let cfg: AppConfig = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .set_override_option("database.url", Some("postgres://from-env/db".to_string()))
            .unwrap()
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();

        assert_eq!(cfg.database.url, "postgres://from-env/db");
    }

    // ── validate() tests ──────────────────────────────────────────────────────

    #[test]
    fn test_validate_passes_for_valid_config() {
        let cfg = build_config(full_toml());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_database_url() {
        let mut cfg = build_config(full_toml());
        cfg.database.url = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("database.url")));
    }

    #[test]
    fn test_validate_whitespace_database_url() {
        let mut cfg = build_config(full_toml());
        cfg.database.url = "   ".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("database.url")));
    }

    #[test]
    fn test_validate_zero_max_connections() {
        let mut cfg = build_config(full_toml());
        cfg.database.max_connections = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_connections")));
    }

    #[test]
    fn test_database_config_acquire_timeout_secs_default() {
        // Configs without the field should use the serde default (10).
        let toml_without_timeout = r#"
[database]
url = "postgres://user:pass@localhost/db"
max_connections = 5

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false

[log]
format = "json"
filter = "info"
"#;
        let cfg = build_config(toml_without_timeout);
        assert_eq!(cfg.database.acquire_timeout_secs, 10);
    }

    #[test]
    fn test_validate_zero_acquire_timeout_secs() {
        let mut cfg = build_config(full_toml());
        cfg.database.acquire_timeout_secs = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("acquire_timeout_secs")));
    }

    #[test]
    fn test_validate_zero_batch_size() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.batch_size = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("batch_size")));
    }

    #[test]
    fn test_validate_zero_max_attempts() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_attempts = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_attempts")));
    }

    #[test]
    fn test_validate_max_attempts_above_per_callback_cap() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_attempts = 51;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_attempts")));
    }

    #[test]
    fn test_validate_max_attempts_at_per_callback_cap() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_attempts = 50;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_backoff_secs() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.backoff_secs = vec![];
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("backoff_secs")));
    }

    #[test]
    fn test_validate_empty_log_filter() {
        let mut cfg = build_config(full_toml());
        cfg.log.filter = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("log.filter")));
    }

    #[test]
    fn test_validate_sweep_interval_below_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.external_timeout_sweep_interval_secs = 9;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.0
                .iter()
                .any(|e| e.contains("external_timeout_sweep_interval_secs"))
        );
    }

    #[test]
    fn test_validate_sweep_interval_at_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.external_timeout_sweep_interval_secs = 10;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_payload_size_below_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.payload_size_limit_bytes = 1023;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.0
                .iter()
                .any(|e| e.contains("payload_size_limit_bytes"))
        );
    }

    #[test]
    fn test_validate_payload_size_at_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.payload_size_limit_bytes = 1024;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_collects_multiple_errors() {
        let mut cfg = build_config(full_toml());
        cfg.database.url = String::new();
        cfg.database.max_connections = 0;
        cfg.dispatch.batch_size = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.len() >= 3);
    }

    // ── New section defaults ───────────────────────────────────────────────────

    #[test]
    fn test_admin_defaults_when_absent() {
        // Build a config that intentionally omits the [admin] section to verify serde defaults.
        let toml_no_admin = r#"
[database]
url = "postgres://user:pass@localhost/db"
max_connections = 10

[dispatch]
poll_interval_secs = 5
batch_size = 50
max_attempts = 6
backoff_secs = [30]
handler_timeout_secs = 30
lock_buffer_secs = 10
external_timeout_sweep_interval_secs = 60
max_completion_cycles = 20
payload_size_limit_bytes = 1048576
notify_channel = "outbox_events_new"
allow_insecure_urls = false
allow_unsigned_callbacks = false

[log]
format = "json"
filter = "info"
"#;
        let cfg = build_config(toml_no_admin);
        assert_eq!(cfg.admin.bind, "0.0.0.0:9090");
        assert_eq!(cfg.admin.auth_token, "");
    }

    #[test]
    fn test_http_client_defaults_when_absent() {
        let cfg = build_config(full_toml());
        assert_eq!(cfg.http_client.connect_timeout_secs, 5);
        assert_eq!(cfg.http_client.user_agent, "outbox-dispatcher/1.0");
        assert!(!cfg.http_client.allow_insecure_tls);
    }

    #[test]
    fn test_retention_defaults_when_absent() {
        let cfg = build_config(full_toml());
        assert!(!cfg.retention.enabled);
        assert_eq!(cfg.retention.processed_retention_days, 7);
        assert_eq!(cfg.retention.dead_letter_retention_days, 30);
        assert_eq!(cfg.retention.cleanup_interval_secs, 3600);
        assert_eq!(cfg.retention.batch_limit, 1000);
    }

    #[test]
    fn test_signing_keys_defaults_empty_when_absent() {
        let cfg = build_config(full_toml());
        assert!(cfg.signing_keys.is_empty());
    }

    #[test]
    fn test_admin_config_from_toml() {
        // Layer an override on top of full_toml() using a second source to avoid duplicate [admin] headers.
        let cfg: AppConfig = config::Config::builder()
            .add_source(config::File::from_str(
                full_toml(),
                config::FileFormat::Toml,
            ))
            .add_source(config::File::from_str(
                "[admin]\nbind = \"127.0.0.1:8080\"\nauth_token = \"secret\"",
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(cfg.admin.bind, "127.0.0.1:8080");
        assert_eq!(cfg.admin.auth_token, "secret");
    }

    #[test]
    fn test_http_client_config_from_toml() {
        let toml = format!(
            "{}\n\n[http_client]\nconnect_timeout_secs = 10\nuser_agent = \"custom/2.0\"\nallow_insecure_tls = true",
            full_toml()
        );
        let cfg = build_config(&toml);
        assert_eq!(cfg.http_client.connect_timeout_secs, 10);
        assert_eq!(cfg.http_client.user_agent, "custom/2.0");
        assert!(cfg.http_client.allow_insecure_tls);
    }

    #[test]
    fn test_retention_config_from_toml() {
        let toml = format!(
            "{}\n\n[retention]\nenabled = true\nprocessed_retention_days = 3\ndead_letter_retention_days = 14\ncleanup_interval_secs = 120\nbatch_limit = 500",
            full_toml()
        );
        let cfg = build_config(&toml);
        assert!(cfg.retention.enabled);
        assert_eq!(cfg.retention.processed_retention_days, 3);
        assert_eq!(cfg.retention.dead_letter_retention_days, 14);
        assert_eq!(cfg.retention.cleanup_interval_secs, 120);
        assert_eq!(cfg.retention.batch_limit, 500);
    }

    #[test]
    fn test_signing_keys_from_toml() {
        let toml = format!(
            "{}\n\n[signing_keys]\n\"welcome-v1\" = {{ secret_env = \"WELCOME_HMAC_SECRET\" }}\n\"audit-v1\" = {{ secret_env = \"AUDIT_HMAC_SECRET\" }}",
            full_toml()
        );
        let cfg = build_config(&toml);
        assert_eq!(cfg.signing_keys.len(), 2);
        assert_eq!(
            cfg.signing_keys["welcome-v1"].secret_env,
            "WELCOME_HMAC_SECRET"
        );
        assert_eq!(cfg.signing_keys["audit-v1"].secret_env, "AUDIT_HMAC_SECRET");
    }

    // ── Validate: new sections ─────────────────────────────────────────────────

    #[test]
    fn test_validate_http_client_zero_connect_timeout() {
        let mut cfg = build_config(full_toml());
        cfg.http_client.connect_timeout_secs = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("connect_timeout_secs")));
    }

    #[test]
    fn test_validate_http_client_empty_user_agent() {
        let mut cfg = build_config(full_toml());
        cfg.http_client.user_agent = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("user_agent")));
    }

    #[test]
    fn test_validate_retention_disabled_skips_checks() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = false;
        cfg.retention.processed_retention_days = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_retention_enabled_zero_days() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.processed_retention_days = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.0
                .iter()
                .any(|e| e.contains("processed_retention_days"))
        );
    }

    #[test]
    fn test_validate_retention_enabled_dead_letter_zero() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.dead_letter_retention_days = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.0
                .iter()
                .any(|e| e.contains("dead_letter_retention_days"))
        );
    }

    #[test]
    fn test_validate_retention_enabled_cleanup_interval_too_short() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.cleanup_interval_secs = 59;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("cleanup_interval_secs")));
    }

    #[test]
    fn test_validate_retention_enabled_cleanup_interval_at_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.cleanup_interval_secs = 60;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_retention_enabled_batch_limit_zero() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.batch_limit = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("batch_limit")));
    }

    #[test]
    fn test_validate_retention_enabled_batch_limit_too_large() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.batch_limit = 10_001;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("batch_limit")));
    }

    #[test]
    fn test_validate_retention_enabled_batch_limit_at_maximum() {
        let mut cfg = build_config(full_toml());
        cfg.retention.enabled = true;
        cfg.retention.batch_limit = 10_000;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_passes_with_full_config() {
        // full_toml() already includes [admin]; layer http_client and retention via a second source.
        let cfg: AppConfig = config::Config::builder()
            .add_source(config::File::from_str(full_toml(), config::FileFormat::Toml))
            .add_source(config::File::from_str(
                "[http_client]\nconnect_timeout_secs = 5\nuser_agent = \"test/1.0\"\nallow_insecure_tls = false\n\n[retention]\nenabled = true\nprocessed_retention_days = 7\ndead_letter_retention_days = 30\ncleanup_interval_secs = 3600\nbatch_limit = 1000",
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert!(cfg.validate().is_ok());
    }

    // ── New validate() checks ─────────────────────────────────────────────────

    #[test]
    fn test_validate_poll_interval_secs_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.poll_interval_secs = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("poll_interval_secs")));
    }

    #[test]
    fn test_validate_handler_timeout_secs_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.handler_timeout_secs = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("handler_timeout_secs")));
    }

    #[test]
    fn test_validate_max_completion_cycles_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_completion_cycles = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_completion_cycles")));
    }

    #[test]
    fn test_validate_backoff_secs_contains_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.backoff_secs = vec![0, 60];
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("backoff_secs")));
    }

    #[test]
    fn test_validate_notify_channel_empty() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.notify_channel = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("notify_channel")));
    }

    #[test]
    fn test_validate_notify_channel_whitespace() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.notify_channel = "   ".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("notify_channel")));
    }

    #[test]
    fn test_validate_payload_size_above_maximum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.payload_size_limit_bytes = 104_857_601;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.0
                .iter()
                .any(|e| e.contains("payload_size_limit_bytes"))
        );
    }

    #[test]
    fn test_validate_payload_size_at_maximum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.payload_size_limit_bytes = 104_857_600;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_admin_auth_token_empty() {
        let mut cfg = build_config(full_toml());
        cfg.admin.auth_token = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("auth_token")));
    }

    #[test]
    fn test_validate_admin_auth_token_whitespace() {
        let mut cfg = build_config(full_toml());
        cfg.admin.auth_token = "   ".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("auth_token")));
    }

    #[test]
    fn test_validate_max_callbacks_per_event_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_callbacks_per_event = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_callbacks_per_event")));
    }

    #[test]
    fn test_validate_lock_buffer_secs_zero() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.lock_buffer_secs = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("lock_buffer_secs")));
    }

    #[test]
    fn test_database_config_debug_redacts_url() {
        let cfg = build_config(full_toml());
        let debug = format!("{:?}", cfg.database);
        assert!(
            !debug.contains("postgres://"),
            "DB URL must not appear in Debug output"
        );
        assert!(
            !debug.contains("user:pass"),
            "DB password must not appear in Debug output"
        );
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn test_database_config_debug_shows_unset_for_empty_url() {
        let mut cfg = build_config(full_toml());
        cfg.database.url = String::new();
        let debug = format!("{:?}", cfg.database);
        assert!(debug.contains("<unset>"));
    }

    #[test]
    fn test_admin_config_debug_redacts_auth_token() {
        let cfg = build_config(full_toml());
        let debug = format!("{:?}", cfg.admin);
        assert!(
            !debug.contains("test-token"),
            "auth_token must not appear in Debug output"
        );
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn test_admin_config_debug_shows_unset_for_empty_token() {
        let cfg: AppConfig = config::Config::builder()
            .add_source(config::File::from_str(
                full_toml(),
                config::FileFormat::Toml,
            ))
            .add_source(config::File::from_str(
                "[admin]\nauth_token = \"\"",
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        let debug = format!("{:?}", cfg.admin);
        assert!(debug.contains("<unset>"));
    }

    // ── Fix #1: upper-bound validation ───────────────────────────────────────

    #[test]
    fn test_validate_handler_timeout_secs_above_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.handler_timeout_secs = MAX_HANDLER_TIMEOUT_SECS + 1;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("handler_timeout_secs")));
    }

    #[test]
    fn test_validate_handler_timeout_secs_at_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.handler_timeout_secs = MAX_HANDLER_TIMEOUT_SECS;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_max_completion_cycles_above_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_completion_cycles = (MAX_COMPLETION_CYCLES_LIMIT + 1) as u32;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("max_completion_cycles")));
    }

    #[test]
    fn test_validate_max_completion_cycles_at_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_completion_cycles = MAX_COMPLETION_CYCLES_LIMIT as u32;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_backoff_secs_element_above_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.backoff_secs = vec![MAX_BACKOFF_ELEMENT_SECS + 1];
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("backoff_secs")));
    }

    #[test]
    fn test_validate_backoff_secs_element_at_max() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.backoff_secs = vec![MAX_BACKOFF_ELEMENT_SECS];
        assert!(cfg.validate().is_ok());
    }

    // ── Fix #4: user_agent header-safety ─────────────────────────────────────

    #[test]
    fn test_validate_user_agent_with_control_chars_rejected() {
        let mut cfg = build_config(full_toml());
        cfg.http_client.user_agent = "bot/1.0\r\nX-Evil: yes".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("user_agent")));
    }

    #[test]
    fn test_validate_user_agent_normal_string_accepted() {
        let mut cfg = build_config(full_toml());
        cfg.http_client.user_agent = "outbox-dispatcher/2.0".to_string();
        assert!(cfg.validate().is_ok());
    }

    // ── Fix #5: admin.bind SocketAddr validation ──────────────────────────────

    #[test]
    fn test_validate_admin_bind_invalid_format_rejected() {
        let mut cfg = build_config(full_toml());
        cfg.admin.bind = "not-a-socket-addr".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("admin.bind")));
    }

    #[test]
    fn test_validate_admin_bind_valid_format_accepted() {
        let mut cfg = build_config(full_toml());
        cfg.admin.bind = "127.0.0.1:8080".to_string();
        assert!(cfg.validate().is_ok());
    }

    // ── Fix #6: AppConfig::load file-layering test ────────────────────────────
    //
    // These tests mutate process-global env vars (APP_CONFIG_DIR, DATABASE_URL).
    // They must not run concurrently; LOAD_MUTEX serialises them within this test binary.

    use std::sync::Mutex;
    static LOAD_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_load_reads_base_config_from_disk() {
        use std::io::Write;
        let _guard = LOAD_MUTEX.lock().unwrap();

        let dir = tempfile::tempdir().expect("tempdir");
        let mut f = std::fs::File::create(dir.path().join("app_config.toml")).unwrap();
        write!(f, "{}", full_toml()).unwrap();

        // SAFETY: protected by LOAD_MUTEX; no concurrent thread touches these vars.
        unsafe { std::env::set_var("APP_CONFIG_DIR", dir.path().to_str().unwrap()) };
        let cfg = AppConfig::load("local").expect("load failed");
        unsafe { std::env::remove_var("APP_CONFIG_DIR") };

        assert_eq!(cfg.database.url, "postgres://user:pass@localhost/db");
        assert_eq!(cfg.dispatch.batch_size, 50);
        assert_eq!(cfg.log.filter, "info");
    }

    #[test]
    fn test_load_env_layer_overrides_base() {
        use std::io::Write;
        let _guard = LOAD_MUTEX.lock().unwrap();

        let dir = tempfile::tempdir().expect("tempdir");
        let mut base = std::fs::File::create(dir.path().join("app_config.toml")).unwrap();
        write!(base, "{}", full_toml()).unwrap();

        let env_override = "[dispatch]\nbatch_size = 99\n";
        let mut env_file =
            std::fs::File::create(dir.path().join("app_config_staging.toml")).unwrap();
        write!(env_file, "{env_override}").unwrap();

        // SAFETY: protected by LOAD_MUTEX; no concurrent thread touches these vars.
        unsafe { std::env::set_var("APP_CONFIG_DIR", dir.path().to_str().unwrap()) };
        let cfg = AppConfig::load("staging").expect("load failed");
        unsafe { std::env::remove_var("APP_CONFIG_DIR") };

        assert_eq!(cfg.dispatch.batch_size, 99);
        assert_eq!(cfg.database.url, "postgres://user:pass@localhost/db");
    }

    #[test]
    fn test_load_database_url_env_var_overrides_file() {
        use std::io::Write;
        let _guard = LOAD_MUTEX.lock().unwrap();

        let dir = tempfile::tempdir().expect("tempdir");
        let mut f = std::fs::File::create(dir.path().join("app_config.toml")).unwrap();
        write!(f, "{}", full_toml()).unwrap();

        // SAFETY: protected by LOAD_MUTEX; no concurrent thread touches these vars.
        unsafe {
            std::env::set_var("APP_CONFIG_DIR", dir.path().to_str().unwrap());
            std::env::set_var("DATABASE_URL", "postgres://from-env/db");
        }
        let cfg = AppConfig::load("local").expect("load failed");
        unsafe {
            std::env::remove_var("DATABASE_URL");
            std::env::remove_var("APP_CONFIG_DIR");
        }

        assert_eq!(cfg.database.url, "postgres://from-env/db");
    }
}
