use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

/// Raw row from `outbox_events` as fetched by the scheduler.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RawEvent {
    /// Internal monotonic ID used for cursor-based polling.
    pub id: i64,
    /// Stable external identifier for the event.
    pub event_id: Uuid,
    /// The event type version (e.g., "user.signup.v1").
    pub kind: String,
    /// Category, class, or domain model of the entity (e.g., "user", "order").
    /// Namespaces the `aggregate_id` to prevent collisions across domains.
    pub aggregate_type: String,
    /// Unique ID of the specific aggregate instance (e.g., the specific user's UUID).
    /// Combined with `aggregate_type`, this enables efficient "All events for X" queries
    /// and allows receivers to enforce strict chronological ordering per entity.
    pub aggregate_id: Uuid,
    /// The actual event data.
    pub payload: serde_json::Value,
    /// Contextual information (request IDs, actor context).
    pub metadata: serde_json::Value,
    /// Array of callback definitions to be expanded.
    pub callbacks: serde_json::Value,
    /// The user or system that triggered the event.
    pub actor_id: Option<Uuid>,
    /// Used to link events in a distributed trace.
    pub correlation_id: Option<Uuid>,
    /// The event that directly caused this event.
    pub causation_id: Option<Uuid>,
    /// When the event was originally written.
    pub created_at: DateTime<Utc>,
    /// Computed in SQL as `octet_length(payload::text)::bigint`.
    pub payload_size_bytes: i64,
}

/// Determines how the dispatcher decides if a delivery is finished.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionMode {
    /// Delivery is done as soon as the receiver returns HTTP 2xx.
    #[default]
    Managed,
    /// Delivery stays "pending" after HTTP 2xx until the receiver explicitly completes it.
    External,
}

impl CompletionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::External => "external",
        }
    }
}

impl std::fmt::Display for CompletionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One delivery target attached to an event, parsed from `outbox_events.callbacks`.
#[derive(Debug, Clone)]
pub struct CallbackTarget {
    /// Unique name within the event's callback list (e.g., "send_welcome_email").
    pub name: String,
    /// The destination HTTPS URL.
    pub url: String,
    /// Whether this is a managed or external-completion callback.
    pub mode: CompletionMode,
    /// ID used to look up the HMAC secret for signing.
    pub signing_key_id: Option<String>,
    /// Custom non-reserved HTTP headers to include in the request.
    pub headers: HashMap<String, String>,
    /// Maximum number of retries before dead-lettering.
    pub max_attempts: u32,
    /// List of durations to wait between successive retries.
    pub backoff: Vec<Duration>,
    /// Hard timeout for the HTTP request itself.
    pub timeout: Duration,
    /// Optional window for external completion before auto-redelivery.
    pub external_completion_timeout: Option<Duration>,
    /// Max cycles the external-timeout sweeper will run before dead-lettering.
    pub max_completion_cycles: u32,
}

/// A delivery row joined with its parent event data, ready to be dispatched.
#[derive(Debug, Clone)]
pub struct DueDelivery {
    /// The ID of the specific delivery attempt row.
    pub delivery_id: i64,
    /// The ID of the source event.
    pub event_id: Uuid,
    /// Number of times we have attempted this specific delivery.
    pub attempts: i32,
    /// The parsed configuration for this specific target.
    pub target: CallbackTarget,
    /// Event metadata used to construct the webhook headers and body.
    pub kind: String,
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub actor_id: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
    pub causation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// The standardized payload passed to `Callback::deliver`.
#[derive(Debug, Clone)]
pub struct EventForDelivery {
    pub delivery_id: i64,
    pub event_id: Uuid,
    pub kind: String,
    pub callback_name: String,
    pub mode: CompletionMode,
    /// Category of the aggregate this event belongs to.
    pub aggregate_type: String,
    /// Unique ID of the specific aggregate instance. Receivers can use this
    /// for internal routing or to acquire a lock before processing the payload.
    pub aggregate_id: Uuid,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub actor_id: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
    pub causation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// The current attempt index (1-based).
    pub attempt: i32,
}

/// Cursor + limit for admin list endpoints.
#[derive(Debug, Clone, Default)]
pub struct PageParams {
    /// Number of items to return.
    pub limit: i64,
    /// Keyset pagination cursor (delivery ID).
    pub cursor: Option<i64>,
    /// Optional filter by callback name.
    pub callback_name: Option<String>,
}

/// Administrative view for a dead-lettered delivery.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DeadLetterRow {
    pub delivery_id: i64,
    pub event_id: Uuid,
    pub event_kind: String,
    pub callback_name: String,
    pub completion_mode: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Administrative view for an external-mode delivery awaiting completion.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ExternalPendingRow {
    pub delivery_id: i64,
    pub event_id: Uuid,
    pub event_kind: String,
    pub callback_name: String,
    pub attempts: i32,
    pub dispatched_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Full raw state of a delivery row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DeliveryRow {
    pub id: i64,
    pub event_id: Uuid,
    pub callback_name: String,
    pub completion_mode: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub available_at: DateTime<Utc>,
    pub locked_until: Option<DateTime<Utc>>,
    pub dispatched_at: Option<DateTime<Utc>>,
    pub processed_at: Option<DateTime<Utc>>,
    pub completion_cycles: i32,
    pub dead_letter: bool,
    pub created_at: DateTime<Utc>,
}

/// Detailed aggregate view for admin troubleshooting.
#[derive(Debug, Clone, Serialize)]
pub struct EventWithDeliveries {
    pub event: RawEventSerializable,
    pub deliveries: Vec<DeliveryRow>,
}

/// Serialized view of an event, suitable for API responses.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct RawEventSerializable {
    pub id: i64,
    pub event_id: Uuid,
    pub kind: String,
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub callbacks: serde_json::Value,
    pub actor_id: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
    pub causation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Per-callback breakdown returned by `GET /v1/stats`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallbackStats {
    /// Deliveries awaiting first dispatch in `managed` completion mode.
    pub pending_managed: i64,
    /// Deliveries awaiting first dispatch in `external` completion mode.
    pub pending_external: i64,
    /// Deliveries dispatched in `external` mode and awaiting completion confirmation.
    pub external_pending: i64,
    pub dead_lettered: i64,
}

/// Aggregate counts for `GET /v1/stats`.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub events_total: i64,
    pub deliveries_pending: i64,
    pub deliveries_external_pending: i64,
    pub deliveries_dead_lettered: i64,
    /// Age of the oldest pending delivery in seconds, or `None` if none exist.
    pub oldest_pending_age_seconds: Option<f64>,
    /// Per-callback breakdown keyed by `callback_name`.
    pub callbacks: std::collections::HashMap<String, CallbackStats>,
}

/// Internal row type returned by the stats query.
#[derive(Debug, sqlx::FromRow)]
pub struct StatsRow {
    pub callback_name: String,
    pub pending_managed: i64,
    pub pending_external: i64,
    pub external_pending: i64,
    pub dead_lettered: i64,
}

/// Outcome of an admin retry request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Row reset; will be picked up by the next dispatcher cycle.
    Reset,
    /// No delivery row with this id.
    NotFound,
    /// Row is currently locked by an in-flight dispatcher; refused to avoid double-dispatch.
    Locked,
}

/// Metrics report from the external-completion timeout sweeper.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Number of deliveries reset to pending.
    pub reset: u64,
    /// Number of deliveries moved to dead-letter after cycle exhaustion.
    pub exhausted: u64,
}

/// Per-reason deletion counts returned by `Repo::delete_terminal_events`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RetentionDeleted {
    /// Events deleted whose deliveries were all successfully processed.
    pub processed: u64,
    /// Events deleted whose deliveries included at least one dead-lettered delivery.
    pub dead_letter: u64,
}

/// The standardized error returned by delivery implementations.
#[derive(Debug)]
pub enum CallbackError {
    /// A failure that should be retried according to the backoff policy.
    Transient {
        reason: String,
        /// Suggestion from the receiver on when to try again (e.g. Retry-After).
        retry_after: Option<Duration>,
    },
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient { reason, .. } => write!(f, "transient: {reason}"),
        }
    }
}

impl std::error::Error for CallbackError {}
