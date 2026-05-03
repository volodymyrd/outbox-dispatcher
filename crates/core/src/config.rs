use std::time::Duration;

/// Dispatch-loop settings (a subset of the full YAML config; expanded in Phase 2).
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
                Duration::from_secs(30),    // 30s
                Duration::from_secs(120),   // 2m
                Duration::from_secs(600),   // 10m
                Duration::from_secs(3600),  // 1h
                Duration::from_secs(21600), // 6h
                Duration::from_secs(86400), // 24h
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
