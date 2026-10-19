//! Recognises subscription-limit refusals and reads the reset time from them.
//!
//! Formats handled, most specific first:
//! - a Unix epoch after `|`, as in `usage limit reached|1792483200`
//! - an ISO 8601 timestamp anywhere in the text
//! - "try again in N minutes/hours"
//! - "resets at 6:30 PM" or "try again at 18:30" (local server time)
//!
//! The formats are assumptions until the phase 0.5 spike records real output.
//! Anything unrecognised still counts as a limit, and the scheduler falls back
//! to the window length or to back-off.

use aura_core::LimitSignal;
use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone, Utc};
use regex::Regex;
use std::sync::OnceLock;

/// Phrases that mark a refusal. Matched case-insensitively.
pub const DEFAULT_LIMIT_PATTERNS: &[&str] = &[
    r"usage limit (reached|exceeded|hit)",
    r"limit (reached|exceeded)",
    r"you'?ve hit your .{0,30}limit",
    r"quota (exceeded|exhausted|reached)",
    r"rate[ -]limit",
];

const EXCERPT_MAX: usize = 200;

struct TimeRegexes {
    epoch: Regex,
    iso: Regex,
    relative: Regex,
    clock: Regex,
}

fn regexes() -> &'static TimeRegexes {
    static RE: OnceLock<TimeRegexes> = OnceLock::new();
    RE.get_or_init(|| TimeRegexes {
        epoch: Regex::new(r"\|(\d{10})\b").unwrap(),
        iso: Regex::new(
            r"\b(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2})?(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)",
        )
        .unwrap(),
        relative: Regex::new(r"(?i)try again (?:in|after) (\d+)\s*(second|minute|hour|day)s?")
            .unwrap(),
        clock: Regex::new(r"(?i)(?:resets?|try again)\s+at\s+(\d{1,2})(?::(\d{2}))?\s*([ap]m)?\b")
            .unwrap(),
    })
}

/// Finds a refusal in `output`. `patterns` must already be case-insensitive.
pub fn detect_limit(patterns: &[Regex], output: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    let excerpt = output
        .lines()
        .find(|line| patterns.iter().any(|p| p.is_match(line)))?
        .trim();
    let excerpt: String = excerpt.chars().take(EXCERPT_MAX).collect();
    Some(LimitSignal {
        reset_at: reset_time(output, now),
        excerpt,
    })
}

/// The reset time quoted in `text`, if any. Times that are already in the
/// past are not returned, because they cannot be the reset of a live refusal.
pub fn reset_time(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let re = regexes();

    if let Some(caps) = re.epoch.captures(text) {
        if let Ok(secs) = caps[1].parse::<i64>() {
            if let Some(t) = DateTime::from_timestamp(secs, 0).filter(|t| *t > now) {
                return Some(t);
            }
        }
    }

    if let Some(m) = re.iso.find(text) {
        if let Ok(t) = DateTime::parse_from_rfc3339(m.as_str()) {
            let t = t.with_timezone(&Utc);
            if t > now {
                return Some(t);
            }
        }
    }

    if let Some(caps) = re.relative.captures(text) {
        let n: i64 = caps[1].parse().ok()?;
        let delta = match caps[2].to_ascii_lowercase().as_str() {
            "second" => Duration::seconds(n),
            "minute" => Duration::minutes(n),
            "hour" => Duration::hours(n),
            "day" => Duration::days(n),
            _ => return None,
        };
        return Some(now + delta);
    }

    if let Some(caps) = re.clock.captures(text) {
        let hour: u32 = caps[1].parse().ok()?;
        let minute: u32 = caps.get(2).map_or(Some(0), |m| m.as_str().parse().ok())?;
        let hour = match caps.get(3).map(|m| m.as_str().to_ascii_lowercase()) {
            Some(ap) if ap == "pm" && hour < 12 => hour + 12,
            Some(ap) if ap == "am" && hour == 12 => 0,
            _ => hour,
        };
        return next_local_time(now, NaiveTime::from_hms_opt(hour, minute, 0)?);
    }

    None
}

/// The next occurrence of a local wall-clock time after `now`.
fn next_local_time(now: DateTime<Utc>, time: NaiveTime) -> Option<DateTime<Utc>> {
    let local_today = now.with_timezone(&Local).date_naive();
    for days in 0..=1 {
        let date = local_today + Duration::days(days);
        let candidate = Local
            .from_local_datetime(&date.and_time(time))
            .single()?
            .with_timezone(&Utc);
        if candidate > now {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn patterns() -> Vec<Regex> {
        DEFAULT_LIMIT_PATTERNS
            .iter()
            .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
            .collect()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 10, 0, 0).unwrap()
    }

    #[test]
    fn epoch_reset_is_read_from_fixture() {
        let text = include_str!("../tests/fixtures/claude-code-json-epoch.txt");
        let signal = detect_limit(&patterns(), text, now()).expect("limit detected");
        assert_eq!(signal.reset_at, DateTime::from_timestamp(1_792_483_200, 0));
        assert!(signal.excerpt.contains("usage limit reached"));
    }

    #[test]
    fn clock_time_is_read_from_fixture_in_local_time() {
        let text = include_str!("../tests/fixtures/codex-clock-time.txt");
        let signal = detect_limit(&patterns(), text, now()).expect("limit detected");
        let reset = signal.reset_at.expect("reset parsed");
        assert!(reset > now());
        let local = reset.with_timezone(&Local);
        assert_eq!((local.hour(), local.minute()), (18, 30));
    }

    #[test]
    fn relative_time_is_added_to_now() {
        let text = include_str!("../tests/fixtures/generic-relative.txt");
        let signal = detect_limit(&patterns(), text, now()).expect("limit detected");
        assert_eq!(signal.reset_at, Some(now() + Duration::minutes(45)));
    }

    #[test]
    fn plain_output_is_not_a_limit() {
        let text = include_str!("../tests/fixtures/no-limit.txt");
        assert!(detect_limit(&patterns(), text, now()).is_none());
    }

    #[test]
    fn limit_without_time_has_no_reset() {
        let signal = detect_limit(&patterns(), "rate limit exceeded", now()).unwrap();
        assert_eq!(signal.reset_at, None);
    }

    #[test]
    fn past_timestamp_is_ignored() {
        let text = "usage limit reached|1600000000";
        assert_eq!(reset_time(text, now()), None);
    }

    #[test]
    fn iso_timestamp_is_read() {
        let text = "quota exceeded until 2026-10-19T14:00:00Z, sorry";
        assert_eq!(
            reset_time(text, now()),
            Some(Utc.with_ymd_and_hms(2026, 10, 19, 14, 0, 0).unwrap())
        );
    }

    #[test]
    fn pm_conversion_handles_noon_and_midnight() {
        // 12 PM is noon, 12 AM is midnight.
        let noon = reset_time("rate limit. resets at 12 pm", now())
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(noon.hour(), 12);
        let midnight = reset_time("rate limit. resets at 12am", now())
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(midnight.hour(), 0);
    }

    #[test]
    fn excerpt_is_bounded() {
        let long = format!("rate limit {}", "x".repeat(500));
        let signal = detect_limit(&patterns(), &long, now()).unwrap();
        assert_eq!(signal.excerpt.chars().count(), EXCERPT_MAX);
    }
}
