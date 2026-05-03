use std::time::Duration;

/// Dispatch-loop settings (a subset of the full YAML config; expanded in Phase 2).
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    pub poll_interval: Duration,
    pub batch_size: i64,
    pub max_attempts: u32,
    pub backoff: Vec<Duration>,
    pub handler_timeout: Duration,
    pub lock_buffer: Duration,
    pub external_timeout_sweep_interval: Duration,
    pub max_completion_cycles: u32,
    pub payload_size_limit_bytes: i64,
    pub notify_channel: String,
    pub allow_insecure_urls: bool,
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
            payload_size_limit_bytes: 1_048_576, // 1 MB
            notify_channel: "outbox_events_new".to_string(),
            allow_insecure_urls: false,
            allow_unsigned_callbacks: false,
        }
    }
}
