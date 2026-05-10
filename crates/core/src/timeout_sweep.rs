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
use crate::repo::Repo;

/// Run one sweep cycle: reset hung external-mode rows and/or dead-letter exhausted ones.
///
/// Logs outcomes and returns the `SweepReport` for metrics (Phase 7). Errors from
/// the repo are returned to the caller so the main loop can decide how to handle them.
pub async fn sweep_hung_external(repo: &dyn Repo, config: &DispatchConfig) -> Result<()> {
    let report = repo
        .reset_hung_external(Utc::now(), config.max_completion_cycles as i32)
        .await?;

    if report.reset > 0 {
        info!(
            reset = report.reset,
            "external timeout sweep reset deliveries for redelivery"
        );
    }

    if report.exhausted > 0 {
        warn!(
            exhausted = report.exhausted,
            "external timeout sweep dead-lettered rows after max_completion_cycles"
        );
    }

    if report.reset == 0 && report.exhausted == 0 {
        return Ok(());
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

        async fn retry_delivery(&self, _: i64) -> CoreResult<bool> {
            Ok(false)
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
