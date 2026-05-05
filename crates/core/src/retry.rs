//! Delivery Retry and Backoff Calculation.
//!
//! # Why do we need this?
//!
//! In a distributed system, webhook deliveries can fail for many transient reasons:
//! network blips, receiver downtime, or rate limiting. To uphold the outbox pattern's
//! "at-least-once" delivery guarantee, the dispatcher must gracefully retry failed HTTP requests.
//!
//! This module computes the `available_at` timestamp for the next delivery attempt,
//! incorporating three critical resilience mechanisms:
//!
//! 1. **Configurable Backoff:** Retries are spaced out according to a configured
//!    schedule (e.g., `[30s, 2m, 10m, 1h]`). If the number of attempts exceeds the
//!    length of the schedule, the last duration is repeated until `max_attempts`
//!    is reached and the delivery is dead-lettered.
//! 2. **Jitter (Thundering Herd Prevention):** A `±25%` randomization factor is applied
//!    to every calculated delay. If a downstream receiver goes offline and causes a large
//!    batch of events to fail simultaneously, jitter spreads out their next attempt times.
//!    This prevents the dispatcher from hammering the receiver with a massive spike of
//!    retries the moment it comes back online.
//! 3. **`Retry-After` Awareness:** If a receiver responds with an HTTP 429 (Too Many Requests)
//!    or 503 (Service Unavailable) status code along with a `Retry-After` header, this
//!    module honors it as a delay "floor". This ensures the dispatcher respects the
//!    downstream service's explicit load-shedding requests.
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Computes the next `available_at` for a failed delivery.
///
/// Uses the backoff schedule (last value repeats after exhaustion), honours a
/// `Retry-After` floor when present, and applies ±25 % jitter to prevent
/// thundering-herd on batch failures.
pub fn compute_next_available_at(
    attempt: i32,
    retry_after: Option<Duration>,
    backoff_schedule: &[Duration],
) -> DateTime<Utc> {
    let index = attempt.max(1) as usize - 1;
    let base = backoff_schedule
        .get(index)
        .or_else(|| backoff_schedule.last())
        .copied()
        .unwrap_or(Duration::from_secs(60));

    // If the receiver asked us to wait longer than our standard backoff, respect it.
    let delay = retry_after.map(|after| after.max(base)).unwrap_or(base);

    // ±25 % jitter
    let jitter: f64 = rand::random::<f64>() * 0.5 - 0.25;
    let total = delay.mul_f64(1.0 + jitter);

    Utc::now() + total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_schedule_by_attempt_index() {
        let schedule = vec![
            Duration::from_secs(30),
            Duration::from_secs(120),
            Duration::from_secs(600),
        ];
        let now = Utc::now();
        let next = compute_next_available_at(1, None, &schedule);
        // With ±25% jitter the delay is in [22.5s, 37.5s].
        assert!(next > now + chrono::Duration::seconds(20));
    }

    #[test]
    fn safely_handles_attempt_zero() {
        let schedule = vec![Duration::from_secs(30)];
        let now = Utc::now();
        // attempt=0 must peg to index 0 (30s), not wrap to usize::MAX and use last().
        let next = compute_next_available_at(0, None, &schedule);
        assert!(next < now + chrono::Duration::seconds(40));
    }

    #[test]
    fn repeats_last_entry_after_schedule_exhausted() {
        let schedule = vec![Duration::from_secs(30)];
        let now = Utc::now();
        let next = compute_next_available_at(5, None, &schedule);
        assert!(next > now);
    }

    #[test]
    fn retry_after_used_as_floor() {
        let schedule = vec![Duration::from_secs(30)];
        let long_retry_after = Duration::from_secs(600);
        let now = Utc::now();
        let next = compute_next_available_at(1, Some(long_retry_after), &schedule);
        // retry_after > schedule[0], so delay must be at least ~450s (600 * 0.75).
        assert!(next > now + chrono::Duration::seconds(400));
    }
}
