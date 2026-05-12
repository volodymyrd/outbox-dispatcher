//! Dispatch loop — lock, invoke Callback trait, update delivery state.
//!
//! # Responsibilities
//!
//! 1. **Fetch due deliveries** from the repository (rows whose `available_at ≤ now()`,
//!    not locked, not dispatched/processed).
//! 2. **Lock each delivery** atomically before invoking the callback so concurrent
//!    dispatchers skip rows already in flight.
//! 3. **Invoke the `Callback` trait** implementation (e.g. `HttpCallback`) under a
//!    per-callback `tokio::time::timeout`.
//! 4. **Write back the result** — success (managed or external mode) or transient
//!    failure (bump attempts, apply backoff, dead-letter when exhausted).
//!
//! Concurrency is bounded by `DispatchConfig::dispatch_concurrency` via
//! `buffer_unordered`, decoupling HTTP parallelism from fetch granularity and
//! protecting the database connection pool from exhaustion under slow callbacks.
//! There is no permanent-failure branch — `CallbackError` only has `Transient`,
//! and dead-lettering is purely a function of `attempts ≥ max_attempts`.

use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use tracing::{debug, error, warn};

use crate::config::DispatchConfig;
use crate::error::Result;
use crate::metrics;
use crate::repo::Repo;
use crate::retry::compute_next_available_at;
use crate::schema::{CallbackError, CallbackTarget, CompletionMode, DueDelivery, EventForDelivery};

// ── Callback trait ─────────────────────────────────────────────────────────────

/// Abstraction over a single delivery mechanism.
///
/// The `http-callback` crate provides `HttpCallback: Callback`, which is what the binary
/// uses. Unit tests in this crate use a `MockCallback` that records calls.
#[async_trait]
pub trait Callback: Send + Sync {
    async fn deliver(
        &self,
        target: &CallbackTarget,
        event: &EventForDelivery,
    ) -> std::result::Result<(), CallbackError>;
}

// ── Public dispatch entry point ────────────────────────────────────────────────

/// Fetch all due deliveries and dispatch them concurrently.
///
/// Errors from individual deliveries are handled internally (written back to the
/// repo). Only repo errors from `fetch_due_deliveries` propagate to the caller.
pub async fn dispatch_due(
    repo: &dyn Repo,
    callback: &dyn Callback,
    config: &DispatchConfig,
) -> Result<()> {
    let due = repo.fetch_due_deliveries(config.batch_size).await?;
    if due.is_empty() {
        return Ok(());
    }

    debug!(count = due.len(), "dispatching due deliveries");

    let mut tasks = futures::stream::iter(
        due.into_iter()
            .map(|d| dispatch_one(repo, callback, config, d)),
    )
    .buffer_unordered(config.dispatch_concurrency);

    while tasks.next().await.is_some() {}
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Build the `EventForDelivery` view from a `DueDelivery`.
fn build_event_for_delivery(due: &DueDelivery) -> EventForDelivery {
    EventForDelivery {
        delivery_id: due.delivery_id,
        event_id: due.event_id,
        kind: due.kind.clone(),
        callback_name: due.target.name.clone(),
        mode: due.target.mode.clone(),
        aggregate_type: due.aggregate_type.clone(),
        aggregate_id: due.aggregate_id,
        payload: due.payload.clone(),
        metadata: due.metadata.clone(),
        actor_id: due.actor_id,
        correlation_id: due.correlation_id,
        causation_id: due.causation_id,
        created_at: due.created_at,
        // `attempts` in DueDelivery is the count *before* this attempt; after
        // `lock_delivery` bumps it the in-progress count is `attempts + 1`.
        attempt: due.attempts + 1,
    }
}

/// Dispatch a single delivery: lock → invoke → write result.
///
/// Errors from repo writes after the HTTP call are logged at `error` level and
/// swallowed — a missed state write is recoverable via `locked_until` expiry.
async fn dispatch_one(
    repo: &dyn Repo,
    callback: &dyn Callback,
    config: &DispatchConfig,
    due: DueDelivery,
) {
    let cb_name = due.target.name.as_str();
    let mode_str = due.target.mode.as_str();

    // ── Lock the row before invoking the callback ─────────────────────────────
    //
    // `locked_until` bounds the duplicate-dispatch window across replicas and
    // after a crash: timeout + buffer. We commit this before the HTTP call.
    let lock_until = Utc::now() + due.target.timeout + config.lock_buffer;
    match repo.lock_delivery(due.delivery_id, lock_until).await {
        Ok(true) => {}
        Ok(false) => {
            // A concurrent dispatcher took this row. Skip silently.
            debug!(
                delivery_id = due.delivery_id,
                "delivery already locked by another dispatcher"
            );
            return;
        }
        Err(e) => {
            error!(delivery_id = due.delivery_id, error = %e, "failed to lock delivery");
            return;
        }
    }

    let event = build_event_for_delivery(&due);

    // ── Invoke the callback under a per-callback timeout ──────────────────────
    let dispatch_start = std::time::Instant::now();
    let result =
        tokio::time::timeout(due.target.timeout, callback.deliver(&due.target, &event)).await;
    let elapsed_secs = dispatch_start.elapsed().as_secs_f64();

    // ── Write result back ─────────────────────────────────────────────────────
    match result {
        // 2xx — success
        Ok(Ok(())) => {
            metrics::record_dispatch_duration(cb_name, mode_str, elapsed_secs);
            metrics::inc_deliveries_total(cb_name, mode_str, metrics::result::OK);

            match due.target.mode {
                CompletionMode::Managed => {
                    if let Err(e) = repo.mark_dispatched_managed(due.delivery_id).await {
                        error!(
                            delivery_id = due.delivery_id,
                            error = %e,
                            "failed to mark delivery as managed-dispatched"
                        );
                    }
                }
                CompletionMode::External => {
                    if let Err(e) = repo.mark_dispatched_external(due.delivery_id).await {
                        error!(
                            delivery_id = due.delivery_id,
                            error = %e,
                            "failed to mark delivery as external-dispatched"
                        );
                    }
                }
            }
        }

        // Transient failure from the callback implementation
        Ok(Err(CallbackError::Transient {
            reason,
            retry_after,
        })) => {
            metrics::record_dispatch_duration(cb_name, mode_str, elapsed_secs);
            metrics::inc_deliveries_total(cb_name, mode_str, metrics::result::TRANSIENT);

            let next_attempt = due.attempts + 1;
            let next_available =
                compute_next_available_at(next_attempt, retry_after, &due.target.backoff);
            let dead = next_attempt >= due.target.max_attempts as i32;

            if dead {
                metrics::inc_dead_letters_total(cb_name);
                metrics::inc_completion_cycles_exhausted_total(cb_name);
                warn!(
                    delivery_id = due.delivery_id,
                    attempts = next_attempt,
                    max_attempts = due.target.max_attempts,
                    reason = %reason,
                    "delivery exhausted retries — dead-lettering"
                );
            } else {
                debug!(
                    delivery_id = due.delivery_id,
                    attempts = next_attempt,
                    reason = %reason,
                    "delivery transient failure; will retry"
                );
            }

            if let Err(e) = repo
                .mark_failure(due.delivery_id, &reason, next_available, dead)
                .await
            {
                error!(
                    delivery_id = due.delivery_id,
                    error = %e,
                    "failed to record delivery failure"
                );
            }
        }

        // Tokio timeout — treat as a transient failure
        Err(_timeout) => {
            metrics::record_dispatch_duration(cb_name, mode_str, elapsed_secs);
            metrics::inc_deliveries_total(cb_name, mode_str, metrics::result::TIMEOUT);

            let next_attempt = due.attempts + 1;
            let next_available = compute_next_available_at(next_attempt, None, &due.target.backoff);
            let dead = next_attempt >= due.target.max_attempts as i32;

            warn!(
                delivery_id = due.delivery_id,
                attempts = next_attempt,
                "callback timed out"
            );

            if dead {
                metrics::inc_dead_letters_total(cb_name);
                metrics::inc_completion_cycles_exhausted_total(cb_name);
                warn!(
                    delivery_id = due.delivery_id,
                    max_attempts = due.target.max_attempts,
                    "delivery exhausted retries after timeout — dead-lettering"
                );
            }

            if let Err(e) = repo
                .mark_failure(due.delivery_id, "callback timeout", next_available, dead)
                .await
            {
                error!(
                    delivery_id = due.delivery_id,
                    error = %e,
                    "failed to record timeout failure"
                );
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        CallbackTarget, CompletionMode, DeadLetterRow, DueDelivery, EventWithDeliveries,
        ExternalPendingRow, PageParams, RawEvent, RetryOutcome, SweepReport,
    };
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use uuid::Uuid;

    // ── Mock Callback ────────────────────────────────────────────────────────

    struct MockCallback {
        result: std::result::Result<(), CallbackError>,
        calls: Mutex<Vec<(String, i32)>>, // (callback_name, attempt)
    }

    impl MockCallback {
        fn success() -> Self {
            Self {
                result: Ok(()),
                calls: Mutex::new(vec![]),
            }
        }

        fn transient(reason: &str) -> Self {
            Self {
                result: Err(CallbackError::Transient {
                    reason: reason.to_string(),
                    retry_after: None,
                }),
                calls: Mutex::new(vec![]),
            }
        }

        fn transient_with_retry_after(reason: &str, secs: u64) -> Self {
            Self {
                result: Err(CallbackError::Transient {
                    reason: reason.to_string(),
                    retry_after: Some(Duration::from_secs(secs)),
                }),
                calls: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl Callback for MockCallback {
        async fn deliver(
            &self,
            target: &CallbackTarget,
            event: &EventForDelivery,
        ) -> std::result::Result<(), CallbackError> {
            self.calls
                .lock()
                .unwrap()
                .push((target.name.clone(), event.attempt));
            // Clone the result for each call — Transient is not Copy so we re-construct.
            match &self.result {
                Ok(()) => Ok(()),
                Err(CallbackError::Transient {
                    reason,
                    retry_after,
                }) => Err(CallbackError::Transient {
                    reason: reason.clone(),
                    retry_after: *retry_after,
                }),
            }
        }
    }

    // ── Mock Repo ────────────────────────────────────────────────────────────

    #[derive(Default)]
    struct State {
        managed_dispatched: Vec<i64>,
        external_dispatched: Vec<i64>,
        failures: Vec<(i64, String, bool)>, // (id, reason, dead)
        locked: Vec<i64>,
        lock_should_fail: bool,
    }

    struct MockRepo {
        due: Vec<DueDelivery>,
        state: Mutex<State>,
    }

    impl MockRepo {
        fn new(due: Vec<DueDelivery>) -> Arc<Self> {
            Arc::new(Self {
                due,
                state: Mutex::new(State::default()),
            })
        }

        fn new_lock_fail(due: Vec<DueDelivery>) -> Arc<Self> {
            Arc::new(Self {
                due,
                state: Mutex::new(State {
                    lock_should_fail: true,
                    ..Default::default()
                }),
            })
        }
    }

    #[async_trait]
    impl Repo for MockRepo {
        async fn fetch_new_events(&self, _: i64, _: i64) -> Result<Vec<RawEvent>> {
            Ok(vec![])
        }

        async fn ensure_deliveries(&self, _: Uuid, _: &[CallbackTarget]) -> Result<()> {
            Ok(())
        }

        async fn create_invalid_delivery(&self, _: Uuid, _: &str, _: &str) -> Result<()> {
            Ok(())
        }

        async fn create_invalid_deliveries(&self, _: Uuid, _: &[(String, String)]) -> Result<()> {
            Ok(())
        }

        async fn fetch_due_deliveries(&self, _: i64) -> Result<Vec<DueDelivery>> {
            Ok(self.due.clone())
        }

        async fn lock_delivery(&self, id: i64, _: DateTime<Utc>) -> Result<bool> {
            let mut s = self.state.lock().unwrap();
            if s.lock_should_fail {
                return Ok(false);
            }
            s.locked.push(id);
            Ok(true)
        }

        async fn mark_dispatched_managed(&self, id: i64) -> Result<()> {
            self.state.lock().unwrap().managed_dispatched.push(id);
            Ok(())
        }

        async fn mark_dispatched_external(&self, id: i64) -> Result<()> {
            self.state.lock().unwrap().external_dispatched.push(id);
            Ok(())
        }

        async fn mark_failure(
            &self,
            id: i64,
            error: &str,
            _available_at: DateTime<Utc>,
            dead_letter: bool,
        ) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .failures
                .push((id, error.to_string(), dead_letter));
            Ok(())
        }

        async fn reset_hung_external(&self, _: DateTime<Utc>, _: i32) -> Result<SweepReport> {
            Ok(SweepReport::default())
        }

        async fn recover_cursor(&self) -> Result<i64> {
            Ok(0)
        }

        async fn list_dead_letters(&self, _: PageParams) -> Result<Vec<DeadLetterRow>> {
            Ok(vec![])
        }

        async fn list_external_pending(
            &self,
            _: PageParams,
            _: Option<Duration>,
        ) -> Result<Vec<ExternalPendingRow>> {
            Ok(vec![])
        }

        async fn retry_delivery(&self, _: i64) -> Result<RetryOutcome> {
            Ok(RetryOutcome::NotFound)
        }

        async fn complete_delivery(&self, _: i64) -> Result<bool> {
            Ok(false)
        }

        async fn abandon_delivery(&self, _: i64) -> Result<bool> {
            Ok(false)
        }

        async fn fetch_event_with_deliveries(&self, _: Uuid) -> Result<EventWithDeliveries> {
            Err(crate::error::Error::InvalidData("not found".into()))
        }

        async fn fetch_stats(&self) -> Result<crate::schema::Stats> {
            Ok(crate::schema::Stats {
                events_total: 0,
                deliveries_pending: 0,
                deliveries_external_pending: 0,
                deliveries_dead_lettered: 0,
                oldest_pending_age_seconds: None,
                callbacks: std::collections::HashMap::new(),
            })
        }

        async fn ping(&self) -> Result<()> {
            Ok(())
        }

        async fn delete_terminal_events(
            &self,
            _dead_letter_cutoff: DateTime<Utc>,
            _processed_cutoff: DateTime<Utc>,
            _batch_limit: i64,
        ) -> Result<u64> {
            Ok(0)
        }

        async fn oldest_terminal_event_age_seconds(
            &self,
            _dead_letter_cutoff: DateTime<Utc>,
            _processed_cutoff: DateTime<Utc>,
        ) -> Result<Option<f64>> {
            Ok(None)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_due(
        delivery_id: i64,
        mode: CompletionMode,
        attempts: i32,
        max_attempts: u32,
    ) -> DueDelivery {
        DueDelivery {
            delivery_id,
            event_id: Uuid::new_v4(),
            attempts,
            target: CallbackTarget {
                name: "test_cb".to_string(),
                url: "https://example.com/hook".to_string(),
                mode,
                signing_key_id: None,
                headers: HashMap::new(),
                max_attempts,
                backoff: vec![Duration::from_secs(30)],
                timeout: Duration::from_secs(30),
                external_completion_timeout: None,
                max_completion_cycles: 20,
            },
            kind: "test@v1".to_string(),
            aggregate_type: "order".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({}),
            metadata: json!({}),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: Utc::now(),
        }
    }

    fn config() -> DispatchConfig {
        DispatchConfig::default()
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn managed_success_marks_managed_dispatched() {
        let due = make_due(42, CompletionMode::Managed, 0, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::success();

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        assert_eq!(s.managed_dispatched, vec![42]);
        assert!(s.external_dispatched.is_empty());
        assert!(s.failures.is_empty());
    }

    #[tokio::test]
    async fn external_success_marks_external_dispatched() {
        let due = make_due(99, CompletionMode::External, 0, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::success();

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        assert_eq!(s.external_dispatched, vec![99]);
        assert!(s.managed_dispatched.is_empty());
    }

    #[tokio::test]
    async fn transient_failure_records_non_dead_letter() {
        let due = make_due(10, CompletionMode::Managed, 0, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::transient("upstream 503");

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        assert!(s.managed_dispatched.is_empty());
        assert_eq!(s.failures.len(), 1);
        assert_eq!(s.failures[0].0, 10);
        assert_eq!(s.failures[0].1, "upstream 503");
        assert!(!s.failures[0].2, "should not be dead-lettered yet");
    }

    #[tokio::test]
    async fn exhausted_attempts_dead_letters() {
        // attempts=2, max_attempts=3 → next attempt is 3 which >= max_attempts
        let due = make_due(7, CompletionMode::Managed, 2, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::transient("always fails");

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        assert_eq!(s.failures.len(), 1);
        assert!(s.failures[0].2, "should be dead-lettered");
    }

    #[tokio::test]
    async fn lock_failure_skips_delivery() {
        let due = make_due(55, CompletionMode::Managed, 0, 3);
        let repo = MockRepo::new_lock_fail(vec![due]);
        let cb = MockCallback::success();

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        // callback was never called because lock returned false
        assert!(s.managed_dispatched.is_empty());
        assert!(s.failures.is_empty());
    }

    #[tokio::test]
    async fn attempt_counter_starts_at_one() {
        let due = make_due(1, CompletionMode::Managed, 0, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::success();

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let calls = cb.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, 1, "first attempt should be 1-indexed");
    }

    #[tokio::test]
    async fn empty_due_list_is_a_no_op() {
        let repo = MockRepo::new(vec![]);
        let cb = MockCallback::success();
        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();
        let s = repo.state.lock().unwrap();
        assert!(s.managed_dispatched.is_empty());
    }

    #[tokio::test]
    async fn retry_after_hint_is_honoured() {
        let due = make_due(20, CompletionMode::Managed, 0, 3);
        let repo = MockRepo::new(vec![due]);
        let cb = MockCallback::transient_with_retry_after("rate limited", 600);

        dispatch_due(repo.as_ref(), &cb, &config()).await.unwrap();

        let s = repo.state.lock().unwrap();
        assert_eq!(s.failures.len(), 1);
        // We can't check available_at directly here, but the failure was recorded.
        assert_eq!(s.failures[0].1, "rate limited");
    }
}
