//! Scheduler — event discovery, callback expansion, and the LISTEN/NOTIFY wake loop.
//!
//! # Responsibilities
//!
//! 1. **Cursor recovery** — on startup, recover the high-water mark from the database
//!    so the first poll resumes from where the previous run left off.
//! 2. **Event scheduling** — for each new event, expand its `callbacks` array into
//!    `outbox_deliveries` rows, validating each callback structurally. Invalid callbacks
//!    are immediately dead-lettered; events whose payload exceeds the configured limit are
//!    dead-lettered in bulk. Poison-pill rows (corrupt JSONB that cannot be deserialized)
//!    are logged, metered, and skipped so one bad row cannot stall the entire queue.
//! 3. **Dispatch** — fetch due deliveries and invoke the `Callback` trait implementation
//!    concurrently for each one, writing results back to the repository.
//! 4. **External-completion sweep** — rate-limited sweep of hung external-mode rows:
//!    reset for redelivery or dead-letter after cycle exhaustion (Phase 5).
//! 5. **Wake source** — a `PgListener` that listens on `outbox_events_new` wakes the loop
//!    with sub-second latency on every INSERT; a periodic poll-timer is the fallback if
//!    the LISTEN connection is lost or too quiet.
//! 6. **Graceful shutdown** — a `CancellationToken` lets the caller signal shutdown; the
//!    loop drains any in-progress scheduling/dispatch step and returns cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use crate::callbacks::{parse_callbacks, payload_too_large_error};
use crate::config::DispatchConfig;
use crate::dispatch::{Callback, dispatch_due};
use crate::error::Result;
use crate::metrics;
use crate::repo::Repo;
use crate::timeout_sweep::sweep_hung_external;
use sqlx::postgres::PgListener;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// ── Public API ─────────────────────────────────────────────────────────────────

/// A shared boolean that reflects whether the `PgListener` connection is up.
///
/// `true` means the LISTEN connection is established; `false` means it is
/// disconnected or has not yet connected. This is updated by the wake loop.
pub type ListenerStatus = Arc<AtomicBool>;

/// A shared timestamp updated after each successful scheduler cycle.
///
/// Stores milliseconds since the Unix epoch (`chrono::Utc::now().timestamp_millis()`),
/// or `0` if no cycle has completed yet. Lock-free and async-safe.
///
/// The admin `/ready` endpoint uses this to check that the scheduler is still
/// running within `2 × poll_interval`.
pub type LastCycleAt = Arc<AtomicI64>;

/// Run the main dispatch loop until `shutdown` is cancelled.
///
/// Each wake cycle:
/// 1. Discovers new events and expands callbacks into delivery rows (scheduling).
/// 2. Fetches due deliveries and invokes the `Callback` implementation concurrently.
/// 3. Rate-limited: sweeps hung external-mode rows for redelivery or dead-letter.
///
/// The `listener_status` value is set to `true` while the LISTEN connection is up and
/// `false` when it drops. Callers (e.g. the admin `/ready` endpoint) can inspect it.
///
/// `last_cycle_at` is updated with `chrono::Utc::now().timestamp_millis()` after each successful
/// cycle. The admin `/ready` endpoint checks this against `2 × poll_interval`.
pub async fn run_scheduler(
    repo: Arc<dyn Repo>,
    callback: Arc<dyn Callback>,
    config: DispatchConfig,
    listener: PgListener,
    listener_status: ListenerStatus,
    shutdown: CancellationToken,
) -> Result<()> {
    run_scheduler_with_cycle_tracker(
        repo,
        callback,
        config,
        listener,
        listener_status,
        shutdown,
        None,
    )
    .await
}

/// Like [`run_scheduler`] but also updates `last_cycle_at` after each successful cycle.
pub async fn run_scheduler_with_cycle_tracker(
    repo: Arc<dyn Repo>,
    callback: Arc<dyn Callback>,
    config: DispatchConfig,
    mut listener: PgListener,
    listener_status: ListenerStatus,
    shutdown: CancellationToken,
    last_cycle_at: Option<LastCycleAt>,
) -> Result<()> {
    // Subscribe to the NOTIFY channel.
    listener
        .listen(&config.notify_channel)
        .await
        .inspect_err(|e| {
            warn!(channel = %config.notify_channel, error = %e, "failed to LISTEN on channel");
        })?;

    listener_status.store(true, Ordering::Release);
    metrics::set_listener_connection_status(true);
    info!(channel = %config.notify_channel, "LISTEN connection established");

    // Recover the event cursor so we don't re-process already-scheduled events.
    let mut cursor = repo.recover_cursor().await?;
    debug!(cursor, "recovered event cursor from database");

    let poll_interval = config.poll_interval;
    let sweep_interval = config.external_timeout_sweep_interval;
    let mut next_poll = Instant::now() + poll_interval;
    let mut last_sweep = Instant::now();

    // Tracks consecutive **scheduling** failures so we can apply a capped backoff
    // and avoid log-spamming at full speed during a sustained DB outage.
    // Dispatch / sweep errors are logged but do not contribute to the backoff because
    // dispatch_one already swallows per-row failures and the sweeper is rate-limited.
    let mut consecutive_errors: u32 = 0;

    loop {
        tokio::select! {
            biased;

            // Shutdown takes highest priority.
            _ = shutdown.cancelled() => {
                info!("scheduler received shutdown signal, stopping");
                listener_status.store(false, Ordering::Release);
                metrics::set_listener_connection_status(false);
                return Ok(());
            }

            // LISTEN notification from Postgres — lowest latency wake.
            result = listener.recv() => {
                match result {
                    Ok(notification) => {
                        debug!(
                            channel = %notification.channel(),
                            payload = notification.payload(),
                            "received NOTIFY",
                        );
                        listener_status.store(true, Ordering::Release);
                        metrics::set_listener_connection_status(true);
                    }
                    Err(e) => {
                        // sqlx PgListener reconnects automatically; we update the status
                        // gauge and let the poll-timer keep us functional.
                        warn!(error = %e, "LISTEN connection lost; will rely on poll-timer until reconnected");
                        listener_status.store(false, Ordering::Release);
                        metrics::set_listener_connection_status(false);
                    }
                }
            }

            // Poll-timer fallback: fires at most every poll_interval.
            _ = sleep_until(next_poll) => {
                debug!("scheduler poll-timer fired");
            }
        }

        // Drain any further notifications that arrived since the select fired so we run
        // one scheduling pass per *batch* of signals rather than one pass per signal.
        // This prevents a burst of 1000 NOTIFYs from triggering 1000 redundant cycles.
        while let Ok(Some(_)) = listener.try_recv().await {
            // intentionally discarded — we just want to flush the buffer
        }

        // ── Step 1: discover new events, expand callbacks into deliveries ─────
        match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
            Ok(new_cursor) => {
                cursor = new_cursor;
                consecutive_errors = 0;
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                error!(
                    error = %e,
                    consecutive_errors,
                    "error during schedule_new_deliveries; will retry on next wake"
                );
                // Cap at poll_interval so we never delay longer than a regular poll.
                let backoff = poll_interval.min(Duration::from_millis(
                    100u64.saturating_mul(1u64 << consecutive_errors.min(6)),
                ));
                tokio::time::sleep(backoff).await;
                // Reset the poll timer before continuing.
                next_poll = Instant::now() + poll_interval;
                continue;
            }
        }

        // ── Step 2: dispatch due deliveries ───────────────────────────────────
        let cycle_start = std::time::Instant::now();
        if let Err(e) = dispatch_due(repo.as_ref(), callback.as_ref(), &config).await {
            error!(error = %e, "error fetching due deliveries for dispatch");
            // Non-fatal: individual dispatch failures are handled inside dispatch_due.
        }
        metrics::record_cycle_duration(cycle_start.elapsed().as_secs_f64());

        // ── Step 3: external-completion timeout sweep (rate-limited) ─────────
        if last_sweep.elapsed() >= sweep_interval {
            if let Err(e) = sweep_hung_external(repo.as_ref(), &config).await {
                error!(error = %e, "error during external-completion timeout sweep");
            }
            last_sweep = Instant::now();
        }

        // Reset the poll timer after every cycle so the next timer fire is a full
        // poll_interval away from the last completed cycle.
        next_poll = Instant::now() + poll_interval;

        // Update the last-cycle timestamp so the admin /ready endpoint knows the
        // scheduler is still alive.
        if let Some(ref tracker) = last_cycle_at {
            tracker.store(chrono::Utc::now().timestamp_millis(), Ordering::Release);
        }
    }
}

// ── Scheduling ─────────────────────────────────────────────────────────────────

/// Fetch new events since `cursor`, expand their callbacks into delivery rows,
/// and return the updated cursor (the highest event id processed).
///
/// Each event is processed independently so one bad event does not block the rest.
/// Poison-pill rows (JSONB that fails deserialization) are counted, logged at `error`,
/// and skipped; the cursor still advances past them.
///
/// This function is intentionally `pub` so integration tests can drive it directly
/// without starting the full wake loop.
pub async fn schedule_new_deliveries(
    repo: &dyn Repo,
    config: &DispatchConfig,
    cursor: i64,
) -> Result<i64> {
    let new_events = repo
        .fetch_new_events(cursor, config.schedule_batch_size)
        .await?;
    if new_events.is_empty() {
        return Ok(cursor);
    }

    let mut new_cursor = cursor;

    for event in &new_events {
        // ── Payload-size guard ────────────────────────────────────────────────
        // Events that exceed the limit are dead-lettered immediately for every
        // callback in the list. This is a documented carve-out from the normal
        // "dead_letter only via attempts >= max_attempts" rule (see §8.2 / §2.3).
        if event.payload_size_bytes > config.payload_size_limit_bytes {
            let reason =
                payload_too_large_error(event.payload_size_bytes, config.payload_size_limit_bytes);
            warn!(
                event_id = %event.event_id,
                payload_bytes = event.payload_size_bytes,
                limit_bytes = config.payload_size_limit_bytes,
                "payload too large — dead-lettering all callbacks"
            );
            metrics::inc_payload_size_rejections_total(&event.kind);
            let names = extract_callback_names(&event.callbacks);
            let entries: Vec<(String, String)> =
                names.into_iter().map(|n| (n, reason.clone())).collect();
            // Propagate: a transient DB error here would otherwise lose the dead-letter
            // record for an oversized payload and skip the event from the cursor.
            repo.create_invalid_deliveries(event.event_id, &entries)
                .await?;
            new_cursor = event.id;
            continue;
        }

        // ── Structural validation & callback expansion ────────────────────────
        // `parse_callbacks` returns a split of valid and invalid entries.
        // Invalid entries are dead-lettered immediately; valid entries are expanded
        // into delivery rows via `ensure_deliveries` (idempotent via UNIQUE constraint).
        let parsed = parse_callbacks(&event.callbacks, config);

        // Array-level rejection guard: `parse_callbacks` returns only `<root>` invalid
        // entries when the JSONB cannot be handled at the array level.
        //
        // Two distinct cases:
        //   (a) Truly corrupt / non-array JSONB — no callback names to materialise.
        //       Log and skip (poison pill); safe because the DB CHECK constraint makes
        //       this unreachable in practice.
        //   (b) Valid array that fails an array-level check (e.g. too many callbacks).
        //       Dead-letter every named entry so the rejection is visible to operators
        //       via the admin dead-letter list.
        if parsed.valid.is_empty() && parsed.invalid.iter().all(|(n, _, _)| n == "<root>") {
            if event.callbacks.is_array() {
                // Array exists but failed an array-level check (e.g. too many callbacks).
                // Dead-letter every entry so the rejection is visible in the dead-letter list.
                let (reason_code, reason_msg) = parsed
                    .invalid
                    .first()
                    .map(|(_, code, msg)| (*code, msg.clone()))
                    .unwrap_or((
                        crate::callbacks::InvalidReason::Other,
                        "invalid_callback: unspecified array-level rejection".to_string(),
                    ));
                let entries: Vec<(String, String)> = extract_callback_names(&event.callbacks)
                    .into_iter()
                    .map(|n| (n, reason_msg.clone()))
                    .collect();
                for _ in &entries {
                    metrics::inc_invalid_callbacks_total(reason_code.metric_label());
                }
                repo.create_invalid_deliveries(event.event_id, &entries)
                    .await?;
            } else {
                // Truly unparseable JSONB — currently unreachable due to DB CHECK constraint.
                error!(
                    event_id = %event.event_id,
                    callbacks_json = %event.callbacks,
                    "callbacks JSONB is structurally corrupt — skipping event (poison pill)"
                );
                metrics::inc_corrupted_rows_total(metrics::stage::SCHEDULE);
            }
            new_cursor = event.id;
            continue;
        }

        // Emit events_total once per successfully-parsed event.
        metrics::inc_events_total(&event.kind);

        // Emit invalid_callbacks_total for each structurally invalid callback.
        for (_, reason_code, _) in &parsed.invalid {
            metrics::inc_invalid_callbacks_total(reason_code.metric_label());
        }

        // Schedule valid callbacks. Propagate DB errors so the cursor stays at the last
        // fully-processed event and the wake loop retries this event on the next cycle.
        if !parsed.valid.is_empty() {
            repo.ensure_deliveries(event.event_id, &parsed.valid)
                .await?;
        }

        // Dead-letter structurally invalid callbacks. Same rule: propagate DB errors.
        if !parsed.invalid.is_empty() {
            let invalid_entries: Vec<(String, String)> = parsed
                .invalid
                .iter()
                .map(|(name, _, msg)| (name.clone(), msg.clone()))
                .collect();
            repo.create_invalid_deliveries(event.event_id, &invalid_entries)
                .await?;
        }

        // Cursor advance is the commit point for "this event has been fully scheduled".
        // Only advance after all DB writes for this event have succeeded.
        new_cursor = event.id;
    }

    Ok(new_cursor)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Extract callback names from a JSONB array without full validation.
///
/// Used in the payload-size rejection path and array-level rejection path where we
/// need names to dead-letter each callback without running full structural validation.
///
/// The DB CHECK constraint guarantees `callbacks` is a non-empty array; the
/// `debug_assert!` below catches regressions in tests.
fn extract_callback_names(callbacks: &serde_json::Value) -> Vec<String> {
    debug_assert!(
        callbacks.as_array().is_some_and(|a| !a.is_empty()),
        "callbacks JSONB must be a non-empty array (DB CHECK constraint)"
    );
    callbacks
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, v)| {
            v.get("name")
                .and_then(|n| n.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("[{i}]"))
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── extract_callback_names ────────────────────────────────────────────────

    #[test]
    fn extract_names_from_array_of_named_objects() {
        let callbacks = json!([
            {"name": "send_email", "url": "https://a.example.com/"},
            {"name": "push_notif", "url": "https://b.example.com/"}
        ]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["send_email", "push_notif"]);
    }

    #[test]
    fn extract_names_falls_back_for_missing_name_field() {
        let callbacks = json!([
            {"url": "https://a.example.com/"},
            {"name": "push_notif"}
        ]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["[0]", "push_notif"]);
    }

    #[test]
    fn extract_names_non_string_name_field_falls_back() {
        let callbacks = json!([{"name": 42, "url": "https://a.example.com/"}]);
        let names = extract_callback_names(&callbacks);
        assert_eq!(names, vec!["[0]"]);
    }

    // ── schedule_new_deliveries (unit tests using MockRepo) ───────────────────
    //
    // These tests mock the Repo trait so they run without a database.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::schema::{
        CallbackTarget, DeadLetterRow, DueDelivery, EventWithDeliveries, ExternalPendingRow,
        PageParams, RawEvent, SweepReport,
    };
    use async_trait::async_trait;
    use chrono::Utc;
    use std::time::Duration;
    use uuid::Uuid;

    type EnsuredCalls = Mutex<Vec<(Uuid, Vec<String>)>>;
    type InvalidCalls = Mutex<Vec<(Uuid, Vec<(String, String)>)>>;

    /// A minimal mock Repo that records calls to `ensure_deliveries` and
    /// `create_invalid_deliveries`, and returns pre-loaded events from a queue.
    struct MockRepo {
        events: Mutex<VecDeque<RawEvent>>,
        ensured: EnsuredCalls,
        invalids: InvalidCalls,
        cursor: Mutex<i64>,
        /// If set, `ensure_deliveries` returns this error once then clears it.
        ensure_error: Mutex<Option<crate::error::Error>>,
        /// If set, `create_invalid_deliveries` returns this error once then clears it.
        invalid_error: Mutex<Option<crate::error::Error>>,
    }

    impl MockRepo {
        fn new(events: Vec<RawEvent>, initial_cursor: i64) -> Self {
            Self {
                events: Mutex::new(events.into()),
                ensured: Mutex::new(vec![]),
                invalids: Mutex::new(vec![]),
                cursor: Mutex::new(initial_cursor),
                ensure_error: Mutex::new(None),
                invalid_error: Mutex::new(None),
            }
        }

        fn with_ensure_error(self, e: crate::error::Error) -> Self {
            *self.ensure_error.lock().unwrap() = Some(e);
            self
        }

        fn with_invalid_error(self, e: crate::error::Error) -> Self {
            *self.invalid_error.lock().unwrap() = Some(e);
            self
        }

        fn simulated_db_error() -> crate::error::Error {
            crate::error::Error::InvalidData("simulated DB failure".into())
        }

        fn ensured_deliveries(&self) -> Vec<(Uuid, Vec<String>)> {
            self.ensured.lock().unwrap().clone()
        }

        fn invalid_deliveries(&self) -> Vec<(Uuid, Vec<(String, String)>)> {
            self.invalids.lock().unwrap().clone()
        }

        /// Flatten all (callback_name, reason) pairs recorded across all calls.
        fn flat_invalids(&self) -> Vec<(Uuid, String, String)> {
            self.invalids
                .lock()
                .unwrap()
                .iter()
                .flat_map(|(eid, entries)| {
                    entries
                        .iter()
                        .map(|(n, r)| (*eid, n.clone(), r.clone()))
                        .collect::<Vec<_>>()
                })
                .collect()
        }
    }

    #[async_trait]
    impl Repo for MockRepo {
        async fn fetch_new_events(
            &self,
            after_id: i64,
            _limit: i64,
        ) -> crate::error::Result<Vec<RawEvent>> {
            let events: Vec<RawEvent> = self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.id > after_id)
                .cloned()
                .collect();
            Ok(events)
        }

        async fn ensure_deliveries(
            &self,
            event_id: Uuid,
            callbacks: &[CallbackTarget],
        ) -> crate::error::Result<()> {
            if let Some(e) = self.ensure_error.lock().unwrap().take() {
                return Err(e);
            }
            let names = callbacks.iter().map(|c| c.name.clone()).collect();
            self.ensured.lock().unwrap().push((event_id, names));
            Ok(())
        }

        async fn create_invalid_delivery(
            &self,
            event_id: Uuid,
            callback_name: &str,
            reason: &str,
        ) -> crate::error::Result<()> {
            self.invalids.lock().unwrap().push((
                event_id,
                vec![(callback_name.to_string(), reason.to_string())],
            ));
            Ok(())
        }

        async fn create_invalid_deliveries(
            &self,
            event_id: Uuid,
            entries: &[(String, String)],
        ) -> crate::error::Result<()> {
            if let Some(e) = self.invalid_error.lock().unwrap().take() {
                return Err(e);
            }
            if !entries.is_empty() {
                self.invalids
                    .lock()
                    .unwrap()
                    .push((event_id, entries.to_vec()));
            }
            Ok(())
        }

        async fn recover_cursor(&self) -> crate::error::Result<i64> {
            Ok(*self.cursor.lock().unwrap())
        }

        // ── unused in scheduler tests ─────────────────────────────────────────

        async fn fetch_due_deliveries(
            &self,
            _batch_size: i64,
        ) -> crate::error::Result<Vec<DueDelivery>> {
            Ok(vec![])
        }

        async fn lock_delivery(
            &self,
            _id: i64,
            _until: chrono::DateTime<Utc>,
        ) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn mark_dispatched_managed(&self, _id: i64) -> crate::error::Result<()> {
            Ok(())
        }

        async fn mark_dispatched_external(&self, _id: i64) -> crate::error::Result<()> {
            Ok(())
        }

        async fn mark_failure(
            &self,
            _id: i64,
            _error: &str,
            _available_at: chrono::DateTime<Utc>,
            _dead_letter: bool,
        ) -> crate::error::Result<()> {
            Ok(())
        }

        async fn reset_hung_external(
            &self,
            _now: chrono::DateTime<Utc>,
            _max_completion_cycles: i32,
        ) -> crate::error::Result<SweepReport> {
            Ok(SweepReport::default())
        }

        async fn list_dead_letters(
            &self,
            _page: PageParams,
        ) -> crate::error::Result<Vec<DeadLetterRow>> {
            Ok(vec![])
        }

        async fn list_external_pending(
            &self,
            _page: PageParams,
            _older_than: Option<Duration>,
        ) -> crate::error::Result<Vec<ExternalPendingRow>> {
            Ok(vec![])
        }

        async fn retry_delivery(
            &self,
            _id: i64,
        ) -> crate::error::Result<crate::schema::RetryOutcome> {
            Ok(crate::schema::RetryOutcome::NotFound)
        }

        async fn complete_delivery(&self, _id: i64) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn abandon_delivery(&self, _id: i64) -> crate::error::Result<bool> {
            Ok(false)
        }

        async fn fetch_event_with_deliveries(
            &self,
            event_id: Uuid,
        ) -> crate::error::Result<EventWithDeliveries> {
            Err(crate::error::Error::InvalidData(format!(
                "event {event_id} not found"
            )))
        }

        async fn ping(&self) -> crate::error::Result<()> {
            Ok(())
        }

        async fn fetch_stats(&self) -> crate::error::Result<crate::schema::Stats> {
            Ok(crate::schema::Stats {
                events_total: 0,
                deliveries_pending: 0,
                deliveries_external_pending: 0,
                deliveries_dead_lettered: 0,
                oldest_pending_age_seconds: None,
                callbacks: std::collections::HashMap::new(),
            })
        }

        async fn sample_external_pending_ages(
            &self,
            _sample_size: i64,
        ) -> crate::error::Result<Vec<(String, f64)>> {
            Ok(Vec::new())
        }

        async fn delete_terminal_events(
            &self,
            _dead_letter_cutoff: chrono::DateTime<Utc>,
            _processed_cutoff: chrono::DateTime<Utc>,
            _batch_limit: i64,
        ) -> crate::error::Result<crate::schema::RetentionDeleted> {
            Ok(crate::schema::RetentionDeleted::default())
        }

        async fn oldest_terminal_event_age_seconds(
            &self,
            _dead_letter_cutoff: chrono::DateTime<Utc>,
            _processed_cutoff: chrono::DateTime<Utc>,
        ) -> crate::error::Result<Option<f64>> {
            Ok(None)
        }
    }

    fn make_event(id: i64, callbacks: serde_json::Value, payload_bytes: i64) -> RawEvent {
        RawEvent {
            id,
            event_id: Uuid::new_v4(),
            kind: "test.event@v1".to_string(),
            aggregate_type: "test".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({"test": "data"}),
            metadata: json!({}),
            callbacks,
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: payload_bytes,
        }
    }

    fn config() -> DispatchConfig {
        DispatchConfig {
            allow_unsigned_callbacks: true,
            allow_private_ip_targets: true,
            ..Default::default()
        }
    }

    // ── Happy-path: valid callbacks are expanded into delivery rows ───────────

    #[tokio::test]
    async fn valid_callbacks_are_scheduled() {
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            callbacks: json!([
                {"name": "notify", "url": "https://example.com/hook"},
                {"name": "log_cb", "url": "https://log.example.com/hook"}
            ]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].0, event_id);
        let mut names = ensured[0].1.clone();
        names.sort();
        assert_eq!(names, vec!["log_cb", "notify"]);
        assert!(repo.invalid_deliveries().is_empty());
    }

    // ── Cursor advances correctly ─────────────────────────────────────────────

    #[tokio::test]
    async fn cursor_advances_to_last_event_id() {
        let events = vec![
            make_event(
                10,
                json!([{"name": "abc", "url": "https://a.example.com/"}]),
                10,
            ),
            make_event(
                20,
                json!([{"name": "def", "url": "https://b.example.com/"}]),
                10,
            ),
        ];
        let repo = MockRepo::new(events, 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();
        assert_eq!(new_cursor, 20);
    }

    #[tokio::test]
    async fn no_new_events_returns_same_cursor() {
        let repo = MockRepo::new(vec![], 5);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 5).await.unwrap();
        assert_eq!(new_cursor, 5);
    }

    // ── Payload-size guard ────────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_payload_dead_letters_all_callbacks() {
        let event = make_event(
            1,
            json!([
                {"name": "notify", "url": "https://a.example.com/"},
                {"name": "log_cb", "url": "https://b.example.com/"}
            ]),
            2_000_000, // 2 MB > 1 MB limit
        );
        let event_id = event.event_id;

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        // Cursor still advances past the oversized event.
        assert_eq!(new_cursor, 1);

        // No valid deliveries created.
        assert!(repo.ensured_deliveries().is_empty());

        // Both callbacks dead-lettered with the source_payload_too_large prefix.
        let invalids = repo.flat_invalids();
        assert_eq!(invalids.len(), 2, "expected 2 dead-lettered callbacks");
        for (eid, _name, reason) in &invalids {
            assert_eq!(*eid, event_id);
            assert!(
                reason.starts_with("source_payload_too_large:"),
                "got: {reason}"
            );
        }
        let mut names: Vec<_> = invalids.iter().map(|(_, n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["log_cb", "notify"]);
    }

    // ── Invalid callbacks dead-lettered at schedule time ─────────────────────

    #[tokio::test]
    async fn invalid_callback_is_dead_lettered() {
        let cfg = DispatchConfig::default(); // allow_unsigned_callbacks = false
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            // Missing required signing_key_id — structurally invalid with default config.
            callbacks: json!([{"name": "bad", "url": "https://a.example.com/"}]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &cfg, 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        assert!(repo.ensured_deliveries().is_empty());
        let invalids = repo.flat_invalids();
        assert_eq!(invalids.len(), 1);
        assert_eq!(invalids[0].0, event_id);
        assert_eq!(invalids[0].1, "bad");
        assert!(invalids[0].2.starts_with("invalid_callback:"));
    }

    // ── Mixed valid and invalid callbacks in the same event ───────────────────

    #[tokio::test]
    async fn mixed_callbacks_split_correctly() {
        let event_id = Uuid::new_v4();
        let event = RawEvent {
            id: 1,
            event_id,
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            callbacks: json!([
                {"name": "good", "url": "https://example.com/hook"},
                {"name": "1bad", "url": "https://example.com/other"}  // invalid name
            ]),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
            payload_size_bytes: 10,
        };

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].1, vec!["good"]);
        let invalids = repo.flat_invalids();
        assert_eq!(invalids.len(), 1);
        assert!(invalids[0].2.starts_with("invalid_callback:"));
    }

    // ── Poison-pill: non-array JSONB is skipped without crashing ─────────────

    #[tokio::test]
    async fn non_array_callbacks_is_poison_pill_skipped() {
        // `parse_callbacks` returns only a "<root>" invalid entry for non-array JSONB.
        // The scheduler must skip it as a poison pill without dead-lettering under
        // the event_id, since we have no callback names to associate.
        let event = make_event(1, json!({"not": "an_array"}), 10);

        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        // Cursor still advances.
        assert_eq!(new_cursor, 1);
        // No deliveries created — poison pill skipped.
        assert!(repo.ensured_deliveries().is_empty());
        assert!(repo.invalid_deliveries().is_empty());
    }

    // ── Cursor does not go backwards ─────────────────────────────────────────

    #[tokio::test]
    async fn events_before_cursor_are_not_returned() {
        let events = vec![
            make_event(
                3,
                json!([{"name": "abc", "url": "https://a.example.com/"}]),
                10,
            ),
            make_event(
                7,
                json!([{"name": "def", "url": "https://b.example.com/"}]),
                10,
            ),
        ];
        let repo = MockRepo::new(events, 0);

        // Call with cursor=5 — only event id=7 should be returned.
        let new_cursor = schedule_new_deliveries(&repo, &config(), 5).await.unwrap();
        assert_eq!(new_cursor, 7);
        let ensured = repo.ensured_deliveries();
        assert_eq!(ensured.len(), 1);
        assert_eq!(ensured[0].1, vec!["def"]);
    }

    // ── Finding 6: cursor does NOT advance past DB write failures ─────────────

    #[tokio::test]
    async fn db_error_on_ensure_deliveries_stops_cursor_advance() {
        // Two events: id=1 (will fail DB write), id=2 (should never be reached).
        let events = vec![
            make_event(
                1,
                json!([{"name": "notify", "url": "https://a.example.com/"}]),
                10,
            ),
            make_event(
                2,
                json!([{"name": "notify", "url": "https://b.example.com/"}]),
                10,
            ),
        ];
        let repo = MockRepo::new(events, 0).with_ensure_error(MockRepo::simulated_db_error());

        let result = schedule_new_deliveries(&repo, &config(), 0).await;

        // Must propagate the error.
        assert!(result.is_err());
        // The cursor must NOT have advanced (stays at 0).
        // Nothing was recorded since ensure_deliveries failed before we could push.
        assert!(repo.ensured_deliveries().is_empty());
    }

    #[tokio::test]
    async fn db_error_on_create_invalid_deliveries_stops_cursor_advance() {
        let cfg = DispatchConfig::default(); // allow_unsigned_callbacks = false
        let event = make_event(
            1,
            json!([{"name": "bad", "url": "https://a.example.com/"}]),
            10,
        );
        let repo = MockRepo::new(vec![event], 0).with_invalid_error(MockRepo::simulated_db_error());

        let result = schedule_new_deliveries(&repo, &cfg, 0).await;

        // Must propagate the error.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn db_error_on_oversized_payload_stops_cursor_advance() {
        let event = make_event(
            1,
            json!([{"name": "a", "url": "https://a.example.com/"}]),
            2_000_000,
        );
        let repo = MockRepo::new(vec![event], 0).with_invalid_error(MockRepo::simulated_db_error());

        let result = schedule_new_deliveries(&repo, &config(), 0).await;

        assert!(result.is_err());
    }

    // ── Finding 7: batch dead-lettering uses create_invalid_deliveries ────────

    #[tokio::test]
    async fn oversized_payload_uses_single_batch_call() {
        let event = make_event(
            1,
            json!([
                {"name": "a", "url": "https://a.example.com/"},
                {"name": "b", "url": "https://b.example.com/"},
                {"name": "c", "url": "https://c.example.com/"}
            ]),
            2_000_000,
        );
        let repo = MockRepo::new(vec![event], 0);
        schedule_new_deliveries(&repo, &config(), 0).await.unwrap();

        // All 3 callbacks should arrive in a single create_invalid_deliveries call.
        let invalids = repo.invalid_deliveries();
        assert_eq!(invalids.len(), 1, "expected exactly one batched call");
        assert_eq!(invalids[0].1.len(), 3);
    }

    // ── Finding 8: too-many-callbacks dead-letters each entry ─────────────────

    #[tokio::test]
    async fn too_many_callbacks_dead_letters_all_entries() {
        let cfg = DispatchConfig {
            max_callbacks_per_event: 1,
            allow_unsigned_callbacks: true,
            allow_private_ip_targets: true,
            ..Default::default()
        };
        let event = make_event(
            1,
            json!([
                {"name": "first",  "url": "https://a.example.com/"},
                {"name": "second", "url": "https://b.example.com/"}
            ]),
            10,
        );
        let event_id = event.event_id;
        let repo = MockRepo::new(vec![event], 0);
        let new_cursor = schedule_new_deliveries(&repo, &cfg, 0).await.unwrap();

        assert_eq!(new_cursor, 1);
        assert!(repo.ensured_deliveries().is_empty());

        let invalids = repo.flat_invalids();
        assert_eq!(invalids.len(), 2, "expected 2 dead-lettered callbacks");
        for (eid, _, reason) in &invalids {
            assert_eq!(*eid, event_id);
            assert!(
                reason.contains("too many callbacks"),
                "expected too-many-callbacks reason, got: {reason}"
            );
        }
        let mut names: Vec<_> = invalids.iter().map(|(_, n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["first", "second"]);
    }
}
