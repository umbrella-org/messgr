//! Quiet-hours window resolution (DESIGN.md §6.1, T-043) -- pure, so it is
//! unit-testable without the local stack. `src/dispatcher/worker.rs`'s quiet
//! hours gate is the only caller.

use std::str::FromStr;

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use rand::RngExt;

use super::model::QuietHoursPolicy;

const JITTER_MAX_MS: i64 = 30 * 60 * 1000; // 30 minutes, DESIGN.md §6.1's thundering-herd fix

/// ponytail: bounds the spring-forward round-forward search below -- covers
/// every real single-jump DST transition (usually one hour; the largest on
/// record, Portugal 1992, was two), not a full-day political re-zoning (e.g.
/// Samoa's Dec-2011 date-line jump). Widen if a future tz database ever
/// routes such a change through an ordinary local-time gap.
const MAX_GAP_SEARCH: Duration = Duration::hours(4);

/// `Some(next_attempt_at)` when `now` falls inside `policy`'s window for a
/// customer in `customer_tz` -- the UTC instant to reschedule to (window
/// end plus uniform jitter, decision matching §6.1's thundering-herd fix).
/// `None` means the send may proceed now.
///
/// `customer_tz` falls back to `tenant_default_tz` when it fails to parse as
/// an IANA zone (§6.1's "unknown timezone" case). If `tenant_default_tz`
/// *also* fails to parse, returns `None` (gate skipped, T-043 decision 2) --
/// a double misconfiguration DESIGN.md does not name, and a courtesy control
/// must not become an availability risk over one.
pub fn resolve_reschedule(
    now: DateTime<Utc>,
    customer_tz: &str,
    tenant_default_tz: &str,
    policy: &QuietHoursPolicy,
) -> Option<DateTime<Utc>> {
    let tz = Tz::from_str(customer_tz)
        .or_else(|_| Tz::from_str(tenant_default_tz))
        .ok()?;

    let local_now = now.with_timezone(&tz);
    let wraps = policy.start_local > policy.end_local;
    let in_window = if wraps {
        local_now.time() >= policy.start_local || local_now.time() < policy.end_local
    } else {
        local_now.time() >= policy.start_local && local_now.time() < policy.end_local
    };
    if !in_window {
        return None;
    }

    let end_date = if wraps && local_now.time() >= policy.start_local {
        local_now.date_naive() + Duration::days(1)
    } else {
        local_now.date_naive()
    };
    let window_end = resolve_local(&tz, end_date, policy.end_local)?;

    let jitter = Duration::milliseconds(rand::rng().random_range(0..=JITTER_MAX_MS));
    Some(window_end + jitter)
}

/// Resolves a local wall-clock time to its UTC instant. On a fall-back
/// ambiguity, takes the earlier offset (decision 7). On a spring-forward gap,
/// rounds forward a minute at a time to the next valid instant (§6.1's own
/// requirement), bounded by `MAX_GAP_SEARCH` (decision 8) -- `None` past the
/// bound, which `resolve_reschedule` treats as gate-skip via its own `?`.
fn resolve_local(tz: &Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let mut candidate = date.and_time(time);
    let deadline = candidate + MAX_GAP_SEARCH;
    loop {
        match tz.from_local_datetime(&candidate) {
            LocalResult::Single(dt) => return Some(dt.with_timezone(&Utc)),
            LocalResult::Ambiguous(earliest, _) => {
                return Some(earliest.with_timezone(&Utc));
            }
            LocalResult::None => {
                candidate += Duration::minutes(1);
                if candidate > deadline {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(start: &str, end: &str) -> QuietHoursPolicy {
        QuietHoursPolicy {
            scope: "default".to_string(),
            scope_key: String::new(),
            start_local: NaiveTime::parse_from_str(start, "%H:%M").unwrap(),
            end_local: NaiveTime::parse_from_str(end, "%H:%M").unwrap(),
        }
    }

    #[test]
    fn inside_a_same_day_window_reschedules_to_window_end_plus_jitter() {
        let p = policy("09:00", "17:00");
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let result = resolve_reschedule(now, "Europe/London", "Europe/London", &p)
            .expect("must reschedule");
        let end = Utc.with_ymd_and_hms(2026, 1, 15, 17, 0, 0).unwrap();
        assert!(result >= end && result <= end + Duration::minutes(30));
    }

    #[test]
    fn outside_the_window_sends_now() {
        let p = policy("09:00", "17:00");
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 20, 0, 0).unwrap();
        assert_eq!(
            resolve_reschedule(now, "Europe/London", "Europe/London", &p),
            None
        );
    }

    #[test]
    fn a_window_that_wraps_midnight_covers_both_sides_and_not_the_middle() {
        let p = policy("22:00", "07:00");
        let late = Utc.with_ymd_and_hms(2026, 1, 15, 23, 0, 0).unwrap();
        assert!(resolve_reschedule(late, "UTC", "UTC", &p).is_some());
        let early = Utc.with_ymd_and_hms(2026, 1, 15, 5, 0, 0).unwrap();
        assert!(resolve_reschedule(early, "UTC", "UTC", &p).is_some());
        let midday = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert_eq!(resolve_reschedule(midday, "UTC", "UTC", &p), None);
    }

    #[test]
    fn unparseable_customer_tz_falls_back_to_tenant_default() {
        let p = policy("09:00", "17:00");
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert!(
            resolve_reschedule(now, "not-a-real-zone", "Europe/London", &p).is_some()
        );
    }

    #[test]
    fn unparseable_customer_and_tenant_default_fails_open() {
        let p = policy("09:00", "17:00");
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert_eq!(resolve_reschedule(now, "nope", "also-nope", &p), None);
    }

    #[test]
    fn a_spring_forward_gap_rounds_forward_to_the_next_valid_instant() {
        // America/New_York, 2024-03-10: clocks jump 02:00 -> 03:00 (EST->EDT);
        // 02:30 local does not exist. Window end 02:30 must round forward to
        // 03:00 local (EDT, UTC-4) = 07:00 UTC.
        let p = policy("00:00", "02:30");
        let tz: Tz = "America/New_York".parse().unwrap();
        let now = tz
            .with_ymd_and_hms(2024, 3, 10, 1, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let result =
            resolve_reschedule(now, "America/New_York", "America/New_York", &p)
                .expect("must reschedule");
        let expected = Utc.with_ymd_and_hms(2024, 3, 10, 7, 0, 0).unwrap();
        assert!(result >= expected && result <= expected + Duration::minutes(30));
    }
}
