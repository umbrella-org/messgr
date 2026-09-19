---
id: T-043
title: Quiet hours with jitter and DST handling
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-043 — Quiet hours with jitter and DST handling

## Outcome

After this ships, marketing (and transactional) messages no longer dispatch during a customer's
configured quiet hours — a message due inside the window reschedules to window-end plus jitter
instead of sending or silently piling up; auth is unaffected.

## Description

Closes build-order step 8 (§6.1) — currently schema-only: `customer.timezone` exists (migration
0008, comment: "quiet hours + local-time scheduling") but no policy table, no window-resolution
logic, and no gate step exists anywhere in the dispatcher.

Per §6.1, exactly three failure modes the naive version hits, all must be handled:

1. **Thundering herd** — reschedule to `quiet_end + random_jitter(0, 30min)`, **plain uniform
   jitter only**. An earlier design draft weighted it (transactional early in the window,
   marketing late) and that was cut as over-engineering — "a knob nobody will tune and which
   duplicates the `ORDER BY priority` preemption already in the claim query (§4.2)." Do not
   reintroduce weighted jitter (AGENTS.md "Prefer cutting to adding" names this cut explicitly).
2. **Unknown timezone** — falls back to an explicit institution default (configured per
   deployment), never to server-local time, never to "send anyway."
3. **DST** — store everything UTC, resolve windows with a real tz database (`chrono-tz`, per
   §6.1 — not yet a dependency; `chrono` itself is already in `Cargo.toml` but not the `-tz`
   crate, so this ticket adds it), and on a non-existent local time (spring-forward gap), round
   forward to the next valid instant.

Resolution order: `customer tz -> segment policy -> institution default`. Auth class skips this
gate entirely (§3, same exemption pattern as verification/consent in T-036/T-037).

**Explicitly out of scope for this ticket** (§6.3's precedence table, which needs T-040 to exist
first): quiet-hours-vs-scheduled-time precedence ("quiet hours wins"), quiet-hours-vs-expiry
interaction ("expires before window end → dropped"), and `scheduled_local` (customer-local-time
scheduling, shares this ticket's tz machinery but is T-040's API surface, not this ticket's).
Note these couplings in the Implementation Plan when refined; building precedence logic against
gates that don't exist yet would be untestable. (T-040 has since shipped — see
`src/dispatcher/worker.rs`'s Expiry gate and `outbox.expires_at`/`scheduled_for` — but §6.3's
precedence logic itself is still unbuilt and stays out of this ticket's scope; it is its own
ticket, not implied by T-040 merely existing now.)

**Design-doc gap found during refinement.** §6.1's resolution order is `customer tz -> segment
policy -> institution default`, and `quiet_hours_policy`'s schema (§4.10) supports
`scope = 'region'|'segment'|'default'`. But messgr has no customer segment attribute anywhere in
the data model — §1 puts audience segmentation out of scope by design ("upstream systems decide
who gets what and why") — and `region` (§2.2) is a deployment-topology concept already 1:1 with a
tenant's own database, not a per-customer field a policy lookup could key on. A
`scope = 'segment'` or `scope = 'region'` row could be inserted but nothing in the data model can
ever resolve a customer to one. Confirmed with the user during refinement: this ticket ships
`quiet_hours_policy` with its full `scope`/`scope_key` shape (so a future ticket can light up
segment/region resolution once an upstream system actually supplies that identity) but its
resolver and its `messgr-control` surface only ever read/write `scope = 'default'`. See Task 1
for the accompanying `05-send-timing.md` correction note.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-043-quiet-hours-with-jitter-and-dst-handling
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear at refinement time: `3-in-development/` 0/1,
`4-in-review/` 0/1.

### Confirmed design decisions (do not deviate without asking)

1. **Only `scope = 'default'` is resolved, ever.** Confirmed with the user (see the Description's
   "Design-doc gap" note). `quiet_hours_policy` keeps the full `scope`/`scope_key` columns from
   §4.10's schema; the `messgr-control quiet-hours` CLI added by this ticket has no
   `--scope`/`--scope-key` flags and always targets `('default', '')`. `quiet_hours::repo` only
   ever queries that one row.
2. **A double-invalid timezone (customer tz *and* tenant `default_timezone` both fail to parse as
   a real IANA zone) fails open — the send proceeds, the gate is skipped for that row.** Confirmed
   with the user. Quiet hours is a courtesy control (thundering-herd/DST hygiene), not a
   compliance gate like consent/suppression; a control whose own bad config can silently withhold
   a transactional or marketing send has converted a courtesy into an availability risk — the same
   reasoning AGENTS.md hard invariant 5 gives for quotas. The bad data is still visible in
   `tenant_config`/`customer` for an operator to find and fix; this ticket adds no new alerting for
   it.
3. **No message-class check in the gate.** `auth` never reaches the outbox at all (T-011 decision
   3), matching T-036 decision (verification)/T-037's own class handling — an explicit
   `matches!` guard would be dead branching against a row shape that can't occur.
4. **Gate placement: in `try_process` (`src/dispatcher/worker.rs`), immediately after the Consent
   gate block and before `repo::load_ciphertexts`.** Cheapest-checks-first, matching every gate
   already in this function — a deferred send never spends a decrypt or DEK fetch. This is *after*
   Suppression/Verification/Consent in the actual code (which already runs in a different order
   than DESIGN.md §5's table — see those tickets' own decisions), not because ordering among them
   matters functionally, but because it's the natural next slot before the one genuinely
   expensive step (decrypt).
5. **The reschedule reuses `repo::reschedule_retry` (T-021) unchanged — no new repo function, no
   new `comms_event` row.** It already does exactly what quiet hours needs: clear the lease, set
   `next_attempt_at`, in one statement. `reschedule_retry`'s own doc comment already establishes
   the "no event write" precedent for a non-terminal reschedule (§2.4 step 6: "the row stays in
   the outbox"); quiet hours is the same shape of non-terminal defer, so no new event type is
   added to `comms_event`'s comment-documented vocabulary.
6. **`tenant_config.default_timezone` and `quiet_hours_policy`'s one row are both loaded once at
   `messgr-dispatcher` startup and held on `DispatcherContext`, exactly like `verification_mode`
   (T-036) — not hot-reloaded via `NOTIFY` like kill switches.** A quiet-hours window is an
   administrative setting, not an incident-response control; there is no stated latency
   requirement for it the way §5.2 states one for kill switches. A change takes effect on the next
   dispatcher restart.
7. **Fall-back DST ambiguity (two valid UTC instants for one local wall-clock time) resolves to
   the earlier one.** DESIGN.md §6.1 only specifies gap (spring-forward) behaviour, not this case;
   either offset is a defensible, deterministic choice, and this only affects the tail end of a
   deferred send by at most an hour.
8. **The spring-forward round-forward search (§6.1: "round forward to the next valid instant") is
   bounded to 4 hours, marked `ponytail:`.** Covers every real single-jump DST transition on
   record (usually one hour; the largest, Portugal 1992, was two) without an unbounded loop. A
   full-day political re-zoning (e.g. Samoa's December 2011 date-line jump) is out of scope — if
   `chrono-tz` ever routes one through this path the function returns `None` (gate skipped, decision
   2's same fail-open reasoning) rather than looping forever.
9. **`quiet_hours_policy` needs no `§7.2` erasure statement.** It carries no `customer_id` and no
   PII — an institution-wide window, not a per-customer row.
   `tests/erasure_coverage.rs`'s COVERED/EXEMPT check keys off a `customer_id` column or FK chain
   to `customer`, which this table has neither of, so it is out of that check's scope by
   construction, not by omission.

### Tasks

#### Task 1 — Design-doc correction

`development/design/05-send-timing.md`: directly beneath §6.1's `policy:`/`resolve:` code block
(before "Auth class skips this evaluation entirely (§3)."), add a correction note — matching the
doc's existing correction-callout style — stating that the `segment`/`region` scopes in
`quiet_hours_policy` are currently unreachable (no customer segment attribute exists per §1's
scope boundary; `region` is a deployment concept already 1:1 with a tenant's database per §2.2),
that T-043 ships the table's full shape but only ever resolves `scope = 'default'`, and that the
effective resolution order today is `customer tz -> institution default`, not the three-tier
version the prose above it describes.

#### Task 2 — `chrono-tz` dependency

`Cargo.toml`: add `chrono-tz = "0.10"` under the existing `chrono = { version = "0.4", ... }`
line. Run `cargo build` once to confirm it resolves against the pinned `chrono` version and
update `Cargo.lock`.

#### Task 3 — Schema migration

`migrations/tenant/0016_quiet_hours_policy.sql`, verbatim from `03-data-model.md`'s §4.10
`CREATE TABLE quiet_hours_policy` snippet (already carrying the doc's own prior correction for
`scope_key`'s `NOT NULL DEFAULT ''`):

```sql
-- Quiet-hours policy (DESIGN.md §4.10, §6.1, T-043). scope_key defaults to
-- '' (NOT NULL, not nullable) so the institution-wide default row --
-- (scope, scope_key) = ('default', '') -- has a representable primary key;
-- a PRIMARY KEY column is implicitly NOT NULL, so a nullable scope_key
-- could never hold that row at all (see 03-data-model.md's own correction
-- note). scope='region'|'segment' rows are representable but currently
-- unreachable -- see 05-send-timing.md's §6.1 correction note; only
-- ('default', '') is ever read or written by this ticket's code.
CREATE TABLE quiet_hours_policy (
    scope       text NOT NULL,
    scope_key   text NOT NULL DEFAULT '',
    start_local time NOT NULL,
    end_local   time NOT NULL,
    PRIMARY KEY (scope, scope_key)
);
```

#### Task 4 — `quiet_hours` module: model + repo

New files, mirroring `src/consent/`'s shape.

`src/quiet_hours/model.rs`:

```rust
use chrono::NaiveTime;

pub const DEFAULT_SCOPE: &str = "default";
pub const DEFAULT_SCOPE_KEY: &str = "";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuietHoursPolicy {
    pub scope: String,
    pub scope_key: String,
    pub start_local: NaiveTime,
    pub end_local: NaiveTime,
}

#[derive(Debug, Clone)]
pub struct QuietHoursPolicyInput {
    pub start_local: NaiveTime,
    pub end_local: NaiveTime,
}

impl QuietHoursPolicyInput {
    pub fn matches(&self, existing: &QuietHoursPolicy) -> bool {
        self.start_local == existing.start_local && self.end_local == existing.end_local
    }
}
```

`src/quiet_hours/repo.rs`:

```rust
use sqlx::PgPool;

use super::model::{DEFAULT_SCOPE, DEFAULT_SCOPE_KEY, QuietHoursPolicy, QuietHoursPolicyInput};

/// The one row this ticket ever reads (decision 1) -- `messgr-dispatcher`
/// startup and `quiet-hours show` both call this, never a scope-parameterized
/// lookup.
pub async fn load_default(pool: &PgPool) -> Result<Option<QuietHoursPolicy>, sqlx::Error> {
    sqlx::query_as::<_, QuietHoursPolicy>(
        "SELECT scope, scope_key, start_local, end_local FROM quiet_hours_policy \
         WHERE scope = $1 AND scope_key = $2",
    )
    .bind(DEFAULT_SCOPE)
    .bind(DEFAULT_SCOPE_KEY)
    .fetch_optional(pool)
    .await
}

pub async fn upsert_default(
    pool: &PgPool,
    input: &QuietHoursPolicyInput,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO quiet_hours_policy (scope, scope_key, start_local, end_local)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (scope, scope_key) DO UPDATE SET
            start_local = EXCLUDED.start_local,
            end_local = EXCLUDED.end_local
        "#,
    )
    .bind(DEFAULT_SCOPE)
    .bind(DEFAULT_SCOPE_KEY)
    .bind(input.start_local)
    .bind(input.end_local)
    .execute(pool)
    .await
    .map(|_| ())
}
```

`src/dispatcher/repo.rs`: add, next to `load_verified_at`:

```rust
/// `customer.timezone` for the quiet-hours gate (DESIGN.md §5, §6.1, T-043).
/// The column itself is `NOT NULL`, but there is no FK tying `outbox.customer_id`
/// to `customer.id` (T-009 decision 1, same absence `load_verified_at` notes for
/// `address_id`), so a missing row still collapses to `None` here -- the caller
/// falls back to the tenant's `default_timezone` either way (decision 2).
pub async fn load_customer_timezone(
    pool: &PgPool,
    customer_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT timezone FROM customer WHERE id = $1")
        .bind(customer_id)
        .fetch_optional(pool)
        .await
}
```

#### Task 5 — `quiet_hours::window` (pure resolution logic)

`src/quiet_hours/window.rs` — no I/O, unit-tested directly (no local stack needed):

```rust
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
/// customer in `customer_tz` -- the UTC instant to reschedule to (window end
/// + uniform jitter, decision matching §6.1's thundering-herd fix). `None`
/// means the send may proceed now.
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
            LocalResult::Ambiguous(earliest, _) => return Some(earliest.with_timezone(&Utc)),
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
        assert_eq!(resolve_reschedule(now, "Europe/London", "Europe/London", &p), None);
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
        assert!(resolve_reschedule(now, "not-a-real-zone", "Europe/London", &p).is_some());
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
        let result = resolve_reschedule(now, "America/New_York", "America/New_York", &p)
            .expect("must reschedule");
        let expected = Utc.with_ymd_and_hms(2024, 3, 10, 7, 0, 0).unwrap();
        assert!(result >= expected && result <= expected + Duration::minutes(30));
    }
}
```

#### Task 6 — `quiet_hours::configure` (actor-facing operations)

`src/quiet_hours/configure.rs`, mirroring `src/tenant_config/configure.rs`'s `set`/`show` shape
(no per-address resolution needed here, unlike `consent::configure` -- this is institution-wide,
not per-customer):

```rust
pub async fn set_quiet_hours_policy(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    input: QuietHoursPolicyInput,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> { /* resolve tenant (audit-then-reject on unknown,
    T-005/F1), connect tenant pool, repo::load_default to decide created/updated/idempotent,
    repo::upsert_default when not idempotent, audit "quiet_hours_policy.set", close pool */ }

pub async fn show_quiet_hours_policy(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Option<QuietHoursPolicy>, ConfigureError> { /* resolve tenant, connect tenant pool,
    repo::load_default, close pool -- no audit, matches tenant_config::show_tenant_config */ }
```

Same `ConfigureError`/`rejected()`/`ConfigureOutcome` boilerplate as `tenant_config::configure`.
`audit()` records `quiet_hours_policy.set` on `platform_audit` with
`{"start_local": ..., "end_local": ..., "outcome": ...}`.

#### Task 7 — register the module

`src/lib.rs`: add `pub mod quiet_hours;` alongside `pub mod provider_config;` (alphabetical
position, between `platform_audit` and `producer`).

`src/quiet_hours/mod.rs`:

```rust
pub mod configure;
pub mod model;
pub mod repo;
pub mod window;
```

#### Task 8 — dispatcher wiring

`src/dispatcher/worker.rs`:

- Add imports: `use crate::quiet_hours::{model::QuietHoursPolicy, window};`
- `DispatcherContext` gains two fields, documented like `verification_mode`:
  ```rust
  /// `tenant_config.default_timezone` (DESIGN.md §5, §6.1, T-043) -- loaded
  /// once at startup, like `verification_mode`, never hot-reloaded.
  pub default_timezone: String,
  /// The tenant's one `quiet_hours_policy` row (`scope = 'default'`,
  /// T-043 decision 1), `None` until an operator runs `quiet-hours set` --
  /// the gate is then a no-op, not a block, since there is nothing to
  /// enforce yet.
  pub quiet_hours_policy: Option<QuietHoursPolicy>,
  ```
- In `try_process`, immediately after the Consent gate block and before `repo::load_ciphertexts`
  (decision 4):
  ```rust
  // Quiet hours gate (DESIGN.md §5, §6.1, T-043) -- no class check needed
  // (decision 3; auth never reaches the outbox, T-011 decision 3). `None`
  // policy means the tenant hasn't configured a window yet.
  if let Some(policy) = ctx.quiet_hours_policy.as_ref() {
      let customer_tz = repo::load_customer_timezone(&ctx.pool, row.customer_id)
          .await?
          .unwrap_or_else(|| ctx.default_timezone.clone());
      if let Some(next_attempt_at) = window::resolve_reschedule(
          Utc::now(),
          &customer_tz,
          &ctx.default_timezone,
          policy,
      ) {
          repo::reschedule_retry(&ctx.pool, row.comms_request_id, next_attempt_at).await?;
          return Ok(());
      }
  }
  ```

`src/bin/dispatcher.rs`:

- Add `const DEFAULT_TIMEZONE: &str = "UTC";` alongside `DEFAULT_VERIFICATION_MODE`.
- After `verification_mode` is resolved, add:
  ```rust
  let default_timezone = tenant_config
      .as_ref()
      .map(|c| c.default_timezone.clone())
      .unwrap_or_else(|| DEFAULT_TIMEZONE.to_string());
  let quiet_hours_policy = messgr::quiet_hours::repo::load_default(&tenant_pool)
      .await
      .expect("loading quiet_hours_policy failed");
  ```
- In the `DispatcherContext { .. }` literal (per-channel loop), add
  `default_timezone: default_timezone.clone(), quiet_hours_policy: quiet_hours_policy.clone(),`.

Every existing test-side `DispatcherContext { .. }` literal in `tests/dispatcher.rs` (and any
other test file constructing one) needs the same two fields added -- grep
`DispatcherContext {` across `tests/` and add `default_timezone: "UTC".to_string(),
quiet_hours_policy: None,` (no policy configured -- gate is a no-op) to each, unless a specific
test is exercising the gate itself (Task 9's new tests set `quiet_hours_policy: Some(..)`
explicitly).

#### Task 9 — CLI wiring (`src/bin/control.rs`)

Add a `Command::QuietHours { command: QuietHoursCommand }` variant, alongside `Suppression`/
`Consent`:

```rust
/// Set or show the tenant's institution-wide quiet-hours window (DESIGN.md
/// §5, §6.1, T-043), enforced by `messgr-dispatcher`'s quiet-hours gate at
/// send time. Only the institution-wide default is settable -- see
/// `05-send-timing.md`'s §6.1 correction note for why per-segment/region
/// windows aren't exposed here.
QuietHours {
    #[command(subcommand)]
    command: QuietHoursCommand,
},
```

```rust
fn parse_local_time(value: &str) -> Result<chrono::NaiveTime, String> {
    chrono::NaiveTime::parse_from_str(value, "%H:%M")
        .map_err(|_| format!("expected HH:MM (24-hour), got {value:?}"))
}

#[derive(Subcommand)]
enum QuietHoursCommand {
    /// Create or overwrite the tenant's quiet-hours window. Safe to re-run:
    /// identical inputs are an idempotent no-op.
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "start-local", value_parser = parse_local_time)]
        start_local: chrono::NaiveTime,
        #[arg(long = "end-local", value_parser = parse_local_time)]
        end_local: chrono::NaiveTime,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
    /// Show the tenant's quiet-hours window, or report that none is set.
    Show {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}
```

`run()` gets a new arm, `Command::QuietHours { command }`, matching `TenantConfig`'s shape
(control-database-only, no Vault connection needed) -- `Set` calls `set_quiet_hours_policy` and
prints `outcome=<outcome>`; `Show` calls `show_quiet_hours_policy` and prints the window or "no
quiet-hours policy configured for tenant <slug>".

`mod tests` (clap-parsing only, no DB, mirroring `tenant_config_set_parses_every_flag`'s
neighbours): `quiet_hours_set_parses_every_flag`, `quiet_hours_set_rejects_an_invalid_time`,
`quiet_hours_show_parses`.

### Acceptance test

**`tests/quiet_hours.rs`** (new file, following `tests/consent.rs`'s conventions -- real
provisioning against the local stack, no mocks):

- `setting_and_reading_back_round_trips_start_and_end`
- `setting_again_with_a_different_window_updates_the_row_without_duplicating_it`
- `resetting_with_identical_inputs_is_idempotent`
- `set_against_an_unknown_tenant_slug_is_rejected_and_audited` (mirrors `consent.rs`'s own
  T-005/F1 test)

**`src/quiet_hours/window.rs`'s own `#[cfg(test)]` module** (Task 5, pure, no DB) covers the
resolution logic itself: same-day window, midnight-wrap window, outside-window, tz fallback,
double-invalid-tz fail-open, and the DST spring-forward gap.

**`tests/dispatcher.rs`** -- add two gate-integration tests, following the existing
`enforce_blocks_unverified_address_with_terminal_event_and_no_send`/
`marketing_without_any_consent_row_is_blocked_with_suppressed_consent` pattern (claim a row,
`try_process`, assert on `outbox`/`comms_request`/`comms_event`):

1. **`a_send_during_quiet_hours_is_deferred_to_window_end_plus_jitter`.** Build a
   `DispatcherContext` with `quiet_hours_policy: Some(policy)` (a window that contains "now" in
   `default_timezone: "UTC"`), write a ready outbox row, claim it, `try_process`. Assert: the
   outbox row still exists (not deleted), `leased_until` is `NULL`, `next_attempt_at` is between
   the window's end and window end + 30 minutes; no `comms_event` row was written; the wiremock
   mock (mounted `.expect(0)`) never receives a request.
2. **`a_send_outside_quiet_hours_is_unaffected`.** Same setup, a window that does not contain
   "now". Assert: normal `sent` event, `final_status = Some("sent")`, outbox row removed, mock
   called once -- proves the gate is not a universal block when `quiet_hours_policy` is
   configured but the row falls outside the window.

Run: `just build && just test && just lint`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/control-plane-cli.adoc`: new `== Quiet hours` section (after `== Consent`,
  same style -- command example, then prose) documenting `set`/`show`, the `HH:MM` 24-hour time
  format, that only the institution-wide default is settable today, and that a change takes
  effect on the dispatcher's next restart (decision 6).
- `docs/user-manual/dispatcher.adoc`: new paragraph in the existing `T-036`/`T-037`/`T-041`
  paragraph style, placed after the `T-037` consent paragraph and before the `T-041` cancellation
  paragraph (matching `try_process`'s actual order, decision 4): the quiet-hours gate runs next,
  before decrypt; a customer inside their configured window has `next_attempt_at` pushed to window
  end plus up to 30 minutes of jitter rather than being sent or blocked; `default_timezone` and the
  quiet-hours window are both read once at startup, not hot-reloaded; no policy configured means
  the gate is a no-op. See "Quiet hours" (control-plane CLI) for managing the window.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` clean.
2. `docs/user-manual/control-plane-cli.adoc` and `docs/user-manual/dispatcher.adoc` updated per
   above.
3. Write a summary (files touched, decisions made, anything deferred) and hand back.
4. Suggested commit message:

   ```
   feat(dispatcher): add quiet-hours gate with jitter and DST handling (T-043)

   Adds quiet_hours_policy (institution-wide window only) and its module/CLI,
   plus a dispatch-time gate that defers a send inside the window to window
   end + uniform jitter, resolved through a real tz database with unknown-tz
   and spring-forward-gap handling per DESIGN.md §6.1.
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on `feat/T-043-quiet-hours-with-jitter-and-dst-handling`. Do not push or open an
   MR without user approval. Present the commit message; after approval, verify
   `origin/main...HEAD` carries no `tickets/` path, then push and open the MR. Merging is the
   human's.

## Review

- [x] Reviewer independence settled (step 0): **independent** — reviewing session started fresh
  (`/clear`) with no memory of authoring `feat/T-043-quiet-hours-with-jitter-and-dst-handling`;
  a reviewer with no hand in the branch needs no delegation.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a)
- [x] Docs-readability pass — **conscious skip**: no docs-readability reviewer configured in this
  host session (step 4b, optional)
- [x] Findings recorded with severity, class, and disposition; disposition summary + cost line
  present (step 5)
- [x] Ticket moved; `## History` appended (step 6)
- [x] Other references updated; governing documents reconciled (step 7)
- [x] Remaining-tickets impact sweep done (step 8) — `2-ready/` and `1-to-do/` empty of any
  `depends-on: [T-043]` or Description reference; only `T-044` is in `1-to-do/` and it is
  unrelated
- [x] Summary + commit message & MR attributes presented for approval; overarching bookkeeping
  committed per policy; next-ticket suggestion (step 9)

**Implementation audit (steps 1–2).** Read the ticket from `main` (the feature-branch worktree
still showed a stale `3-in-development/` copy — the exact in-tree staleness hazard the protocol
warns about). All 9 tasks verified against the tree: `chrono-tz = "0.10"` added
(`Cargo.toml`/`Cargo.lock`, resolves clean); `migrations/tenant/0016_quiet_hours_policy.sql`
verbatim from `03-data-model.md` §4.10; `src/quiet_hours/{model,repo,window,configure,mod}.rs`
and the `dispatcher.rs`/`worker.rs`/`control.rs` wiring all match the plan's code blocks; every
existing `DispatcherContext { .. }` literal (`tests/dispatcher.rs`, `tests/kill_switch.rs`)
updated with the two new fields. Re-ran the acceptance test verbatim plus the full suite:
`just build` clean, `just lint` clean (`cargo fmt --check` + `cargo clippy --all-targets
--all-features -D warnings`), `just docs-check` clean, `just test` — 90+ tests green including
the new `tests/quiet_hours.rs` (4), `src/quiet_hours/window.rs`'s unit tests (6, covering
same-day/midnight-wrap/outside-window/tz-fallback/double-invalid-fail-open/DST-spring-forward),
and the two new `tests/dispatcher.rs` gate-integration tests. All 9 confirmed design decisions
honoured, including decision 9 (`tests/erasure_coverage.rs` passes — `quiet_hours_policy` has no
`customer_id`/FK, out of that check's scope by construction).

**Quality audit (step 3, addendum advisory).** The two new gate-integration tests use real,
mutation-testable assertions, not `is_err()`/`is_ok()` tautologies: a mounted mock with
`.expect(0)`/`.expect(1)`, `leased_until`/`next_attempt_at`/`comms_event` row counts, and
`final_status`. Removing the gate would fail `a_send_during_quiet_hours_is_deferred…`'s mock
expectation; removing the reschedule would fail `a_send_outside_quiet_hours_is_unaffected`'s.

**Consistency audit (step 4).** Gate order in `worker.rs` matches decision 4 (after Consent,
before decrypt) and `dispatcher.adoc`'s narrative. `AGENTS.md` invariants 1 and 3 grepped: auth
never reaches the outbox (no class check needed, decision 3, consistent with T-011/T-036/T-037);
the gate runs in `try_process` at dispatch time, not at ingest. No NULL-in-unique-index issue
(`quiet_hours_policy`'s PK columns are both `NOT NULL`). No secret read from env/config — no
secrets involved in this ticket. Found two stale governing-document references this branch made
false (F1, F2 below); both fixed inline per the addendum's step 5 ("do not defer a design
correction to a follow-up ticket").

**Documentation audit (step 4a).** `docs/user-manual/control-plane-cli.adoc` and
`dispatcher.adoc` both updated, correctly cross-referenced, no duplication. `just docs-check`
clean.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | stale-xref | fixed inline | `03-data-model.md` still said `quiet_hours_policy` "is an unrelated table not yet created (later ticket: quiet-hours resolution)" — T-043 created it | `development/design/03-data-model.md` (pre-fix, ~line 467) | Corrected to describe the shipped table and cross-reference `05-send-timing.md`'s existing correction note; fixed on `feat/T-043-…` (commit b55fc96) |
| F2 | non-blocking | stale-xref | fixed inline | `14-decisions-and-open-questions.md` Still-open #2 posed "the actual windows per region" as an undecided *value*, when T-043's own Description/decisions establish region resolution is structurally unreachable (no per-customer region attribute), same as segment | `development/design/14-decisions-and-open-questions.md` (pre-fix, Still-open #2) | Reworded to separate the still-genuinely-open institution-wide default value from the no-longer-open region/segment reachability question; fixed on `feat/T-043-…` (commit b55fc96) |

Disposition summary: 2 fixed inline (F1, F2). 0 folded, 0 new ticket, 0 noted. No blocking
findings.

cost: estimated M, actual M

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — last of the next-batch-of-5 (T-039-T-043), lowest urgency since it's schema-only
  today rather than a live gap in an already-claimed control.
- 2026-09-18 — TO DO → READY: plan complete
- 2026-09-18 — READY → IN DEVELOPMENT: picked up
- 2026-09-18 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-19 — IN REVIEW → DONE: reviewed: 0 blocking, F1+F2 stale-xref fixed inline
