use std::time::Duration;

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
        }
    }
}

// ── Database ──────────────────────────────────────────────────────────────────

fn default_acquire_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    /// How long to wait for a free connection before returning an error.
    #[serde(default = "default_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,
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
pub struct LogConfig {
    pub format: LogFormat,
    /// Tracing filter string (e.g. `"info,outbox_dispatcher_core=debug"`).
    /// `RUST_LOG` env var overrides this at runtime.
    pub filter: String,
}

// ── Dispatch settings (TOML-friendly) ────────────────────────────────────────

/// Deserializable mirror of [`DispatchConfig`] that stores durations as seconds.
#[derive(Debug, Clone, serde::Deserialize)]
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
        }
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub dispatch: DispatchSettings,
    pub log: LogConfig,
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
    pub fn load(env: &str) -> Result<Self, config::ConfigError> {
        let dir = std::env::var("APP_CONFIG_DIR").unwrap_or_else(|_| "envs".into());
        config::Config::builder()
            .add_source(config::File::with_name(&format!("{dir}/app_config")))
            .add_source(config::File::with_name(&format!("{dir}/app_config_{env}")).required(false))
            .add_source(config::File::with_name(".env.toml").required(false))
            .add_source(config::Environment::with_prefix("APP").separator("__"))
            .set_override_option("database.url", std::env::var("DATABASE_URL").ok())?
            .build()?
            .try_deserialize()
    }

    /// Validate the loaded config and return all problems as a list of human-readable messages.
    ///
    /// Call this immediately after [`Self::load`] before starting the service so operators
    /// get a clear, actionable error instead of a cryptic low-level failure later.
    pub fn validate(&self) -> Result<(), Vec<String>> {
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
        if self.dispatch.batch_size <= 0 {
            errors.push("dispatch.batch_size must be > 0".to_string());
        }
        if self.dispatch.max_attempts == 0 {
            errors.push("dispatch.max_attempts must be > 0".to_string());
        }
        if self.dispatch.backoff_secs.is_empty() {
            errors.push("dispatch.backoff_secs must contain at least one value".to_string());
        }
        if self.dispatch.external_timeout_sweep_interval_secs < 10 {
            errors.push("dispatch.external_timeout_sweep_interval_secs must be >= 10".to_string());
        }
        if self.dispatch.payload_size_limit_bytes < 1024 {
            errors.push("dispatch.payload_size_limit_bytes must be >= 1024 (1 KB)".to_string());
        }
        if self.log.filter.trim().is_empty() {
            errors.push("log.filter must not be empty".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
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

[log]
format = "json"
filter = "info"
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
        assert!(errs.iter().any(|e| e.contains("database.url")));
    }

    #[test]
    fn test_validate_whitespace_database_url() {
        let mut cfg = build_config(full_toml());
        cfg.database.url = "   ".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("database.url")));
    }

    #[test]
    fn test_validate_zero_max_connections() {
        let mut cfg = build_config(full_toml());
        cfg.database.max_connections = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("max_connections")));
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
        assert!(errs.iter().any(|e| e.contains("acquire_timeout_secs")));
    }

    #[test]
    fn test_validate_zero_batch_size() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.batch_size = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("batch_size")));
    }

    #[test]
    fn test_validate_zero_max_attempts() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.max_attempts = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("max_attempts")));
    }

    #[test]
    fn test_validate_empty_backoff_secs() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.backoff_secs = vec![];
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("backoff_secs")));
    }

    #[test]
    fn test_validate_empty_log_filter() {
        let mut cfg = build_config(full_toml());
        cfg.log.filter = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("log.filter")));
    }

    #[test]
    fn test_validate_sweep_interval_below_minimum() {
        let mut cfg = build_config(full_toml());
        cfg.dispatch.external_timeout_sweep_interval_secs = 9;
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter()
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
        assert!(errs.iter().any(|e| e.contains("payload_size_limit_bytes")));
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
        assert!(errs.len() >= 3);
    }
}
