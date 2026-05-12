//! External-completion timeout sweeper.
//!
//! Periodically resets hung external-mode deliveries for redelivery, or dead-letters
//! them when `max_completion_cycles` is exhausted (§8.4).
//!
//! The sweeper runs at a rate-limited cadence controlled by
//! `DispatchConfig::external_timeout_sweep_interval`. Both the reset and dead-letter
//! branches are handled by a single SQL statement inside `PgRepo::reset_hung_external`;
//! this module is responsible for invoking it on the correct schedule and logging the
//! outcomes.

use chrono::Utc;
use tracing::{info, warn};

use crate::config::DispatchConfig;
use crate::error::Result;
use crate::metrics;
use crate::repo::Repo;

/// Run one sweep cycle: reset hung external-mode rows and/or dead-letter exhausted ones.
///
/// Emits `outbox_external_timeout_resets_total` and `outbox_completion_cycles_exhausted_total`
/// metrics. Errors from the repo are returned to the caller so the main loop can decide
/// how to handle them.
pub async fn sweep_hung_external(repo: &dyn Repo, config: &DispatchConfig) -> Result<()> {
    let report = repo
        .reset_hung_external(Utc::now(), config.max_completion_cycles as i32)
        .await?;

    if report.reset > 0 {
        info!(
            reset = report.reset,
            "external timeout sweep reset deliveries for redelivery"
        );
        // We don't have per-callback counts from the sweeper, so use "_all" as label.
        for _ in 0..report.reset {
            metrics::inc_external_timeout_resets_total("_all");
        }
    }
    if report.exhausted > 0 {
        warn!(
            exhausted = report.exhausted,
            "external timeout sweep dead-lettered rows after max_completion_cycles"
        );
        for _ in 0..report.exhausted {
            metrics::inc_completion_cycles_exhausted_total("_all");
        }
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DispatchConfig;
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

    struct SweepRepo {
        report: SweepReport,
        calls: Mutex<Vec<(DateTime<chrono::Utc>, i32)>>,
    }

    impl SweepRepo {
        fn new(reset: u64, exhausted: u64) -> Arc<Self> {
            Arc::new(Self {
                report: SweepReport { reset, exhausted },
                calls: Mutex::new(vec![]),
            })
        }
    }

    #[async_trait]
    impl crate::repo::Repo for SweepRepo {
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

        async fn lock_delivery(&self, _: i64, _: DateTime<chrono::Utc>) -> CoreResult<bool> {
            Ok(false)
        }

        async fn mark_dispatched_managed(&self, _: i64) -> CoreResult<()> {
            Ok(())
        }

        async fn mark_dispatched_external(&self, _: i64) -> CoreResult<()> {
            Ok(())
        }

        async fn mark_failure(
            &self,
            _: i64,
            _: &str,
            _: DateTime<chrono::Utc>,
            _: bool,
        ) -> CoreResult<()> {
            Ok(())
        }

        async fn reset_hung_external(
            &self,
            now: DateTime<chrono::Utc>,
            max_completion_cycles: i32,
        ) -> CoreResult<SweepReport> {
            self.calls
                .lock()
                .unwrap()
                .push((now, max_completion_cycles));
            Ok(SweepReport {
                reset: self.report.reset,
                exhausted: self.report.exhausted,
            })
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
            _dead_letter_cutoff: DateTime<chrono::Utc>,
            _processed_cutoff: DateTime<chrono::Utc>,
            _batch_limit: i64,
        ) -> CoreResult<u64> {
            Ok(0)
        }

        async fn oldest_terminal_event_age_seconds(
            &self,
            _dead_letter_cutoff: DateTime<chrono::Utc>,
            _processed_cutoff: DateTime<chrono::Utc>,
        ) -> CoreResult<Option<f64>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn sweep_calls_repo_with_config_max_cycles() {
        let cfg = DispatchConfig {
            max_completion_cycles: 5,
            ..Default::default()
        };
        let repo = SweepRepo::new(0, 0);
        sweep_hung_external(repo.as_ref(), &cfg).await.unwrap();

        let calls = repo.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1, 5,
            "max_completion_cycles should be passed through"
        );
    }

    #[tokio::test]
    async fn sweep_returns_ok_on_zero_report() {
        let cfg = DispatchConfig::default();
        let repo = SweepRepo::new(0, 0);
        assert!(sweep_hung_external(repo.as_ref(), &cfg).await.is_ok());
    }

    #[tokio::test]
    async fn sweep_returns_ok_with_activity() {
        let cfg = DispatchConfig::default();
        let repo = SweepRepo::new(3, 1);
        assert!(sweep_hung_external(repo.as_ref(), &cfg).await.is_ok());
    }
}
