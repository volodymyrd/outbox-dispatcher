//! Prometheus metric names, label constants, and emit helpers.
//!
//! All metrics use the `metrics` facade crate. The Prometheus exporter is installed
//! in the binary crate (`crates/bin`) and starts listening on
//! `observability.metrics_bind`. Any code in `core` that calls these helpers just
//! writes to the global recorder; nothing is exported here.
//!
//! # Metric index (§12.1)
//!
//! | Metric | Type | Labels |
//! |---|---|---|
//! | `outbox_events_total` | counter | `kind` |
//! | `outbox_deliveries_total` | counter | `callback`, `mode`, `result` |
//! | `outbox_dispatch_duration_seconds` | histogram | `callback`, `mode` |
//! | `outbox_lag_seconds` | gauge | — |
//! | `outbox_pending_deliveries` | gauge | `callback`, `mode` |
//! | `outbox_external_pending_deliveries` | gauge | `callback` |
//! | `outbox_external_pending_seconds_bucket` | histogram | `callback`, `le` |
//! | `outbox_dead_letters_total` | counter | `callback` |
//! | `outbox_external_timeout_resets_total` | counter | `callback` |
//! | `outbox_completion_cycles_exhausted_total` | counter | `callback` |
//! | `outbox_signing_key_resolution_failures_total` | counter | `signing_key_id`, `callback` |
//! | `outbox_invalid_callbacks_total` | counter | `reason` |
//! | `outbox_payload_size_rejections_total` | counter | `kind` |
//! | `outbox_retention_deletions_total` | counter | `reason` |
//! | `outbox_retention_cycle_duration_seconds` | histogram | — |
//! | `outbox_retention_oldest_event_age_seconds` | gauge | — |
//! | `outbox_corrupted_rows_total` | counter | `stage` |
//! | `outbox_cycle_duration_seconds` | histogram | — |
//! | `outbox_listener_connection_status` | gauge | — |

// ── Metric names ───────────────────────────────────────────────────────────────

pub const EVENTS_TOTAL: &str = "outbox_events_total";
pub const DELIVERIES_TOTAL: &str = "outbox_deliveries_total";
pub const DISPATCH_DURATION_SECONDS: &str = "outbox_dispatch_duration_seconds";
pub const LAG_SECONDS: &str = "outbox_lag_seconds";
pub const PENDING_DELIVERIES: &str = "outbox_pending_deliveries";
pub const EXTERNAL_PENDING_DELIVERIES: &str = "outbox_external_pending_deliveries";
pub const EXTERNAL_PENDING_SECONDS: &str = "outbox_external_pending_seconds";
pub const DEAD_LETTERS_TOTAL: &str = "outbox_dead_letters_total";
pub const EXTERNAL_TIMEOUT_RESETS_TOTAL: &str = "outbox_external_timeout_resets_total";
pub const COMPLETION_CYCLES_EXHAUSTED_TOTAL: &str = "outbox_completion_cycles_exhausted_total";
pub const SIGNING_KEY_RESOLUTION_FAILURES_TOTAL: &str =
    "outbox_signing_key_resolution_failures_total";
pub const INVALID_CALLBACKS_TOTAL: &str = "outbox_invalid_callbacks_total";
pub const PAYLOAD_SIZE_REJECTIONS_TOTAL: &str = "outbox_payload_size_rejections_total";
pub const RETENTION_DELETIONS_TOTAL: &str = "outbox_retention_deletions_total";
pub const RETENTION_CYCLE_DURATION_SECONDS: &str = "outbox_retention_cycle_duration_seconds";
pub const RETENTION_OLDEST_EVENT_AGE_SECONDS: &str = "outbox_retention_oldest_event_age_seconds";
pub const CORRUPTED_ROWS_TOTAL: &str = "outbox_corrupted_rows_total";
pub const CYCLE_DURATION_SECONDS: &str = "outbox_cycle_duration_seconds";
pub const LISTENER_CONNECTION_STATUS: &str = "outbox_listener_connection_status";

// ── Result label values (outbox_deliveries_total.result) ──────────────────────

pub mod result {
    pub const OK: &str = "ok";
    pub const TRANSIENT: &str = "transient";
    pub const TIMEOUT: &str = "timeout";
    pub const INVALID: &str = "invalid";
    pub const EXTERNAL_RESET: &str = "external_reset";
}

// ── Retention reason label values ─────────────────────────────────────────────

pub mod retention_reason {
    pub const PROCESSED: &str = "processed";
    pub const DEAD_LETTER: &str = "dead_letter";
}

// ── Stage label values (outbox_corrupted_rows_total.stage) ────────────────────

pub mod stage {
    pub const SCHEDULE: &str = "schedule";
    pub const DISPATCH: &str = "dispatch";
    pub const SWEEP: &str = "sweep";
    pub const RETENTION: &str = "retention";
}

// ── Emit helpers ───────────────────────────────────────────────────────────────

/// Increment `outbox_events_total{kind}`.
#[inline]
pub fn inc_events_total(kind: &str) {
    metrics::counter!(EVENTS_TOTAL, "kind" => kind.to_owned()).increment(1);
}

/// Increment `outbox_deliveries_total{callback, mode, result}`.
#[inline]
pub fn inc_deliveries_total(callback: &str, mode: &str, result_label: &str) {
    metrics::counter!(
        DELIVERIES_TOTAL,
        "callback" => callback.to_owned(),
        "mode" => mode.to_owned(),
        "result" => result_label.to_owned()
    )
    .increment(1);
}

/// Record a dispatch duration observation (seconds) for `outbox_dispatch_duration_seconds`.
#[inline]
pub fn record_dispatch_duration(callback: &str, mode: &str, duration_secs: f64) {
    metrics::histogram!(
        DISPATCH_DURATION_SECONDS,
        "callback" => callback.to_owned(),
        "mode" => mode.to_owned()
    )
    .record(duration_secs);
}

/// Set `outbox_lag_seconds` gauge to `value` seconds.
#[inline]
pub fn set_lag_seconds(value: f64) {
    metrics::gauge!(LAG_SECONDS).set(value);
}

/// Set `outbox_pending_deliveries{callback, mode}` gauge.
#[inline]
pub fn set_pending_deliveries(callback: &str, mode: &str, value: f64) {
    metrics::gauge!(
        PENDING_DELIVERIES,
        "callback" => callback.to_owned(),
        "mode" => mode.to_owned()
    )
    .set(value);
}

/// Set `outbox_external_pending_deliveries{callback}` gauge.
#[inline]
pub fn set_external_pending_deliveries(callback: &str, value: f64) {
    metrics::gauge!(
        EXTERNAL_PENDING_DELIVERIES,
        "callback" => callback.to_owned()
    )
    .set(value);
}

/// Record an observation in `outbox_external_pending_seconds{callback}`.
#[inline]
pub fn record_external_pending_seconds(callback: &str, age_secs: f64) {
    metrics::histogram!(
        EXTERNAL_PENDING_SECONDS,
        "callback" => callback.to_owned()
    )
    .record(age_secs);
}

/// Increment `outbox_dead_letters_total{callback}`.
#[inline]
pub fn inc_dead_letters_total(callback: &str) {
    metrics::counter!(DEAD_LETTERS_TOTAL, "callback" => callback.to_owned()).increment(1);
}

/// Increment `outbox_external_timeout_resets_total{callback}`.
#[inline]
pub fn inc_external_timeout_resets_total(callback: &str) {
    metrics::counter!(EXTERNAL_TIMEOUT_RESETS_TOTAL, "callback" => callback.to_owned())
        .increment(1);
}

/// Increment `outbox_completion_cycles_exhausted_total{callback}`.
#[inline]
pub fn inc_completion_cycles_exhausted_total(callback: &str) {
    metrics::counter!(
        COMPLETION_CYCLES_EXHAUSTED_TOTAL,
        "callback" => callback.to_owned()
    )
    .increment(1);
}

/// Increment `outbox_signing_key_resolution_failures_total{signing_key_id, callback}`.
#[inline]
pub fn inc_signing_key_resolution_failures(signing_key_id: &str, callback: &str) {
    metrics::counter!(
        SIGNING_KEY_RESOLUTION_FAILURES_TOTAL,
        "signing_key_id" => signing_key_id.to_owned(),
        "callback" => callback.to_owned()
    )
    .increment(1);
}

/// Increment `outbox_invalid_callbacks_total{reason}`.
#[inline]
pub fn inc_invalid_callbacks_total(reason: &str) {
    metrics::counter!(INVALID_CALLBACKS_TOTAL, "reason" => reason.to_owned()).increment(1);
}

/// Increment `outbox_payload_size_rejections_total{kind}`.
#[inline]
pub fn inc_payload_size_rejections_total(kind: &str) {
    metrics::counter!(PAYLOAD_SIZE_REJECTIONS_TOTAL, "kind" => kind.to_owned()).increment(1);
}

/// Increment `outbox_retention_deletions_total{reason}`.
#[inline]
pub fn inc_retention_deletions_total(reason: &str, count: u64) {
    metrics::counter!(RETENTION_DELETIONS_TOTAL, "reason" => reason.to_owned()).increment(count);
}

/// Record a retention cycle duration observation (seconds).
#[inline]
pub fn record_retention_cycle_duration(duration_secs: f64) {
    metrics::histogram!(RETENTION_CYCLE_DURATION_SECONDS).record(duration_secs);
}

/// Set `outbox_retention_oldest_event_age_seconds` gauge.
#[inline]
pub fn set_retention_oldest_event_age_seconds(age_secs: f64) {
    metrics::gauge!(RETENTION_OLDEST_EVENT_AGE_SECONDS).set(age_secs);
}

/// Increment `outbox_corrupted_rows_total{stage}`.
#[inline]
pub fn inc_corrupted_rows_total(stage_label: &str) {
    metrics::counter!(CORRUPTED_ROWS_TOTAL, "stage" => stage_label.to_owned()).increment(1);
}

/// Record a full scheduler cycle duration observation (seconds).
#[inline]
pub fn record_cycle_duration(duration_secs: f64) {
    metrics::histogram!(CYCLE_DURATION_SECONDS).record(duration_secs);
}

/// Set `outbox_listener_connection_status` gauge (1.0 = up, 0.0 = down).
#[inline]
pub fn set_listener_connection_status(up: bool) {
    metrics::gauge!(LISTENER_CONNECTION_STATUS).set(if up { 1.0_f64 } else { 0.0_f64 });
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // These tests verify that calling the helpers does not panic (the noop recorder
    // is installed by default in unit tests — no Prometheus exporter needed).

    #[test]
    fn inc_events_total_does_not_panic() {
        inc_events_total("user.registered@v1");
    }

    #[test]
    fn inc_deliveries_total_does_not_panic() {
        inc_deliveries_total("welcome_email", "managed", result::OK);
    }

    #[test]
    fn record_dispatch_duration_does_not_panic() {
        record_dispatch_duration("welcome_email", "managed", 0.247);
    }

    #[test]
    fn set_lag_seconds_does_not_panic() {
        set_lag_seconds(12.5);
    }

    #[test]
    fn set_pending_deliveries_does_not_panic() {
        set_pending_deliveries("cb", "managed", 42.0);
    }

    #[test]
    fn set_external_pending_deliveries_does_not_panic() {
        set_external_pending_deliveries("cb", 5.0);
    }

    #[test]
    fn record_external_pending_seconds_does_not_panic() {
        record_external_pending_seconds("cb", 120.0);
    }

    #[test]
    fn inc_dead_letters_total_does_not_panic() {
        inc_dead_letters_total("cb");
    }

    #[test]
    fn inc_external_timeout_resets_total_does_not_panic() {
        inc_external_timeout_resets_total("cb");
    }

    #[test]
    fn inc_completion_cycles_exhausted_total_does_not_panic() {
        inc_completion_cycles_exhausted_total("cb");
    }

    #[test]
    fn inc_signing_key_resolution_failures_does_not_panic() {
        inc_signing_key_resolution_failures("key-v1", "cb");
    }

    #[test]
    fn inc_invalid_callbacks_total_does_not_panic() {
        inc_invalid_callbacks_total("missing_url");
    }

    #[test]
    fn inc_payload_size_rejections_total_does_not_panic() {
        inc_payload_size_rejections_total("user.registered@v1");
    }

    #[test]
    fn inc_retention_deletions_total_does_not_panic() {
        inc_retention_deletions_total(retention_reason::PROCESSED, 10);
    }

    #[test]
    fn record_retention_cycle_duration_does_not_panic() {
        record_retention_cycle_duration(0.05);
    }

    #[test]
    fn set_retention_oldest_event_age_seconds_does_not_panic() {
        set_retention_oldest_event_age_seconds(86400.0);
    }

    #[test]
    fn inc_corrupted_rows_total_does_not_panic() {
        inc_corrupted_rows_total(stage::SCHEDULE);
    }

    #[test]
    fn record_cycle_duration_does_not_panic() {
        record_cycle_duration(0.42);
    }

    #[test]
    fn set_listener_connection_status_does_not_panic() {
        set_listener_connection_status(true);
        set_listener_connection_status(false);
    }
}
