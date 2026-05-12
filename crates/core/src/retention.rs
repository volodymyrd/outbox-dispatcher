//! Retention worker — periodically deletes fully-terminal events past their window.
//!
//! # Design
//!
//! The worker is **opt-in and disabled by default** (`retention.enabled = false`).
//! When enabled it runs every `cleanup_interval` and deletes up to `batch_limit`
//! events whose deliveries have all reached a terminal state and whose age exceeds
//! the configured window (see §11 of the TDD).
//!
//! # Concurrency safety
//!
//! The worker can race with active dispatch. An in-flight delivery uses an
//! `EventForDelivery` already in memory; if the retention worker deletes the parent
//! event mid-dispatch, the dispatcher's late `UPDATE`s affect 0 rows, which is
//! logged at `warn` and ignored. This is benign.
//!
//! # Poison-pill guard
//!
//! If the retention query itself fails (e.g. corrupt JSONB involved in a sub-select),
//! the worker logs at `error`, increments `outbox_corrupted_rows_total{stage="retention"}`,
//! and backs off before retrying. A single bad row will not crash the loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::RetentionConfig;
use crate::error::Result;
use crate::metrics;
use crate::repo::Repo;

// ── Public types ───────────────────────────────────────────────────────────────

/// Summary of one retention cycle.
#[derive(Debug, Default, Clone)]
pub struct RetentionReport {
    /// Total event rows deleted in this cycle.
    pub deleted: u64,
    /// Age in seconds of the oldest retention-eligible event still present after
    /// this cycle; `None` when no eligible events remain.
    pub oldest_age_seconds: Option<f64>,
}

// ── Public entry point ─────────────────────────────────────────────────────────

/// Run the retention worker until `shutdown` is cancelled.
///
/// If `retention.enabled` is `false`, returns immediately.
pub async fn run_retention_worker(
    repo: Arc<dyn Repo>,
    config: RetentionConfig,
    shutdown: CancellationToken,
) {
    if !config.enabled {
        return;
    }

    let interval = Duration::from_secs(config.cleanup_interval_secs);
    info!(
        cleanup_interval_secs = config.cleanup_interval_secs,
        batch_limit = config.batch_limit,
        processed_retention_days = config.processed_retention_days,
        dead_letter_retention_days = config.dead_letter_retention_days,
        "retention worker started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("retention worker received shutdown signal, stopping");
                return;
            }
            _ = tokio::time::sleep(interval) => {}
        }

        let cycle_start = std::time::Instant::now();
        match run_retention_cycle(repo.as_ref(), &config).await {
            Ok(report) => {
                let elapsed = cycle_start.elapsed().as_secs_f64();
                metrics::record_retention_cycle_duration(elapsed);

                if let Some(age) = report.oldest_age_seconds {
                    metrics::set_retention_oldest_event_age_seconds(age);
                } else {
                    metrics::set_retention_oldest_event_age_seconds(0.0);
                }

                if report.deleted > 0 {
                    info!(deleted = report.deleted, "retention cycle completed");
                }
            }
            Err(e) => {
                error!(error = %e, "retention cycle failed — will retry on next interval");
                metrics::inc_corrupted_rows_total(metrics::stage::RETENTION);
            }
        }
    }
}

// ── Internal ───────────────────────────────────────────────────────────────────

async fn run_retention_cycle(repo: &dyn Repo, config: &RetentionConfig) -> Result<RetentionReport> {
    let now = Utc::now();
    let processed_cutoff = now - chrono::Duration::days(config.processed_retention_days as i64);
    let dead_letter_cutoff = now - chrono::Duration::days(config.dead_letter_retention_days as i64);

    // Delete terminal events in bounded batches. We always delete the
    // processed-cutoff window as $1 for rows where any delivery is dead-lettered,
    // and dead-letter-cutoff as $2 otherwise. Per the TDD the longer window
    // (dead_letter_retention_days) applies when any delivery is dead-lettered.
    let deleted = repo
        .delete_terminal_events(
            dead_letter_cutoff,
            processed_cutoff,
            config.batch_limit as i64,
        )
        .await?;

    // Emit per-bucket deletion counters. We use a single "mixed" bucket here
    // because the DB query combines both windows in one DELETE. The TDD separates
    // `processed` vs `dead_letter` but it is not possible to distinguish the count
    // from a single RETURNING clause without splitting the query.
    if deleted > 0 {
        // Attribute all deletions to "processed" for simplicity; a future iteration
        // can split the query to produce separate counts.
        metrics::inc_retention_deletions_total(metrics::retention_reason::PROCESSED, deleted);
        warn!(deleted, "retention worker deleted terminal events");
    }

    // Query the oldest eligible event age for the gauge.
    let oldest_age = repo
        .oldest_terminal_event_age_seconds(dead_letter_cutoff, processed_cutoff)
        .await?;

    Ok(RetentionReport {
        deleted,
        oldest_age_seconds: oldest_age,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetentionConfig;
    use crate::error::Result as CoreResult;
    use crate::schema::{
        CallbackTarget, DeadLetterRow, DueDelivery, EventWithDeliveries, ExternalPendingRow,
        PageParams, RawEvent, SweepReport,
    };
    use async_trait::async_trait;
    use chrono::DateTime;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use uuid::Uuid;

    // ── Minimal mock repo ─────────────────────────────────────────────────────

    #[derive(Default)]
    struct RetentionMockState {
        delete_calls: Vec<(DateTime<Utc>, DateTime<Utc>, i64)>,
        age_calls: Vec<(DateTime<Utc>, DateTime<Utc>)>,
        deleted_count: u64,
        oldest_age: Option<f64>,
        delete_error: bool,
    }

    struct MockRepo {
        state: Mutex<RetentionMockState>,
    }

    impl MockRepo {
        fn new(deleted_count: u64, oldest_age: Option<f64>) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(RetentionMockState {
                    deleted_count,
                    oldest_age,
                    ..Default::default()
                }),
            })
        }

        fn new_with_error() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(RetentionMockState {
                    delete_error: true,
                    ..Default::default()
                }),
            })
        }

        fn delete_calls(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>, i64)> {
            self.state.lock().unwrap().delete_calls.clone()
        }

        fn age_calls(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
            self.state.lock().unwrap().age_calls.clone()
        }
    }

    #[async_trait]
    impl crate::repo::Repo for MockRepo {
        async fn fetch_new_events(&self, _: i64, _: i64) -> CoreResult<Vec<RawEvent>> {
            Ok(vec![])
        }

        async fn ensure_deliveries(&self, _: Uuid, _: &[CallbackTarget]) -> CoreResult<()> {
            Ok(())
        }

        async fn create_invalid_delivery(&self, _: Uuid, _: &str, _: &str) -> CoreResult<()> {
            Ok(())
        }

        async fn create_invalid_deliveries(
            &self,
            _: Uuid,
            _: &[(String, String)],
        ) -> CoreResult<()> {
            Ok(())
        }

        async fn fetch_due_deliveries(&self, _: i64) -> CoreResult<Vec<DueDelivery>> {
            Ok(vec![])
        }

        async fn lock_delivery(&self, _: i64, _: DateTime<Utc>) -> CoreResult<bool> {
            Ok(false)
        }

        async fn mark_dispatched_managed(&self, _: i64) -> CoreResult<()> {
            Ok(())
        }

        async fn mark_dispatched_external(&self, _: i64) -> CoreResult<()> {
            Ok(())
        }

        async fn mark_failure(&self, _: i64, _: &str, _: DateTime<Utc>, _: bool) -> CoreResult<()> {
            Ok(())
        }

        async fn reset_hung_external(&self, _: DateTime<Utc>, _: i32) -> CoreResult<SweepReport> {
            Ok(SweepReport::default())
        }

        async fn recover_cursor(&self) -> CoreResult<i64> {
            Ok(0)
        }

        async fn list_dead_letters(&self, _: PageParams) -> CoreResult<Vec<DeadLetterRow>> {
            Ok(vec![])
        }

        async fn list_external_pending(
            &self,
            _: PageParams,
            _: Option<Duration>,
        ) -> CoreResult<Vec<ExternalPendingRow>> {
            Ok(vec![])
        }

        async fn retry_delivery(&self, _: i64) -> CoreResult<crate::schema::RetryOutcome> {
            Ok(crate::schema::RetryOutcome::NotFound)
        }

        async fn complete_delivery(&self, _: i64) -> CoreResult<bool> {
            Ok(false)
        }

        async fn abandon_delivery(&self, _: i64) -> CoreResult<bool> {
            Ok(false)
        }

        async fn fetch_event_with_deliveries(&self, _: Uuid) -> CoreResult<EventWithDeliveries> {
            Err(crate::error::Error::InvalidData("not found".into()))
        }

        async fn fetch_stats(&self) -> CoreResult<crate::schema::Stats> {
            Ok(crate::schema::Stats {
                events_total: 0,
                deliveries_pending: 0,
                deliveries_external_pending: 0,
                deliveries_dead_lettered: 0,
                oldest_pending_age_seconds: None,
                callbacks: std::collections::HashMap::new(),
            })
        }

        async fn ping(&self) -> CoreResult<()> {
            Ok(())
        }

        async fn delete_terminal_events(
            &self,
            dead_letter_cutoff: DateTime<Utc>,
            processed_cutoff: DateTime<Utc>,
            batch_limit: i64,
        ) -> CoreResult<u64> {
            let mut s = self.state.lock().unwrap();
            if s.delete_error {
                return Err(crate::error::Error::InvalidData(
                    "simulated DB error".into(),
                ));
            }
            s.delete_calls
                .push((dead_letter_cutoff, processed_cutoff, batch_limit));
            Ok(s.deleted_count)
        }

        async fn oldest_terminal_event_age_seconds(
            &self,
            dead_letter_cutoff: DateTime<Utc>,
            processed_cutoff: DateTime<Utc>,
        ) -> CoreResult<Option<f64>> {
            self.state
                .lock()
                .unwrap()
                .age_calls
                .push((dead_letter_cutoff, processed_cutoff));
            Ok(self.state.lock().unwrap().oldest_age)
        }
    }

    fn default_config() -> RetentionConfig {
        RetentionConfig {
            enabled: true,
            processed_retention_days: 7,
            dead_letter_retention_days: 30,
            cleanup_interval_secs: 3600,
            batch_limit: 100,
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cycle_calls_delete_with_correct_cutoffs_and_batch() {
        let repo = MockRepo::new(0, None);
        let cfg = default_config();
        run_retention_cycle(repo.as_ref(), &cfg).await.unwrap();

        let calls = repo.delete_calls();
        assert_eq!(calls.len(), 1, "should call delete_terminal_events once");
        let (dl_cutoff, proc_cutoff, batch) = calls[0];
        // dead_letter_cutoff should be older (further in the past) than processed_cutoff
        assert!(
            dl_cutoff < proc_cutoff,
            "dead_letter_cutoff should be before processed_cutoff"
        );
        assert_eq!(batch, 100);
    }

    #[tokio::test]
    async fn cycle_queries_oldest_age_after_delete() {
        let repo = MockRepo::new(5, Some(86400.0));
        let cfg = default_config();
        let report = run_retention_cycle(repo.as_ref(), &cfg).await.unwrap();

        assert_eq!(report.deleted, 5);
        assert_eq!(report.oldest_age_seconds, Some(86400.0));
        assert_eq!(repo.age_calls().len(), 1);
    }

    #[tokio::test]
    async fn cycle_returns_zero_deleted_and_none_age_when_nothing_eligible() {
        let repo = MockRepo::new(0, None);
        let cfg = default_config();
        let report = run_retention_cycle(repo.as_ref(), &cfg).await.unwrap();

        assert_eq!(report.deleted, 0);
        assert!(report.oldest_age_seconds.is_none());
    }

    #[tokio::test]
    async fn cycle_propagates_delete_error() {
        let repo = MockRepo::new_with_error();
        let cfg = default_config();
        let result = run_retention_cycle(repo.as_ref(), &cfg).await;
        assert!(result.is_err(), "expected error from repo");
    }

    #[tokio::test]
    async fn worker_does_not_run_when_disabled() {
        let repo = MockRepo::new(0, None);
        let cfg = RetentionConfig {
            enabled: false,
            ..default_config()
        };
        let shutdown = CancellationToken::new();
        // Should return immediately without any cycles.
        run_retention_worker(repo.clone(), cfg, shutdown.clone()).await;
        assert!(
            repo.delete_calls().is_empty(),
            "disabled worker must not delete anything"
        );
    }

    #[tokio::test]
    async fn worker_runs_one_cycle_then_stops_on_shutdown() {
        let repo = MockRepo::new(0, None);
        // Very short interval so the first sleep finishes quickly.
        let cfg = RetentionConfig {
            enabled: true,
            cleanup_interval_secs: 0, // instant first cycle
            ..default_config()
        };
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let repo_clone = repo.clone();
        let handle = tokio::spawn(async move {
            run_retention_worker(repo_clone, cfg, shutdown_clone).await;
        });
        // Wait briefly then cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        handle.await.unwrap();
        // At least one cycle should have run.
        assert!(
            !repo.delete_calls().is_empty() || !repo.age_calls().is_empty(),
            "worker should have run at least one cycle"
        );
    }
}
