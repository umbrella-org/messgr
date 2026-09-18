---
id: T-042
title: Producer quotas and producer_usage enforcement
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-042 — Producer quotas and producer_usage enforcement

## Outcome

After this ships, a producer's marketing traffic that exceeds its configured per-minute or
per-day limit defers to the next window instead of spending unlimited budget; a transactional
producer over its limit still sends, loudly alerted; auth traffic is counted but never blocked.

## Description

Closes build-order step 6 (§5.1) — currently entirely unbuilt: no `producer_quota` or
`producer_usage` table exists anywhere in `migrations/`, no in-process counter, no enforcement
path. Only `tenant_config.quota_day_boundary_tz` exists (a config field with nothing reading it).

Per §5.1, this needs, precisely (do not build more than this without asking — §5.1 is unusually
prescriptive about what *not* to build):

1. **Two distinct limits, not one** — admission rate (ingest API, protects messgr's own DB,
   `429` immediate/retryable) vs. send quota (dispatcher, protects customers/provider spend,
   defer-or-alert per mode). Conflating them is explicitly called out as the usual mistake.
2. **Charged at dispatch, not ingest** — a message scheduled three weeks out consumes the quota
   of the day it sends, not the day it was submitted.
3. **In-process counter, not a hot DB row** — a plain in-memory map behind the single dispatcher
   enforcer per tenant (no coordination/contention, relies on T-039's single-active-dispatcher
   guarantee), flushed to `producer_usage` every few seconds for reporting, rebuilt from that
   table on dispatcher startup so a restart doesn't zero a producer's daily allowance.
4. **Fixed windows** (per-minute burst + per-day total resetting at `quota_day_boundary_tz`
   local midnight), not rolling — explicitly rejected as "fairer and harder to explain; not
   worth it here."
5. **Enforcement mode per (producer, channel, class)**, with these exact defaults (AGENTS.md
   invariant 5 is the `transactional`/`auth` rows — do not make quota able to block either):
   `marketing` → hard/defer; `transactional` → soft/send-and-alert; `auth` → exempt but counted.
6. **`producer_quota_override`** for time-boxed, approved, self-expiring campaign-day uplift.

**Explicitly cut, already recorded in DESIGN.md — do not reintroduce:** a `campaign_stats`
rollup and a marketing share-of-budget throttle (AGENTS.md "Prefer cutting to adding" names both
as removed for being unjustified).

Soft coupling: depends conceptually on T-039 (single active dispatcher) for the in-process
counter to be safe without coordination — not a hard `depends-on`, since quota logic can be built
and tested before T-039 lands, but the "no coordination needed" property only holds once T-039 is
real. Flag this explicitly if picked up before T-039 is done.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-042-producer-quotas-and-producer-usage-enforcement
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1. The soft
coupling to T-039 (single active dispatcher) named in the Description is satisfied — T-039 is in
`6-done/` and merged (PR #57 pattern; verify its History carries a `merged to main` line before
picking this up).

### Confirmed design decisions (do not deviate without asking)

1. **Scope is the send-quota half only.** §5.1's admission-rate limit (ingest API, `429`,
   protects messgr's own DB) is build-order step 18, unbuilt, and out of scope here — this ticket
   never touches `src/ingest/`. The Description's "two distinct limits" point is about not
   conflating them, not about building both.
2. **Charged at dispatch.** The quota check runs inside `try_process` against the claimed
   `outbox` row's own `producer_id`/`channel`/`class` — never at ingest, matching AGENTS.md hard
   invariant 3.
3. **In-process counters, one `QuotaTracker` shared across a tenant's channel loops**, exactly
   like `KillSwitchCache` — safe without coordination only because T-039 guarantees one active
   dispatcher process per tenant (done, merged; see Prerequisite gate).
4. **Fixed windows.** Per-minute is a plain UTC-truncated minute (tz-independent). Per-day resets
   at local midnight in `tenant_config.quota_day_boundary_tz`, which needs a real timezone
   database — see decision 5.
5. **`chrono-tz` is added as a new dependency** (confirmed with the user during refinement) —
   `chrono` alone cannot resolve an IANA name like `"Europe/London"` or its DST transitions, and
   §5.1 explicitly wants a local-midnight reset. Ambiguous local time (DST fall-back, two
   midnights) resolves to `.earliest()`; a nonexistent local time (DST spring-forward skipping
   midnight itself — vanishingly rare for a `00:00` transition) falls back to
   `Tz::from_utc_datetime` on the naive midnight rather than failing the gate. This is a pragmatic
   fallback, not the DST-rigor bar — that belongs to T-043 ("Quiet hours with jitter and DST
   handling"), which owns getting this fully right; T-042 only needs day windows to reset on the
   correct calendar day in the overwhelming common case.
6. **Enforcement legality is mechanically enforced at configure-time, not left to operator
   discipline** (AGENTS.md hard invariant 5: quota must never be able to block transactional or
   auth traffic). `producer_quota::configure::set_producer_quota` rejects (audited, like the
   T-005/F1 unknown-tenant-slug pattern):
   - any row for `class = "auth"` at all — exempt-but-counted is unconditional in the dispatcher
     code (decision 9), so a quota row for auth is meaningless, not just non-blocking.
   - `enforcement = "hard"` for `class = "transactional"`.
   `class = "marketing"` may be `hard` or `soft` (hard is the documented default outcome, but the
   CLI does not force it — an operator can knowingly run marketing in `soft` too). The
   `messgr-control producer-quota set --class` flag's `PossibleValuesParser` only offers
   `marketing`/`transactional` (never `auth`), and `configure.rs` re-checks anyway rather than
   trusting the CLI's choice list to be the only caller.
7. **`producer_quota_override` raises only `per_day`** — the table has no `per_minute` column
   (matches `03-data-model.md`'s schema verbatim) and never touches `enforcement`, which is always
   inherited from the base `producer_quota` row. Overrides are legal for `marketing` and
   `transactional`, rejected for `auth` (same reason as decision 6).
8. **Gate placement in `try_process`: immediately after the consent gate, before
   `repo::load_ciphertexts`.** After every terminal/consent gate, so a message that was already
   going to be blocked for an unrelated reason (expired, suppressed, unverified-enforce,
   unconsented) never spends quota budget it was never really going to use; before any DEK/decrypt
   work, so a quota-deferred message spends no Vault work — the same "cheapest first" reasoning
   T-036/T-038 already applied to their own gates, applied here to the resource quota actually
   protects (spend/budget, not CPU).
9. **`auth` is exempt but unconditionally counted; a row with no `producer_quota` configured at
   all is unlimited but still counted.** Both cases skip the block/defer decision entirely but
   still call into the tracker's admit path, so `producer_usage` carries real numbers for the
   quota dashboard (a later ticket) even for producers nobody has configured a limit for yet, and
   an OTP volume spike is still visible (§5.1's stated reason to count it).
10. **A hard breach reschedules via the existing `dispatcher::repo::reschedule_retry`, never
    terminal-writes.** `next_attempt_at` is set to the start of whichever window actually
    breached — day takes precedence over minute (if the day total is already exhausted, deferring
    to the next minute would just re-breach every tick until the day rolls over anyway).
    `outbox.attempts` still increments on every claim regardless (T-013 decision 10, unchanged);
    `MAX_SEND_ATTEMPTS` is never consulted here, matching every other gate's defer/terminal path
    (only the sender-failure branch reads it).
11. **Soft-breach alerting is a `tracing::warn!` log line, not a `comms_event` row.** Unlike
    verification's `observe` mode (a rare, address-scoped event), a soft-mode producer routinely
    over quota would write a duplicate non-terminal `comms_event` row on every retry tick if this
    followed that precedent. `producer_usage.sent`/`.blocked` plus the log line (producer_id,
    channel, class, current count, configured limit — no extra DB round-trip for
    `producer.contact`; an operator cross-references that via `producer list`) are the intended,
    cheaper visibility path. §5.1's "loudly" is served by the log; wiring an actual paging
    integration is out of scope (none exists in this codebase today).
12. **Config cache vs. counters are gated differently at startup.** `QuotaTracker`'s limit/override
    cache is read-only and refreshed on a poll loop (30s, matching `kill_switch`'s own poll
    fallback interval) started alongside the kill-switch refresh loop, before `leader::acquire` —
    safe on a standby, like `KillSwitchCache`. The usage counters are rebuilt from `producer_usage`
    and the periodic flush loop (every 5s) is started only *after* `_leadership` is acquired,
    immediately alongside `repo::clear_stale_leases` — a standby's tracker never has real counts,
    and a racing flush from one would corrupt the real leader's `producer_usage` rows.
13. **`producer_usage` retention/sweep is out of scope for this ticket** (confirmed with the
    user) — the migration's own comment already documents the need (minute rows after 7 days, day
    rows after a year); a follow-up ticket adds the sweep, matching `orphan_reconcile`'s own
    precedent of being its own ticket.
14. **Configuration surface is CLI-only** (`messgr-control producer-quota ...`), mirroring
    consent/suppression/tenant-config. No HTTP endpoint — the query API/admin panel that would
    expose this are steps 13/14, unbuilt.
15. **New `DispatcherContext` fields (`quota_day_boundary_tz: String`, `quota:
    Arc<QuotaTracker>`) touch every existing construction site.** 21 as of this writing (1 in
    `src/bin/dispatcher.rs`, 19 in `tests/dispatcher.rs`, 1 in `tests/kill_switch.rs` —
    re-`grep -rn "DispatcherContext {" src tests` before starting, since the count drifts as new
    tests land; T-036's own History records exactly this happening). Unavoidable, matching
    T-036's `verification_mode` addition.

### Tasks

#### Task 1 — dependency

`Cargo.toml`: add `chrono-tz = "0.9"` (or whatever the current `0.4`-compatible `chrono` line
resolves to) under the existing `chrono` dependency. No feature flags needed beyond the default
(bundled tzdata).

#### Task 2 — schema migration

`migrations/tenant/0016_producer_quota.sql`, verbatim from `development/design/03-data-model.md`
§4.9's own snippet (already exact — no design-doc bug found this time):

```sql
-- Producer send quotas and usage (DESIGN.md §5.1, T-042): two fixed windows
-- (per-minute burst, per-day total) per (producer, channel, class), enforced
-- at dispatch, never at ingest (AGENTS.md invariant 3). enforcement is
-- hard|soft; auth never gets a row here at all (exempt but counted
-- unconditionally in the dispatcher, never blocked -- AGENTS.md invariant 5).
CREATE TABLE producer_quota (
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_minute  int,                     -- burst ceiling; NULL = unlimited
    per_day     int,                     -- daily total; NULL = unlimited
    enforcement text NOT NULL,           -- hard | soft
    PRIMARY KEY (producer_id, channel, class)
);

-- Time-boxed uplift (e.g. a campaign day). Raises per_day only -- enforcement
-- is always inherited from the base producer_quota row, never overridden.
CREATE TABLE producer_quota_override (
    id          uuid PRIMARY KEY,
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_day     int  NOT NULL,
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz NOT NULL,    -- mandatory; overrides always expire
    approved_by text NOT NULL,
    reason      text NOT NULL
);

-- Flushed from the dispatcher's in-process QuotaTracker every few seconds
-- for reporting, and read back on dispatcher startup to rebuild the
-- in-process counters so a restart mid-window doesn't reset a producer's
-- allowance to zero. Retention sweep (minute rows after 7 days, day rows
-- after a year) is explicitly out of scope for T-042 -- a follow-up ticket.
CREATE TABLE producer_usage (
    producer_id  uuid NOT NULL,
    channel      text NOT NULL,
    class        text NOT NULL,
    granularity  text NOT NULL,          -- minute | day
    window_start timestamptz NOT NULL,
    sent         bigint NOT NULL DEFAULT 0,
    blocked      bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (producer_id, channel, class, granularity, window_start)
);
CREATE INDEX ON producer_usage (granularity, window_start);
```

#### Task 3 — `producer_quota` module: model + repo

New files, mirroring `src/suppression/`.

`src/producer_quota/model.rs`:

```rust
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod enforcement {
    pub const HARD: &str = "hard";
    pub const SOFT: &str = "soft";
}

pub mod granularity {
    pub const MINUTE: &str = "minute";
    pub const DAY: &str = "day";
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProducerQuota {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_minute: Option<i32>,
    pub per_day: Option<i32>,
    pub enforcement: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProducerQuotaInput {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_minute: Option<i32>,
    pub per_day: Option<i32>,
    pub enforcement: String,
}

impl ProducerQuotaInput {
    pub fn matches(&self, existing: &ProducerQuota) -> bool {
        self.producer_id == existing.producer_id
            && self.channel == existing.channel
            && self.class == existing.class
            && self.per_minute == existing.per_minute
            && self.per_day == existing.per_day
            && self.enforcement == existing.enforcement
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProducerQuotaOverride {
    pub id: Uuid,
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_day: i32,
    pub valid_from: DateTime<Utc>,
    pub valid_to: DateTime<Utc>,
    pub approved_by: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ProducerQuotaOverrideInput {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub per_day: i32,
    pub valid_from: DateTime<Utc>,
    pub valid_to: DateTime<Utc>,
    pub approved_by: String,
    pub reason: String,
}

/// One `producer_usage` row, either a live in-process snapshot being
/// flushed or a row read back at startup to rebuild one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRow {
    pub producer_id: Uuid,
    pub channel: String,
    pub class: String,
    pub granularity: String,
    pub window_start: DateTime<Utc>,
    pub sent: i64,
    pub blocked: i64,
}
```

`src/producer_quota/repo.rs` — CRUD mirroring `suppression::repo`'s shape:

- `load_one(pool, producer_id, channel, class) -> Option<ProducerQuota>`
- `list(pool) -> Vec<ProducerQuota>` (whole tenant — table stays small, like `provider_config`)
- `upsert(pool, input: &ProducerQuotaInput) -> Result<(), sqlx::Error>` (`ON CONFLICT (producer_id,
  channel, class) DO UPDATE SET per_minute = EXCLUDED.per_minute, per_day = EXCLUDED.per_day,
  enforcement = EXCLUDED.enforcement`)
- `insert_override(pool, id: Uuid, input: &ProducerQuotaOverrideInput) -> Result<(), sqlx::Error>`
  (plain `INSERT`, no upsert — every call makes a new row, keyed on the generated `id`)
- `list_overrides(pool) -> Vec<ProducerQuotaOverride>` (whole tenant, newest `valid_from` first)
- `load_active_overrides(pool, now: DateTime<Utc>) -> Vec<ProducerQuotaOverride>` (`WHERE
  valid_from <= $1 AND valid_to > $1`) — used by the tracker's config refresh
- `load_current_usage(pool, minute_window_start, day_window_start) -> Vec<UsageRow>` (`WHERE
  (granularity = 'minute' AND window_start = $1) OR (granularity = 'day' AND window_start = $2)`)
  — used by `QuotaTracker::rebuild_from_db` at dispatcher startup
- `flush_usage(pool, rows: &[UsageRow]) -> Result<(), sqlx::Error>` — one `INSERT ... ON CONFLICT
  (producer_id, channel, class, granularity, window_start) DO UPDATE SET sent = EXCLUDED.sent,
  blocked = EXCLUDED.blocked` per row, in one transaction; each row's `sent`/`blocked` is the
  in-process counter's own current total for that window, an overwrite snapshot, not an increment
  (the in-memory value is already authoritative)

#### Task 4 — `producer_quota::configure` (actor-facing operations)

`src/producer_quota/configure.rs`, mirroring `provider_config::configure` (`ConfigureError`/
`rejected`/`audit` shape) plus a producer-name resolution step (`producer::repo::find_by_name`,
audited-reject on unknown, same pattern as the unknown-tenant-slug case):

```rust
pub async fn set_producer_quota(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    producer_name: &str,
    channel: &str,
    class: &str,
    per_minute: Option<i32>,
    per_day: Option<i32>,
    enforcement: &str,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    /* resolve tenant (audit-then-reject on unknown, T-005/F1); connect tenant pool;
       resolve producer by name (audit-then-reject on unknown, same pattern); reject
       (audit-then-reject) class == class::AUTH, or (class == class::TRANSACTIONAL &&
       enforcement == enforcement::HARD) — decision 6; repo::load_one to decide
       created/updated/idempotent; repo::upsert when not idempotent; audit; close pool */
}

pub async fn list_producer_quota(
    control_pool: &PgPool, base_db_url: &str, tenant_slug: &str,
) -> Result<Vec<ProducerQuota>, ConfigureError> { /* resolve tenant, repo::list */ }

pub async fn add_producer_quota_override(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    producer_name: &str,
    channel: &str,
    class: &str,
    per_day: i32,
    valid_from: DateTime<Utc>,
    valid_to: DateTime<Utc>,
    approved_by: &str,
    reason: &str,
    actor: &str,
) -> Result<Uuid, ConfigureError> {
    /* resolve tenant + producer (audited-reject on unknown); reject (audited)
       class == class::AUTH, or valid_to <= valid_from, or per_day <= 0;
       repo::insert_override with a fresh Uuid::new_v4(); audit; close pool; return the id */
}

pub async fn list_producer_quota_overrides(
    control_pool: &PgPool, base_db_url: &str, tenant_slug: &str,
) -> Result<Vec<ProducerQuotaOverride>, ConfigureError> { /* resolve tenant, repo::list_overrides */ }
```

`audit()` records `producer_quota.set` / `producer_quota_override.add` on `platform_audit`, same
shape as `provider_config`'s own.

#### Task 5 — `QuotaTracker` (in-process counter + config cache)

`src/producer_quota/tracker.rs`:

```rust
use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use chrono_tz::Tz;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{ProducerQuota, UsageRow, enforcement, granularity};
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
    /// parsed once at construction; an unparseable name falls back to `Tz::UTC`
    /// with a `tracing::error!`, matching the codebase's other
    /// loaded-once-at-startup config fallbacks.
    pub fn new(tz_name: &str) -> Self { /* ... */ }

    /// Reloads the limit/override cache — safe to call from a standby
    /// (decision 12), read-only.
    pub async fn refresh_config(&self, pool: &PgPool, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
        let quotas = repo::list(pool).await?;
        let overrides = repo::load_active_overrides(pool, now).await?;
        let mut merged: HashMap<Key, EffectiveQuota> = quotas
            .into_iter()
            .map(|q| ((q.producer_id, q.channel.clone(), q.class.clone()), EffectiveQuota {
                per_minute: q.per_minute, per_day: q.per_day, enforcement: q.enforcement,
            }))
            .collect();
        for o in overrides {
            if let Some(q) = merged.get_mut(&(o.producer_id, o.channel.clone(), o.class.clone())) {
                q.per_day = Some(q.per_day.map_or(o.per_day, |base| base.max(o.per_day)));
            }
            // an override with no base producer_quota row has nothing to raise — ignored,
            // matching "an override only ever raises an existing ceiling."
        }
        *self.config.write().expect("config lock poisoned") = merged;
        Ok(())
    }

    /// Seeds counters from producer_usage's current-window rows — called once
    /// at dispatcher startup, after leadership is acquired (decision 12).
    pub async fn rebuild_from_db(&self, pool: &PgPool, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
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
            counters.iter().flat_map(|((producer_id, channel, class), c)| {
                let mut out = Vec::new();
                if let Some(ws) = c.minute_window_start {
                    out.push(UsageRow { producer_id: *producer_id, channel: channel.clone(),
                        class: class.clone(), granularity: granularity::MINUTE.into(),
                        window_start: ws, sent: c.minute_sent, blocked: c.minute_blocked });
                }
                if let Some(ws) = c.day_window_start {
                    out.push(UsageRow { producer_id: *producer_id, channel: channel.clone(),
                        class: class.clone(), granularity: granularity::DAY.into(),
                        window_start: ws, sent: c.day_sent, blocked: c.day_blocked });
                }
                out
            }).collect()
        };
        if !rows.is_empty() { repo::flush_usage(pool, &rows).await?; }
        Ok(())
    }

    /// The gate itself — synchronous, no `.await` (decision 3): `try_process`
    /// calls this inline, never across a lock held over network I/O.
    pub fn check_and_record(&self, producer_id: Uuid, channel: &str, class: &str, now: DateTime<Utc>) -> QuotaDecision {
        // class::AUTH: unconditional admit, still counted (decision 9) — never
        // looks at `config` at all, so a stray producer_quota row for auth
        // (rejected at configure-time, decision 6) couldn't block it even if
        // one existed.
        // Otherwise: look up `config` for (producer_id, channel, class); no
        // entry -> unconditional admit, still counted (decision 9).
        // Otherwise: get-or-init this key's Counters, rolling each window
        // (reset sent/blocked to 0, window_start to the fresh boundary) if
        // `now` has passed it. Day breach (`per_day` set and `day_sent >=`
        // it) takes precedence over minute breach (decision 10). A breach
        // increments that window's `blocked` and returns `Defer` (enforcement
        // hard) or increments `sent` on both windows and returns `SoftBreach`
        // (enforcement soft). No breach increments `sent` on both windows and
        // returns `Admit`.
    }

    fn minute_window_start(now: DateTime<Utc>) -> DateTime<Utc> {
        now.duration_trunc(TimeDelta::minutes(1)).expect("minute truncation is infallible")
    }

    /// Local midnight in `self.tz`, mapped back to UTC — decision 5's
    /// ambiguous/nonexistent handling lives here.
    fn day_window_start(&self, now: DateTime<Utc>) -> DateTime<Utc> { /* ... */ }

    /// Local midnight in `self.tz` for the day *after* `now`'s local day —
    /// what a day-breach `Defer` reschedules to.
    fn next_day_window_start(&self, now: DateTime<Utc>) -> DateTime<Utc> { /* ... */ }
}
```

`#[cfg(test)] mod tests` in the same file (pure, no DB — `now`/`tz` are always caller-supplied,
never `Utc::now()` internally, exactly so these can be deterministic):

- `auth_class_is_always_admitted_and_counted_even_with_no_config` (decision 9 — this is the
  *only* place this behaviour can be tested at all, since `class = "auth"` never reaches the
  outbox, T-011 decision 3, so no dispatcher-level test can exercise it)
- `a_producer_with_no_configured_quota_is_unlimited_but_counted`
- `hard_enforcement_defers_once_the_per_minute_limit_is_reached`
- `soft_enforcement_admits_past_the_per_minute_limit`
- `a_day_breach_takes_precedence_over_a_minute_breach_and_defers_to_the_day_boundary`
- `counters_reset_once_the_minute_window_boundary_passes`
- `counters_reset_once_the_day_window_boundary_passes_local_midnight_in_the_configured_tz`
- `an_active_override_raises_the_effective_per_day_limit`
- `an_expired_override_no_longer_applies`
- `day_window_start_lands_on_the_correct_calendar_day_across_a_dst_transition` (pick a real
  spring-forward or fall-back instant in a DST-observing zone, e.g. `America/New_York`, and assert
  the computed local midnight is the calendar day a human would expect)

#### Task 6 — register the module

`src/lib.rs`: add `pub mod producer_quota;` alongside `pub mod suppression;`.

#### Task 7 — dispatcher gate

`src/dispatcher/worker.rs`:

- Add to `DispatcherContext`:
  ```rust
  /// `tenant_config.quota_day_boundary_tz` (DESIGN.md §5.1, T-042) — loaded
  /// once at startup, like `verification_mode`, never hot-reloaded.
  pub quota_day_boundary_tz: String,
  pub quota: Arc<crate::producer_quota::tracker::QuotaTracker>,
  ```
- In `try_process`, immediately after the consent-gate block and before
  `repo::load_ciphertexts` (decision 8):
  ```rust
  // Producer quota gate (DESIGN.md §5.1, T-042) — after every terminal/consent
  // gate above (a message already going to be blocked never spends quota
  // budget) and before decrypt (a deferred message spends no Vault/DEK work).
  match ctx.quota.check_and_record(row.producer_id, &row.channel, &row.class, Utc::now()) {
      crate::producer_quota::tracker::QuotaDecision::Admit => {}
      crate::producer_quota::tracker::QuotaDecision::SoftBreach => {
          tracing::warn!(
              producer_id = %row.producer_id,
              channel = %row.channel,
              class = %row.class,
              "messgr-dispatcher: producer over its soft quota, sending anyway"
          );
      }
      crate::producer_quota::tracker::QuotaDecision::Defer(next_attempt_at) => {
          repo::reschedule_retry(&ctx.pool, row.comms_request_id, next_attempt_at).await?;
          return Ok(());
      }
  }
  ```

#### Task 8 — dispatcher startup wiring

`src/bin/dispatcher.rs`:

- Add `const DEFAULT_QUOTA_DAY_BOUNDARY_TZ: &str = "UTC";` alongside
  `DEFAULT_VERIFICATION_MODE`, and `const USAGE_FLUSH_INTERVAL: Duration =
  Duration::from_secs(5);` / `const QUOTA_CONFIG_POLL_INTERVAL: Duration =
  Duration::from_secs(30);` alongside `KILL_SWITCH_POLL_INTERVAL`.
- Extract `quota_day_boundary_tz` from the already-loaded `tenant_config`, same pattern as
  `verification_mode`:
  ```rust
  let quota_day_boundary_tz = tenant_config
      .as_ref()
      .map(|c| c.quota_day_boundary_tz.clone())
      .unwrap_or_else(|| DEFAULT_QUOTA_DAY_BOUNDARY_TZ.to_string());
  let quota = Arc::new(QuotaTracker::new(&quota_day_boundary_tz));
  ```
- Pass `quota_day_boundary_tz: quota_day_boundary_tz.clone()` and `quota: quota.clone()` into each
  `DispatcherContext { .. }` literal in the per-channel loop (alongside `kill_switches`/`draining`).
- Spawn the config-refresh poll loop alongside the kill-switch refresh loop (before
  `leader::acquire`, decision 12) — a plain `tokio::spawn` loop calling `quota.refresh_config(&tenant_pool,
  Utc::now())` every `QUOTA_CONFIG_POLL_INTERVAL`, logging (`tracing::error!`) and continuing on
  error (no `LISTEN` channel needed — config changes are rare and 30s staleness is acceptable,
  unlike kill switches).
- After `_leadership` is acquired, immediately alongside `repo::clear_stale_leases`:
  ```rust
  quota.rebuild_from_db(&tenant_pool, Utc::now())
      .await
      .expect("rebuilding producer quota counters from producer_usage failed");
  ```
  then spawn the flush loop (`tokio::spawn`, calls `quota.flush(&tenant_pool)` every
  `USAGE_FLUSH_INTERVAL`, logs and continues on error) — both before the `for (channel, ctx) in
  contexts { ... }` claim-loop spawn.

#### Task 9 — every existing `DispatcherContext { .. }` literal

Add `quota_day_boundary_tz: "UTC".to_string()` and `quota:
Arc::new(QuotaTracker::new("UTC"))` (or a scenario-specific tz/tracker where a test actually
exercises quota behaviour — Task 5's own tests cover that in isolation, so dispatcher-level tests
mostly just need a tracker that never blocks) to every existing `DispatcherContext { .. }`
literal — re-`grep -rn "DispatcherContext {" src tests` first (21 as of this writing: decision 15).

#### Task 10 — CLI wiring (`src/bin/control.rs`)

Add `Command::ProducerQuota { command: ProducerQuotaCommand }`, mirroring `Suppression`/`Consent`:

```rust
#[derive(Subcommand)]
enum ProducerQuotaCommand {
    /// Set (create or update) one (producer, channel, class) quota row.
    Set {
        #[arg(long = "tenant-slug")] tenant_slug: String,
        #[arg(long = "producer-name")] producer_name: String,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([channel::SMS, channel::EMAIL, channel::WHATSAPP]))]
        channel: String,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([class::MARKETING, class::TRANSACTIONAL]))]
        class: String,
        #[arg(long = "per-minute")] per_minute: Option<i32>,
        #[arg(long = "per-day")] per_day: Option<i32>,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([enforcement::HARD, enforcement::SOFT]))]
        enforcement: String,
        #[arg(long)] actor: String,
    },
    /// List every producer_quota row for a tenant.
    List { #[arg(long = "tenant-slug")] tenant_slug: String },
    /// Add a time-boxed per_day uplift.
    OverrideAdd {
        #[arg(long = "tenant-slug")] tenant_slug: String,
        #[arg(long = "producer-name")] producer_name: String,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([channel::SMS, channel::EMAIL, channel::WHATSAPP]))]
        channel: String,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([class::MARKETING, class::TRANSACTIONAL]))]
        class: String,
        #[arg(long = "per-day")] per_day: i32,
        #[arg(long = "valid-from")] valid_from: String,   // RFC 3339
        #[arg(long = "valid-to")] valid_to: String,       // RFC 3339
        #[arg(long = "approved-by")] approved_by: String,
        #[arg(long)] reason: String,
        #[arg(long)] actor: String,
    },
    /// List every producer_quota_override row for a tenant.
    OverrideList { #[arg(long = "tenant-slug")] tenant_slug: String },
}
```

`run()` arm, following `Suppression`'s exact shape (no Vault connection needed — unlike
suppression/consent, nothing here computes an HMAC): resolve/print outcomes the same way,
parsing `--valid-from`/`--valid-to` with `DateTime::parse_from_rfc3339` (mirrors `Suppression`'s
own `--review-at` parsing) and rejecting the CLI call before ever calling into `configure.rs` if
either fails to parse.

### Acceptance test

**`tests/producer_quota.rs`** (new file, mirrors `tests/suppression.rs`):

- `setting_and_listing_round_trips_every_typed_field`
- `setting_again_with_different_limits_updates_the_row_without_duplicating_it`
- `resetting_with_identical_inputs_is_idempotent`
- `setting_hard_enforcement_for_transactional_class_is_rejected` (decision 6)
- `setting_any_quota_row_for_auth_class_is_rejected` (decision 6)
- `setting_quota_against_an_unknown_producer_name_is_rejected_and_audited`
- `setting_quota_against_an_unknown_tenant_slug_is_rejected_and_audited` (T-005/F1 pattern)
- `adding_an_override_with_valid_to_before_valid_from_is_rejected`
- `adding_an_override_for_auth_class_is_rejected`
- `listing_overrides_returns_every_row_for_the_tenant`
- `flushing_and_rebuilding_preserves_in_progress_window_counts` — drive `QuotaTracker` directly
  against a real tenant pool: record several admits/blocks, `flush`, construct a *fresh* tracker,
  `rebuild_from_db` with the same `now`, assert its counters match (the direct proof of decision
  12's restart-safety claim)

**`src/producer_quota/tracker.rs`'s own `#[cfg(test)] mod tests`** — the list under Task 5.

**`tests/dispatcher.rs`** additions — add a `producer_id: Uuid` parameter to
`write_outbox_row_with_verification` (currently a hardcoded `Uuid::new_v4()` inline; every
existing call site passes a fresh `Uuid::new_v4()` to stay unaffected, mirroring how T-038 added
`destination_hmac` as a parameter to the same helper) plus a `register_test_producer_row(tenant,
name) -> Uuid` helper calling `producer::register::register_producer` directly (no cert issuance
needed — unlike `tests/kill_switch.rs`'s heavier `register_test_producer`, nothing here does mTLS):

1. `a_marketing_producer_over_its_per_minute_limit_is_deferred_not_terminal_failed` — register a
   producer, `set_producer_quota(..., per_minute: Some(1), enforcement: hard)`, send two
   marketing rows for that producer, `try_process` both after a shared `QuotaTracker` (constructed
   directly, `refresh_config`'d against the tenant pool) is wired into the test's own
   `DispatcherContext`. Assert: first row sends; second row's outbox row is still present with
   `next_attempt_at` advanced, no terminal write, mock server `.expect(1)`.
2. `a_transactional_producer_over_its_per_minute_limit_still_sends_and_is_counted` — same setup,
   `enforcement: soft`. Assert both rows send (`mock.expect(2)`), no terminal-fail either way —
   the direct proof of AGENTS.md invariant 5.
3. `a_producer_with_no_configured_quota_row_is_never_blocked` — no `producer_quota` row at all;
   assert an ordinary send still succeeds.

Run: `just build && just test && just lint`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/control-plane-cli.adoc`: new `== Producer quotas` section, placed after
  `== Consent` and before `== Customer DEK pre-provisioning` (keeps the three gate-config
  sections — Suppression, Consent, Producer quotas — adjacent), documenting `set`/`list`/
  `override-add`/`override-list`, the closed `channel`/`class`/`enforcement` vocabularies, that
  `auth` is never a legal `--class` value here, that `per-minute`/`per-day` omitted means
  unlimited, and that an override raises `per_day` only.
- `docs/user-manual/dispatcher.adoc`: new paragraph after the `T-037` consent paragraph (line 70)
  and before the `T-041` cancellation paragraph, in the same incremental narrative style,
  documenting: gate order (right after consent, before decrypt), `hard`/`soft`/exempt-but-counted
  per class, the in-process counter + 5s flush + startup rebuild, and the local-midnight day
  boundary. Update the `T-041` paragraph's own gate list ("the Expiry/Suppression/Verification/
  Consent gates above") to include Producer quota, since a fifth gate now precedes decrypt.
- `docs/user-manual/ingest.adoc` line 10: reword "Quotas (§5.1) are still unbuilt" to distinguish
  the two halves now that send quotas are enforced — send quotas are enforced at dispatch (cross-
  reference `messgr-dispatcher`, T-042); admission-rate limiting on this API is still unbuilt
  (build-order step 18).
- `docs/user-manual/introduction.adoc` line 16: remove "quotas" from "There is no full gate chain
  (quotas, kill switches)". While touching this line, also fix "kill switches" — already false
  (shipped in T-016, well before this branch) and flagged but left unfixed by T-038's own review
  (finding F5, "leave for whoever next touches that page's Status section") — reword the whole
  clause to state what is actually enforced (verification/consent/suppression/quota, all at
  dispatch time; kill switches at both ingest and dispatch) and that only the query API/UI remain.
  Note this in the Description/History as incidentally resolving T-038's F5.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint`, `just docs-check` clean.
2. Docs updated per above.
3. Write a summary (files touched, decisions made, anything deferred — the sweep job, per decision
   13) and hand back.
4. Suggested commit message:

   ```
   feat(dispatcher): enforce producer send quotas at dispatch (T-042)

   Adds producer_quota/producer_quota_override/producer_usage, an in-process
   QuotaTracker (per-minute + local-midnight per-day fixed windows, flushed
   every few seconds and rebuilt on restart), and wires it into try_process
   right after the consent gate: marketing defers past its hard limit,
   transactional sends and alerts past its soft limit, auth is exempt but
   still counted. Configuration is CLI-only (messgr-control producer-quota).
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on `feat/T-042-producer-quotas-and-producer-usage-enforcement`. Do not push or
   open an MR without user approval. Present the commit message; after approval, verify
   `origin/main...HEAD` carries no `tickets/` path, then push and open the MR. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: pickle ticket new
- 2026-09-18 — TO DO → READY: plan complete
