use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

/// Raw row from `outbox_events` as fetched by the scheduler.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RawEvent {
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
    /// Computed in SQL as `octet_length(payload::text)::bigint`.
    pub payload_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionMode {
    Managed,
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
    pub name: String,
    pub url: String,
    pub mode: CompletionMode,
    pub signing_key_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub max_attempts: u32,
    pub backoff: Vec<Duration>,
    pub timeout: Duration,
    pub external_completion_timeout: Option<Duration>,
    pub max_completion_cycles: u32,
}

/// A delivery row joined with its parent event data, ready to be dispatched.
#[derive(Debug, Clone)]
pub struct DueDelivery {
    pub delivery_id: i64,
    pub event_id: Uuid,
    pub attempts: i32,
    pub target: CallbackTarget,
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

/// The payload passed to `Callback::deliver`.
#[derive(Debug, Clone)]
pub struct EventForDelivery {
    pub delivery_id: i64,
    pub event_id: Uuid,
    pub kind: String,
    pub callback_name: String,
    pub mode: CompletionMode,
    pub aggregate_type: String,
    pub aggregate_id: Uuid,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub actor_id: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
    pub causation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub attempt: i32,
}

/// Cursor + limit for admin list endpoints.
#[derive(Debug, Clone, Default)]
pub struct PageParams {
    pub limit: i64,
    /// Opaque cursor: last seen delivery id (for keyset pagination, results < cursor).
    pub cursor: Option<i64>,
    pub callback_name: Option<String>,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct EventWithDeliveries {
    pub event: RawEventSerializable,
    pub deliveries: Vec<DeliveryRow>,
}

/// A serializable view of `RawEvent` (omits `payload_size_bytes`).
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

/// Counts returned by the external-completion timeout sweeper.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub reset: u64,
    pub exhausted: u64,
}

/// The only error variant from a callback — every failure is transient.
#[derive(Debug)]
pub enum CallbackError {
    Transient {
        reason: String,
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
