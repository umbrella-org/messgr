use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, DurationRound, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use sqlx::PgPool;
use uuid::Uuid;

use crate::ingest::model::class;

use super::model::{UsageRow, enforcement, granularity};
use super::repo;

type Key = (Uuid, String, String); // (producer_id, channel, class)

#[derive(Debug, Clone)]
struct EffectiveQuota {
    per_minute: Option<i32>,
    per_day: Option<i32>, // overridden in place when an active override exists
    enforcement: String,
}

#[derive(Debug, Clone, Default)]
struct Counters {
    minute_window_start: Option<DateTime<Utc>>,
    minute_sent: i64,
    minute_blocked: i64,
    day_window_start: Option<DateTime<Utc>>,
    day_sent: i64,
    day_blocked: i64,
}

#[derive(Debug, PartialEq)]
pub enum QuotaDecision {
    Admit,
    SoftBreach,
    Defer(DateTime<Utc>),
}

pub struct QuotaTracker {
    tz: Tz,
    config: RwLock<HashMap<Key, EffectiveQuota>>,
    counters: RwLock<HashMap<Key, Counters>>,
}

impl QuotaTracker {
    /// `tz_name` is `tenant_config.quota_day_boundary_tz` (or
    /// `DEFAULT_QUOTA_DAY_BOUNDARY_TZ` when no `tenant_config` row exists) —
    /// parsed once at construction; an unparseable name falls back to
    /// `Tz::UTC` with a `tracing::error!`, matching the codebase's other
    /// loaded-once-at-startup config fallbacks.
    pub fn new(tz_name: &str) -> Self {
        let tz = tz_name.parse::<Tz>().unwrap_or_else(|err| {
            tracing::error!(
                tz_name,
                %err,
                "producer_quota: unparseable quota_day_boundary_tz, falling back to UTC"
            );
            chrono_tz::UTC
        });
        Self {
            tz,
            config: RwLock::new(HashMap::new()),
            counters: RwLock::new(HashMap::new()),
        }
    }

    /// Reloads the limit/override cache — safe to call from a standby
    /// (decision 12), read-only.
    pub async fn refresh_config(
        &self,
        pool: &PgPool,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let quotas = repo::list(pool).await?;
        let overrides = repo::load_active_overrides(pool, now).await?;
        let mut merged: HashMap<Key, EffectiveQuota> = quotas
            .into_iter()
            .map(|q| {
                (
                    (q.producer_id, q.channel.clone(), q.class.clone()),
                    EffectiveQuota {
                        per_minute: q.per_minute,
                        per_day: q.per_day,
                        enforcement: q.enforcement,
                    },
                )
            })
            .collect();
        for o in overrides {
            if let Some(q) =
                merged.get_mut(&(o.producer_id, o.channel.clone(), o.class.clone()))
            {
                q.per_day =
                    Some(q.per_day.map_or(o.per_day, |base| base.max(o.per_day)));
            }
            // an override with no base producer_quota row has nothing to raise — ignored,
            // matching "an override only ever raises an existing ceiling."
        }
        *self.config.write().expect("config lock poisoned") = merged;
        Ok(())
    }

    /// Seeds counters from producer_usage's current-window rows — called once
    /// at dispatcher startup, after leadership is acquired (decision 12).
    pub async fn rebuild_from_db(
        &self,
        pool: &PgPool,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let minute_start = Self::minute_window_start(now);
        let day_start = self.day_window_start(now);
        let rows = repo::load_current_usage(pool, minute_start, day_start).await?;
        let mut counters = self.counters.write().expect("counters lock poisoned");
        for row in rows {
            let key = (row.producer_id, row.channel, row.class);
            let entry = counters.entry(key).or_default();
            if row.granularity == granularity::MINUTE {
                entry.minute_window_start = Some(row.window_start);
                entry.minute_sent = row.sent;
                entry.minute_blocked = row.blocked;
            } else {
                entry.day_window_start = Some(row.window_start);
                entry.day_sent = row.sent;
                entry.day_blocked = row.blocked;
            }
        }
        Ok(())
    }

    /// Snapshots every tracked key's current windows into `producer_usage` —
    /// called on a timer, only after leadership (decision 12).
    pub async fn flush(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        let rows: Vec<UsageRow> = {
            let counters = self.counters.read().expect("counters lock poisoned");
            counters
                .iter()
                .flat_map(|((producer_id, channel, class), c)| {
                    let mut out = Vec::new();
                    if let Some(ws) = c.minute_window_start {
                        out.push(UsageRow {
                            producer_id: *producer_id,
                            channel: channel.clone(),
                            class: class.clone(),
                            granularity: granularity::MINUTE.into(),
                            window_start: ws,
                            sent: c.minute_sent,
                            blocked: c.minute_blocked,
                        });
                    }
                    if let Some(ws) = c.day_window_start {
                        out.push(UsageRow {
                            producer_id: *producer_id,
                            channel: channel.clone(),
                            class: class.clone(),
                            granularity: granularity::DAY.into(),
                            window_start: ws,
                            sent: c.day_sent,
                            blocked: c.day_blocked,
                        });
                    }
                    out
                })
                .collect()
        };
        if !rows.is_empty() {
            repo::flush_usage(pool, &rows).await?;
        }
        Ok(())
    }

    /// The gate itself — synchronous, no `.await` (decision 3): `try_process`
    /// calls this inline, never across a lock held over network I/O.
    pub fn check_and_record(
        &self,
        producer_id: Uuid,
        channel: &str,
        class_value: &str,
        now: DateTime<Utc>,
    ) -> QuotaDecision {
        let key: Key = (producer_id, channel.to_string(), class_value.to_string());

        // class::AUTH: unconditional admit, still counted (decision 9) —
        // never looks at `config` at all, so a stray producer_quota row for
        // auth (rejected at configure-time, decision 6) couldn't block it
        // even if one existed.
        if class_value == class::AUTH {
            self.record_admit(&key, now);
            return QuotaDecision::Admit;
        }

        // Otherwise: look up `config` for (producer_id, channel, class); no
        // entry -> unconditional admit, still counted (decision 9).
        let effective = {
            let config = self.config.read().expect("config lock poisoned");
            config.get(&key).cloned()
        };
        let Some(effective) = effective else {
            self.record_admit(&key, now);
            return QuotaDecision::Admit;
        };

        let minute_start = Self::minute_window_start(now);
        let day_start = self.day_window_start(now);

        let mut counters = self.counters.write().expect("counters lock poisoned");
        let entry = counters.entry(key).or_default();
        Self::roll_window(
            &mut entry.minute_window_start,
            &mut entry.minute_sent,
            &mut entry.minute_blocked,
            minute_start,
        );
        Self::roll_window(
            &mut entry.day_window_start,
            &mut entry.day_sent,
            &mut entry.day_blocked,
            day_start,
        );

        // Day breach (`per_day` set and `day_sent >=` it) takes precedence
        // over minute breach (decision 10).
        let day_breach = effective
            .per_day
            .is_some_and(|limit| entry.day_sent >= i64::from(limit));
        let minute_breach = effective
            .per_minute
            .is_some_and(|limit| entry.minute_sent >= i64::from(limit));

        if day_breach || minute_breach {
            if effective.enforcement == enforcement::HARD {
                // A breach increments that window's `blocked` and returns
                // `Defer`.
                if day_breach {
                    entry.day_blocked += 1;
                    return QuotaDecision::Defer(self.next_day_window_start(now));
                }
                entry.minute_blocked += 1;
                return QuotaDecision::Defer(minute_start + TimeDelta::minutes(1));
            }
            // Soft breach: increments `sent` on both windows and returns
            // `SoftBreach`.
            entry.minute_sent += 1;
            entry.day_sent += 1;
            return QuotaDecision::SoftBreach;
        }

        // No breach increments `sent` on both windows and returns `Admit`.
        entry.minute_sent += 1;
        entry.day_sent += 1;
        QuotaDecision::Admit
    }

    /// Rolls one window forward (resetting `sent`/`blocked` to 0) if `fresh`
    /// has passed the window's current boundary, matching or initializing
    /// it otherwise.
    fn roll_window(
        window_start: &mut Option<DateTime<Utc>>,
        sent: &mut i64,
        blocked: &mut i64,
        fresh: DateTime<Utc>,
    ) {
        if *window_start != Some(fresh) {
            *window_start = Some(fresh);
            *sent = 0;
            *blocked = 0;
        }
    }

    /// Records an unconditional admit — used for `class::AUTH` and for any
    /// key with no configured `producer_quota` row at all (decision 9):
    /// both cases skip the block/defer decision entirely but still call
    /// into the tracker's admit path, so `producer_usage` carries real
    /// numbers for producers nobody has configured a limit for yet.
    fn record_admit(&self, key: &Key, now: DateTime<Utc>) {
        let minute_start = Self::minute_window_start(now);
        let day_start = self.day_window_start(now);

        let mut counters = self.counters.write().expect("counters lock poisoned");
        let entry = counters.entry(key.clone()).or_default();
        Self::roll_window(
            &mut entry.minute_window_start,
            &mut entry.minute_sent,
            &mut entry.minute_blocked,
            minute_start,
        );
        Self::roll_window(
            &mut entry.day_window_start,
            &mut entry.day_sent,
            &mut entry.day_blocked,
            day_start,
        );
        entry.minute_sent += 1;
        entry.day_sent += 1;
    }

    fn minute_window_start(now: DateTime<Utc>) -> DateTime<Utc> {
        now.duration_trunc(TimeDelta::minutes(1))
            .expect("minute truncation is infallible")
    }

    /// Local midnight in `self.tz`, mapped back to UTC — decision 5's
    /// ambiguous/nonexistent handling lives here.
    fn day_window_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let local_date = now.with_timezone(&self.tz).date_naive();
        let naive_midnight = local_date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time-of-day");
        self.resolve_local_midnight(naive_midnight)
    }

    /// Local midnight in `self.tz` for the day *after* `now`'s local day —
    /// what a day-breach `Defer` reschedules to.
    fn next_day_window_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let next_local_date =
            now.with_timezone(&self.tz).date_naive() + chrono::Duration::days(1);
        let naive_midnight = next_local_date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time-of-day");
        self.resolve_local_midnight(naive_midnight)
    }

    /// Ambiguous local time (DST fall-back, two midnights) resolves to
    /// `.earliest()`; a nonexistent local time (DST spring-forward skipping
    /// midnight itself — vanishingly rare for a `00:00` transition) falls
    /// back to `Tz::from_utc_datetime` on the naive midnight rather than
    /// failing the gate. This is a pragmatic fallback, not the DST-rigor
    /// bar — T-043 owns getting this fully right.
    fn resolve_local_midnight(
        &self,
        naive_midnight: chrono::NaiveDateTime,
    ) -> DateTime<Utc> {
        match self.tz.from_local_datetime(&naive_midnight).earliest() {
            Some(dt) => dt.with_timezone(&Utc),
            None => self
                .tz
                .from_utc_datetime(&naive_midnight)
                .with_timezone(&Utc),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(tz_name: &str) -> QuotaTracker {
        QuotaTracker::new(tz_name)
    }

    fn set_quota(
        tracker: &QuotaTracker,
        producer_id: Uuid,
        channel: &str,
        class_value: &str,
        per_minute: Option<i32>,
        per_day: Option<i32>,
        enforcement_value: &str,
    ) {
        tracker
            .config
            .write()
            .expect("config lock poisoned")
            .insert(
                (producer_id, channel.to_string(), class_value.to_string()),
                EffectiveQuota {
                    per_minute,
                    per_day,
                    enforcement: enforcement_value.to_string(),
                },
            );
    }

    #[test]
    fn auth_class_is_always_admitted_and_counted_even_with_no_config() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        let now = Utc::now();

        let decision = tracker.check_and_record(producer_id, "sms", class::AUTH, now);
        assert_eq!(decision, QuotaDecision::Admit);

        let counters = tracker.counters.read().expect("lock poisoned");
        let entry = counters
            .get(&(producer_id, "sms".to_string(), class::AUTH.to_string()))
            .expect("counter must exist after admit");
        assert_eq!(entry.minute_sent, 1);
        assert_eq!(entry.day_sent, 1);
    }

    #[test]
    fn a_producer_with_no_configured_quota_is_unlimited_but_counted() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        let now = Utc::now();

        let decision = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert_eq!(decision, QuotaDecision::Admit);

        let counters = tracker.counters.read().expect("lock poisoned");
        let entry = counters
            .get(&(producer_id, "sms".to_string(), "marketing".to_string()))
            .expect("counter must exist after admit");
        assert_eq!(entry.minute_sent, 1);
    }

    #[test]
    fn hard_enforcement_defers_once_the_per_minute_limit_is_reached() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            Some(1),
            None,
            enforcement::HARD,
        );
        let now = Utc::now();

        let first = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert_eq!(first, QuotaDecision::Admit);

        let second = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert!(matches!(second, QuotaDecision::Defer(_)));
    }

    #[test]
    fn soft_enforcement_admits_past_the_per_minute_limit() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "transactional",
            Some(1),
            None,
            enforcement::SOFT,
        );
        let now = Utc::now();

        let first = tracker.check_and_record(producer_id, "sms", "transactional", now);
        assert_eq!(first, QuotaDecision::Admit);

        let second = tracker.check_and_record(producer_id, "sms", "transactional", now);
        assert_eq!(second, QuotaDecision::SoftBreach);
    }

    #[test]
    fn a_day_breach_takes_precedence_over_a_minute_breach_and_defers_to_the_day_boundary()
     {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            Some(100),
            Some(1),
            enforcement::HARD,
        );
        let now = Utc::now();

        let first = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert_eq!(first, QuotaDecision::Admit);

        let second = tracker.check_and_record(producer_id, "sms", "marketing", now);
        let expected_defer = tracker.next_day_window_start(now);
        assert_eq!(second, QuotaDecision::Defer(expected_defer));
    }

    #[test]
    fn counters_reset_once_the_minute_window_boundary_passes() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            Some(1),
            None,
            enforcement::HARD,
        );
        let first_minute = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap();
        let next_minute = Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 5).unwrap();

        let first =
            tracker.check_and_record(producer_id, "sms", "marketing", first_minute);
        assert_eq!(first, QuotaDecision::Admit);
        let second =
            tracker.check_and_record(producer_id, "sms", "marketing", first_minute);
        assert!(matches!(second, QuotaDecision::Defer(_)));

        let third =
            tracker.check_and_record(producer_id, "sms", "marketing", next_minute);
        assert_eq!(third, QuotaDecision::Admit);
    }

    #[test]
    fn counters_reset_once_the_day_window_boundary_passes_local_midnight_in_the_configured_tz()
     {
        let tracker = tracker("America/New_York");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            None,
            Some(1),
            enforcement::HARD,
        );
        // 2026-01-01 12:00 UTC is 2026-01-01 07:00 in America/New_York (EST, UTC-5).
        let day_one = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        // 2026-01-02 12:00 UTC is 2026-01-02 07:00 EST -- a fresh New York calendar day.
        let day_two = Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap();

        let first = tracker.check_and_record(producer_id, "sms", "marketing", day_one);
        assert_eq!(first, QuotaDecision::Admit);
        let second = tracker.check_and_record(producer_id, "sms", "marketing", day_one);
        assert!(matches!(second, QuotaDecision::Defer(_)));

        let third = tracker.check_and_record(producer_id, "sms", "marketing", day_two);
        assert_eq!(third, QuotaDecision::Admit);
    }

    #[test]
    fn an_active_override_raises_the_effective_per_day_limit() {
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            None,
            Some(1),
            enforcement::HARD,
        );
        {
            let mut config = tracker.config.write().expect("config lock poisoned");
            let entry = config
                .get_mut(&(producer_id, "sms".to_string(), "marketing".to_string()))
                .expect("base quota must exist");
            entry.per_day = Some(5); // simulates refresh_config's override merge
        }
        let now = Utc::now();

        for _ in 0..5 {
            let decision =
                tracker.check_and_record(producer_id, "sms", "marketing", now);
            assert_eq!(decision, QuotaDecision::Admit);
        }
        let sixth = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert!(matches!(sixth, QuotaDecision::Defer(_)));
    }

    #[test]
    fn an_expired_override_no_longer_applies() {
        // An expired override is simply never merged into `config` by
        // `refresh_config` (it is not in `load_active_overrides`'s result) --
        // exercised here by never raising the base limit at all.
        let tracker = tracker("UTC");
        let producer_id = Uuid::new_v4();
        set_quota(
            &tracker,
            producer_id,
            "sms",
            "marketing",
            None,
            Some(1),
            enforcement::HARD,
        );
        let now = Utc::now();

        let first = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert_eq!(first, QuotaDecision::Admit);
        let second = tracker.check_and_record(producer_id, "sms", "marketing", now);
        assert!(matches!(second, QuotaDecision::Defer(_)));
    }

    #[test]
    fn day_window_start_lands_on_the_correct_calendar_day_across_a_dst_transition() {
        // 2026-03-08 is America/New_York's spring-forward day (clocks jump
        // 02:00 -> 03:00). 06:00 UTC on 2026-03-08 is 01:00 EST -- still the
        // 8th locally -- so local midnight for that instant must be
        // 2026-03-08 00:00 EST (05:00 UTC), not the 7th or the 9th.
        let tracker = tracker("America/New_York");
        let now = Utc.with_ymd_and_hms(2026, 3, 8, 6, 0, 0).unwrap();

        let window_start = tracker.day_window_start(now);
        let local = window_start.with_timezone(&tracker.tz);
        assert_eq!(
            local.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 3, 8).unwrap()
        );
        assert_eq!(
            local.time(),
            chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        );
    }
}
