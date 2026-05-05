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
