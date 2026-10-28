//! Subscription quota per provider. Subscriptions refuse runs until a window
//! refreshes, so the scheduler must know when a provider can run again.
//!
//! Stored state is a snapshot. `effective` answers the question the scheduler
//! asks: is this provider usable at time `now`?

use crate::model::QuotaState;
use chrono::{DateTime, Duration, Utc};

pub const MIN_BACKOFF_MINUTES: u32 = 5;
pub const MAX_BACKOFF_MINUTES: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderQuota {
    pub provider: String,
    pub state: QuotaState,
    pub retry_at: Option<DateTime<Utc>>,
    /// Where the reset time came from: `message`, `window` or `backoff`.
    pub source: Option<String>,
    pub backoff_minutes: u32,
    pub updated_at: DateTime<Utc>,
}

/// What an adapter found in a run's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitSignal {
    /// Reset time quoted by the provider, if any.
    pub reset_at: Option<DateTime<Utc>>,
    /// The line that matched, for display.
    pub excerpt: String,
}

pub fn fresh(provider: &str, now: DateTime<Utc>) -> ProviderQuota {
    ProviderQuota {
        provider: provider.to_string(),
        state: QuotaState::Available,
        retry_at: None,
        source: None,
        backoff_minutes: 0,
        updated_at: now,
    }
}

/// Quota after a provider refused a run.
///
/// Preference order: the reset time the provider quoted, then the plan's
/// window length from now, then exponential back-off (5, 10, 20 ... 60 min).
pub fn on_limit(
    prev: &ProviderQuota,
    signal: &LimitSignal,
    now: DateTime<Utc>,
    window: Option<std::time::Duration>,
) -> ProviderQuota {
    let mut next = prev.clone();
    next.updated_at = now;
    if let Some(reset_at) = signal.reset_at.filter(|r| *r > now) {
        next.state = QuotaState::Limited;
        next.retry_at = Some(reset_at);
        next.source = Some("message".into());
        next.backoff_minutes = 0;
    } else if let Some(window) = window {
        let length = Duration::from_std(window).unwrap_or_else(|_| Duration::hours(5));
        next.state = QuotaState::EstimatedLimited;
        next.retry_at = Some(now + length);
        next.source = Some("window".into());
        next.backoff_minutes = 0;
    } else {
        let backoff = match prev.backoff_minutes {
            0 => MIN_BACKOFF_MINUTES,
            n => (n * 2).min(MAX_BACKOFF_MINUTES),
        };
        next.state = QuotaState::Unknown;
        next.retry_at = Some(now + Duration::minutes(i64::from(backoff)));
        next.source = Some("backoff".into());
        next.backoff_minutes = backoff;
    }
    next
}

/// A run succeeded, so any earlier refusal is over.
pub fn on_success(prev: &ProviderQuota, now: DateTime<Utc>) -> ProviderQuota {
    ProviderQuota {
        provider: prev.provider.clone(),
        state: QuotaState::Available,
        retry_at: None,
        source: None,
        backoff_minutes: 0,
        updated_at: now,
    }
}

/// State at time `now`. Once a retry time has passed the provider is treated
/// as available again, and the next run acts as the probe.
pub fn effective(q: &ProviderQuota, now: DateTime<Utc>) -> QuotaState {
    match (q.state, q.retry_at) {
        (QuotaState::Available, _) => QuotaState::Available,
        (_, Some(retry)) if retry <= now => QuotaState::Available,
        (state, _) => state,
    }
}

pub fn is_available(q: &ProviderQuota, now: DateTime<Utc>) -> bool {
    effective(q, now) == QuotaState::Available
}

/// Earliest future time at which a refused provider becomes usable again.
pub fn next_wake(quotas: &[ProviderQuota], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    quotas
        .iter()
        .filter(|q| !is_available(q, now))
        .filter_map(|q| q.retry_at)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, h, m, s).unwrap()
    }

    fn signal(reset: Option<DateTime<Utc>>) -> LimitSignal {
        LimitSignal {
            reset_at: reset,
            excerpt: "limit reached".into(),
        }
    }

    #[test]
    fn quoted_reset_time_wins() {
        let now = at(10, 0, 0);
        let q = on_limit(&fresh("p", now), &signal(Some(at(13, 0, 0))), now, None);
        assert_eq!(q.state, QuotaState::Limited);
        assert_eq!(q.retry_at, Some(at(13, 0, 0)));
        assert_eq!(q.source.as_deref(), Some("message"));
    }

    #[test]
    fn window_is_used_when_no_time_is_quoted() {
        let now = at(10, 0, 0);
        let window = std::time::Duration::from_secs(5 * 3600);
        let q = on_limit(&fresh("p", now), &signal(None), now, Some(window));
        assert_eq!(q.state, QuotaState::EstimatedLimited);
        assert_eq!(q.retry_at, Some(at(15, 0, 0)));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let now = at(10, 0, 0);
        let mut q = on_limit(&fresh("p", now), &signal(None), now, None);
        assert_eq!(q.backoff_minutes, 5);
        for expected in [10, 20, 40, 60, 60] {
            q = on_limit(&q, &signal(None), now, None);
            assert_eq!(q.backoff_minutes, expected);
        }
    }

    #[test]
    fn stale_quoted_time_falls_back_to_backoff() {
        let now = at(10, 0, 0);
        let q = on_limit(&fresh("p", now), &signal(Some(at(9, 0, 0))), now, None);
        assert_eq!(q.state, QuotaState::Unknown);
        assert_eq!(q.source.as_deref(), Some("backoff"));
    }

    #[test]
    fn limited_becomes_available_after_retry_time() {
        let now = at(10, 0, 0);
        let q = on_limit(&fresh("p", now), &signal(Some(at(10, 30, 0))), now, None);
        assert!(!is_available(&q, at(10, 15, 0)));
        assert!(is_available(&q, at(10, 30, 0)));
        assert_eq!(effective(&q, at(10, 31, 0)), QuotaState::Available);
    }

    #[test]
    fn next_wake_is_earliest_refused_retry() {
        let now = at(10, 0, 0);
        let a = on_limit(&fresh("a", now), &signal(Some(at(12, 0, 0))), now, None);
        let b = on_limit(&fresh("b", now), &signal(Some(at(11, 0, 0))), now, None);
        let ok = fresh("c", now);
        assert_eq!(next_wake(&[a, b, ok], now), Some(at(11, 0, 0)));
    }

    #[test]
    fn success_clears_backoff() {
        let now = at(10, 0, 0);
        let q = on_limit(&fresh("p", now), &signal(None), now, None);
        let ok = on_success(&q, now);
        assert_eq!(ok.backoff_minutes, 0);
        assert_eq!(ok.state, QuotaState::Available);
    }
}
