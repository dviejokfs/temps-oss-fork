//! The background task that drains the spool.
//!
//! Runs beside the instance's own work, so it is written to be a bad citizen of
//! nothing: bounded interval, exponential backoff on failure, and it never
//! holds a lock across a network call.

use std::sync::Arc;
use std::time::Duration;

use crate::link::{CloudLink, FlushOutcome};

/// Interval between flushes when everything is healthy.
pub const BASE_INTERVAL: Duration = Duration::from_secs(15);

/// Ceiling for backoff. A backend that has been down for an hour should be
/// polled every few minutes, not every fifteen seconds — but it must still be
/// polled, or recovery would need a restart to notice.
pub const MAX_INTERVAL: Duration = Duration::from_secs(300);

/// Next interval after an outcome.
///
/// Separated from the loop so the policy is testable without waiting on real
/// time — a sleeping test is a slow test and a flaky one.
pub fn next_interval(current: Duration, outcome: &FlushOutcome) -> Duration {
    match outcome {
        // Progress, or nothing to do: return to the base rate immediately.
        // Backing off after a success would leave a recovered backend receiving
        // telemetry minutes late for no reason.
        FlushOutcome::Shipped { .. } | FlushOutcome::Idle => BASE_INTERVAL,

        // Not linked: there is nothing to poll for. Slow all the way down, but
        // keep ticking so linking later is noticed without a restart.
        FlushOutcome::NotLinked => MAX_INTERVAL,

        // Transient failure: back off, capped.
        FlushOutcome::Retained { .. } => (current * 2).min(MAX_INTERVAL),

        // Permanent refusal. Backing off does not help — only the operator can
        // fix it — so poll at the base rate to pick up their fix promptly.
        FlushOutcome::Blocked { .. } => BASE_INTERVAL,
    }
}

/// Run until cancelled. Spawn this once at instance startup.
pub async fn run(link: Arc<CloudLink>, mut cancel: tokio::sync::watch::Receiver<bool>) {
    let mut interval = BASE_INTERVAL;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    // One last attempt on the way out, bounded: a clean
                    // shutdown should not lose a spool we could have delivered,
                    // but it also must not hang the process.
                    let _ = tokio::time::timeout(Duration::from_secs(5), link.flush()).await;
                    tracing::info!("cloud mirror stopped");
                    return;
                }
            }
        }

        let outcome = link.flush().await;
        interval = next_interval(interval, &outcome);

        match &outcome {
            FlushOutcome::Shipped { spans } => {
                tracing::debug!(spans, "mirrored telemetry");
            }
            FlushOutcome::Retained { spans, reason } => {
                tracing::warn!(
                    spans,
                    reason,
                    retry_in_secs = interval.as_secs(),
                    "buffering"
                );
            }
            FlushOutcome::Blocked { spans, reason } => {
                tracing::error!(spans, reason, "telemetry shipment needs operator action");
            }
            FlushOutcome::Idle | FlushOutcome::NotLinked => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transient_failure_backs_off_and_is_capped() {
        let mut d = BASE_INTERVAL;
        let retained = FlushOutcome::Retained {
            spans: 1,
            reason: "unreachable".into(),
        };

        d = next_interval(d, &retained);
        assert_eq!(d, BASE_INTERVAL * 2);

        for _ in 0..20 {
            d = next_interval(d, &retained);
        }
        assert_eq!(d, MAX_INTERVAL, "backoff must be bounded");
    }

    #[test]
    fn success_returns_to_the_base_rate_immediately() {
        // Staying backed off after recovery would deliver telemetry minutes
        // late for no reason.
        assert_eq!(
            next_interval(MAX_INTERVAL, &FlushOutcome::Shipped { spans: 10 }),
            BASE_INTERVAL
        );
        assert_eq!(
            next_interval(MAX_INTERVAL, &FlushOutcome::Idle),
            BASE_INTERVAL
        );
    }

    #[test]
    fn a_permanent_refusal_does_not_back_off() {
        // Only the operator can fix it, so poll at the base rate to pick up
        // their fix promptly rather than making them wait out a backoff.
        assert_eq!(
            next_interval(
                MAX_INTERVAL,
                &FlushOutcome::Blocked {
                    spans: 1,
                    reason: "re-enroll".into()
                }
            ),
            BASE_INTERVAL
        );
    }

    #[test]
    fn an_unlinked_instance_still_ticks() {
        // Slowly — but it must tick, or linking an account would need a restart
        // before anything shipped.
        let d = next_interval(BASE_INTERVAL, &FlushOutcome::NotLinked);
        assert_eq!(d, MAX_INTERVAL);
        assert!(d < Duration::from_secs(3600), "must still poll");
    }
}
