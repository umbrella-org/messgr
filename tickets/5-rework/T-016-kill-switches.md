---
id: T-016
title: Kill switches
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-016 — Kill switches

## Outcome

An operator can stop message flow at any of five scopes (global, channel, producer,
producer_channel, campaign): new ingests matching the scope are rejected with a distinct error
code, dispatch stops for matching queued rows, and queued rows are held (not discarded, unless
`on_queued = 'discard'` is set) — all within seconds of the switch firing, and fully audited.
Auth traffic is structurally unaffected by any switch scope. A suspended or offboarding tenant's
producers are rejected at `POST /comms` the same way, closing the gap T-011/F3 found. The
control database gains an `auth_enabled` column and its own `platform_audit` trail, ready for
the operator runbook that will flip it, ahead of the OTP path that will read it.

## Description

Builds the kill-switch mechanism from §5.2: five scopes, hold-by-default with an explicit
discard option, and a dedicated `NOTIFY kill_switch` channel (separate from the outbox wakeup of
§4.2) with a 30-second fallback re-read so propagation is seconds, not minutes. Release ramps a
drained backlog at a configured rate rather than dispatching it all at once the instant a switch
lifts, and the `expires_at` check (§6.2) runs first so stale held messages drop instead of
arriving hours late. Every engage/release is audited: who, when, why, how many messages held or
discarded.

This is build-order step 4 (§14) — deliberately early, before the system can send at volume,
per AGENTS.md's stated build-order reasoning ("kill switches before volume"). A database-level
switch plus a `psql` runbook is sufficient for this ticket; the operator-facing panel is step 14
and out of scope here.

**Hard invariant, not negotiable in this ticket's design:** kill switches never touch the OTP
path (AGENTS.md invariant #1, §3) — OTP shares no process with the dispatcher, so no switch
scope can reach it. The two-person-approval auth-disable control lives in `sms-sender` /
`otp-api` via a separate `auth_enabled` flag polled from the control database, not through
`kill_switch` at all (§5.2) — building that flag and its approval flow is in scope here since
it is the one auth-adjacent control this ticket owns, but the two-person approval *mechanism*
itself is an open question (§"Still open" #10: built into a panel, or an out-of-band process the
panel merely records?) and needs a decision before this ticket can reach READY.

Out of scope: the platform-tier switch (`platform_kill_switch`, control database, §4.11) — that
is cloud-only and this build order defers cloud enablement to step 19; single-tenant on-prem
only needs the tenant-scoped `kill_switch`.

### Folded in from the 2026-09-02 design/implementation audit

Five items, all touching this ticket's own schema or mechanism, folded in rather than filed
separately per the promotion test:

- **`kill_switch`'s NULL-key uniqueness bug.** DESIGN.md's `kill_switch` schema is corrected
  (this audit) to `CREATE UNIQUE INDEX ON kill_switch (scope, COALESCE(scope_key, '')) WHERE
  released_at IS NULL` — the original `(scope, scope_key)` form let two `global`-scope switches
  (`scope_key IS NULL` on both) be active simultaneously, since Postgres treats every NULL as
  distinct. Build against the corrected schema, not the one that shipped in an earlier
  DESIGN.md revision.
- **"Held" needs a representation, or it spins.** DESIGN.md §5.2 (corrected) now specifies:
  the dispatcher must check cached kill-switch state *before* claiming, excluding matching
  channels/producers/campaigns from the claim query's candidate set — not lease-then-gate-block,
  which busy-waits for the life of the switch as each lease expires and the row is re-claimed
  only to be blocked again. This is a design decision this ticket must implement, not an
  optimization to defer.
- **Ingest-side rejection has no propagation path as originally specified.** The `NOTIFY
  kill_switch` mechanism above is read over the dispatcher's *direct* Postgres connection
  (§2.3); `messgr-ingest` connects via PgBouncer transaction mode, where `LISTEN` never fires.
  Point 1 of this ticket's own Outcome ("new ingests matching the scope are rejected") needs
  its own mechanism: `ingest-api` polling `kill_switch` on its pooled connection every few
  seconds and caching the active set in-process, independent of the dispatcher's `NOTIFY` path.
- **`auth_enabled` must fail open, not closed.** This ticket's Description above already
  states the flag is "polled from the control database" — DESIGN.md's §5.2 correction (this
  audit) requires it fail **open** (auth stays enabled) on a read failure or timeout, with
  alerting on the failure. Failing closed would mean a control-database outage — unrelated to
  any marketing incident or intentional switch — silently disables customer login, which is
  the exact failure mode AGENTS.md hard invariant 1 exists to prevent. Confirm this explicitly
  during refinement; it is easy to default to "fail closed" as the safe-looking choice and get
  this one backwards.
- **T-011/F3 — `tenant.status` is never checked.** A tenant marked `suspended` or
  `offboarding_*` (constants already exist in `src/tenant/model.rs`, currently
  `#[allow(dead_code)]`) whose producer certs are still `enabled` can still submit through
  `messgr-ingest` and get a `201`. Noted in T-011's review as belonging to "whichever future
  ticket owns tenant offboarding enforcement" — no such ticket existed then. Suspension is an
  abuse/administrative control in the same family as a kill switch (both stop ingest for a
  scope), so it belongs here rather than waiting for full offboarding tooling (§7.7, step 19).
  Scope for this ticket: check `tenant.status` at the same point ingest resolves producer
  identity, rejecting with a distinct error code the same way an engaged kill switch does.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-016-kill-switches
```

Local WIP commits as you go. Tidy into atomic commits before presenting (root-path child,
`tickets/README.md` §0). No push / MR without explicit user approval.

### Prerequisite gate (hard)

None. Every build-order step this ticket depends on (0b–3) is in `6-done/` and merged: T-001–T-015,
T-017. `depends-on: []` is correct as filed.

### Confirmed design decisions (do not deviate without asking)

1. **`kill_switch` lives in the tenant database**, schema exactly as DESIGN.md §4.9 now reads
   (already corrected by this audit): `CREATE UNIQUE INDEX ON kill_switch (scope,
   COALESCE(scope_key, '')) WHERE released_at IS NULL`. New migration
   `migrations/tenant/0009_kill_switch.sql`, plus a `kill_switch_notify` trigger firing
   `pg_notify('kill_switch', ...)` on `INSERT OR UPDATE`, following
   `migrations/tenant/0007_outbox_notify.sql`'s exact pattern but its own channel name and (per
   §5.2) no per-channel fan-out — one topic for the whole table.
2. **The dispatcher checks kill-switch state before claiming, never leases-then-blocks**
   (DESIGN.md decision 28). A new `Arc<kill_switch::cache::KillSwitchCache>` holds the current
   active-switch set in memory, refreshed by a dedicated `LISTEN kill_switch` connection with a
   30-second poll fallback — its own channel and interval, separate from
   `outbox_<channel>`/1s (§5.2 is explicit these must not share either). One cache per dispatcher
   process, shared across every per-channel loop (scopes like `global`/`producer`/`campaign`
   aren't channel-specific), built once in `src/bin/dispatcher.rs::main` and cloned into every
   `DispatcherContext` (`src/dispatcher/worker.rs`) alongside `pool`/`keystore`/`cache`.
3. **`repo::claim` gains exclusion, not a post-claim filter.** `src/dispatcher/repo.rs::claim`
   takes the cache's currently-active scopes: skip the query outright for this tick if `global`
   is active or a `channel`-scope switch matches this loop's own channel; otherwise extend the
   `WHERE` clause to exclude `producer_id`s under an active `producer`/`producer_channel` switch
   (channel already pinned per loop) and `campaign_id`s under an active `campaign` switch, via
   `<> ALL($n)` array parameters built from the cache snapshot.
4. **Release ramps via a one-shot background drain task that claims its own rows directly —
   not by bumping `next_attempt_at` and reopening the normal claim query.** *(Revised during
   implementation — see the plan-amendment History line below for why the plan as originally
   written here didn't actually ramp anything.)* A released scope's `outbox` rows already have
   `next_attempt_at` in the past, so the moment the shared cache stops excluding a released
   scope, the *normal* claim loop would see the whole backlog at once regardless of any
   `next_attempt_at` bump — there is no way to make a bump "trickle" rows in through a query
   that has no per-row admission state to check. Instead: the shared `KillSwitchCache` only ever
   tracks *engaged* switches (`released_at IS NULL`) — released switches drop out of it
   immediately, which is exactly right for `messgr-ingest` (a release must unblock new sends at
   once). The dispatcher additionally tracks its own `draining: Arc<RwLock<HashMap<Uuid,
   KillSwitch>>>` (one map shared by all this process's channel loops, not exposed to ingest):
   `KillSwitchCache::refresh`'s returned `RefreshDelta.released` entries are inserted here
   *before* anything else happens, so there is no window where a released `hold` scope is
   excluded by neither map. `DispatcherContext::claim_exclusion` folds both maps together via
   `kill_switch::cache::exclusion_for_channel`, so the normal claim loop keeps excluding a
   draining scope exactly as if it were still engaged. The drain task itself
   (`dispatcher::drain::drain_released_scope`, one per channel this process runs, since sending
   needs that channel's own `Sender`) is the *only* thing allowed to claim a draining scope's
   rows: `dispatcher::repo::claim_for_scope` leases up to `kill_switch_release_rate` matching
   rows directly (the inverse of the normal claim's exclusion — it matches the switch's scope
   instead of excluding it), sleeping one second between batches, until the scope's backlog is
   empty — at which point it's removed from `draining` and the exclusion lifts for good. A row
   whose `expires_at` has passed by the time it's drained gets the existing `expired` terminal
   write (`repo::write_terminal`, reusing T-013's path) instead of being sent. No new `outbox`
   column — `next_attempt_at`/`expires_at` already exist (T-009); no new state on the switch row
   either — "draining" is process-local, derived from the cache diff, never persisted. `draining`
   is a plain `std::sync::RwLock`, not `tokio::sync::RwLock`: the insert must happen
   synchronously, in the same refresh tick that removes the switch from the shared cache's
   engaged set — doing it inside a spawned task instead (an `.await` away) reopens the exact
   same window this decision exists to close, for however long it takes that task to be
   scheduled.
5. **`kill_switch_release_rate` is a new `tenant_config` column** (`int NOT NULL DEFAULT 500`,
   rows/second), following T-007's established shape for an operator-tunable value: migration,
   `TenantConfig`/`TenantConfigInput` fields + `matches()`, `repo.rs` load/upsert,
   `configure.rs`, and a `--kill-switch-release-rate` flag on `messgr-control tenant-config-set`
   (`src/bin/control.rs`).
6. **`ingest-api` gets its own poll, independent of `LISTEN`**, because it connects via PgBouncer
   transaction mode where `LISTEN` never fires (§2.3, §5.2's own correction). `TenantContext`
   (`src/tenant/registry.rs`) gains a `kill_switches: Arc<kill_switch::cache::KillSwitchCache>`
   field — same cache type as the dispatcher's, minus the `LISTEN` half — populated by a
   background poll every 5 seconds ("every few seconds", §5.2), spawned the first time
   `get_or_open` opens that tenant's context. `create_comms` (`src/ingest/handler.rs`) checks it
   before any resolution/template/DEK work and rejects with a new
   `IngestError::KillSwitchEngaged { scope: String }` → `503 Service Unavailable` (temporary and
   distinct from every existing 4xx rejection, per §5.2's "not a generic 500").
7. **`tenant.status` is checked fresh at identity resolution, never from the cached
   `TenantContext`.** `TenantContext` is opened once per process and held for its life
   (`registry.rs`'s own doc comment) — a status flip after first use would never be re-observed
   there. Instead, `cert_repo::find_producer_cert` (`src/producer/cert_repo.rs`) joins `tenant`
   and returns `status` alongside its existing columns; it already runs fresh on every request,
   so this needs no cache or poll of its own. `resolve_producer`
   (`src/producer/resolve.rs`) gains `ResolutionError::TenantNotActive { tenant_id, producer_id
   }` when `status != 'active'`, mapped to `IngestError::TenantNotActive` → `403 Forbidden`
   (matching `ProducerDisabled`'s status code; distinct message/body). Allowlisting `active`
   (rather than blocklisting `suspended`/`offboarding_*`) also rejects a stray request against a
   still-`provisioning` tenant, which is strictly safer and free — no producer cert should exist
   for one yet. Closes T-011/F3.
8. **`auth_enabled` ships as schema + audit only** (confirmed during refinement: otp-api/
   sms-sender, its only reader, is build-order step 17 and doesn't exist, so no poll/fail-open
   logic can be built or tested against a real caller yet). `migrations/control/
   0005_tenant_auth_enabled.sql` adds `tenant.auth_enabled boolean NOT NULL DEFAULT true`, with a
   migration comment naming step 17 as the eventual reader — the same pattern `tenant.status`'s
   `#[allow(dead_code)]` offboarding constants already use in `src/tenant/model.rs`. No Rust
   reads or writes it in this ticket. Engaging/releasing it is a documented `psql` runbook, same
   shape as `kill_switch`: one transaction doing `UPDATE tenant SET auth_enabled = ...` and
   `INSERT INTO platform_audit` (`src/platform_audit.rs`'s existing table/columns) recording both
   approvers' identities in `detail` — the two-person approval is an out-of-band human process
   this ticket records, not enforces (DESIGN.md decision 29, Still Open #9 resolved for this
   step).
9. **`platform_kill_switch` is untouched.** Cloud-only, unread until step 19 (§4.11's own column
   comment) — nothing here references it.
10. **No new erasure surface.** `kill_switch` holds no customer data (scope/reason/operator
    identity only) — review-addendum step-2 item 5 does not apply; state this explicitly so a
    reviewer doesn't have to re-derive it.
11. **A `discard` switch's backlog is handled the same way as release-drain, but at engage time.**
    *(Added during implementation — not in the original plan; see the plan-amendment History
    line.)* The Outcome/Description above already promised `on_queued = 'discard'` behaviour, but
    decision 4 as originally written only covered release and never specified an engage-time
    mechanism at all. `KillSwitchCache::refresh` also returns `RefreshDelta.newly_engaged`; for
    any `on_queued = 'discard'` entry there, `dispatcher::drain::discard_engaged_scope` claims
    that scope's matching rows via the same `claim_for_scope` decision 4 introduces (with
    `channel: None`, since discarding never sends and so needs no channel-specific `Sender` — one
    task covers every channel at once) and immediately writes each an `expired`-shaped
    `discarded` terminal state, looping until the scope's backlog is empty. No pacing needed
    (nothing is sent), and no interaction with `draining` — a `discard` switch's rows are gone
    before release is ever relevant.

### Tasks

#### Task 1 — `kill_switch` schema
`migrations/tenant/0009_kill_switch.sql`: the `CREATE TABLE kill_switch` + corrected unique
index from DESIGN.md §4.9, plus the `kill_switch_notify` trigger (decision 1).

#### Task 2 — kill-switch cache module
New `src/kill_switch/{mod.rs,model.rs,repo.rs,cache.rs}` (as planned, minus `find_released_since`
— superseded, see decision 4's amendment): `model.rs` the `KillSwitch` row, `scope`/`on_queued`
constants (matching `tenant::model::status`'s `&'static str` convention), and `matches`/
`producer_id`/`producer_channel_parts` helpers; `repo.rs` `list_active` only — the drain trigger
turned out to need a *diff* against the cache's own previous snapshot, not a separate query, so
it lives in `cache.rs` instead; `cache.rs` `KillSwitchCache` (the in-memory active-*engaged*-only
set — see decision 4), `RefreshDelta`/`refresh` (the diff), `blocking_scope` (ingest's check),
`ChannelExclusion`/`exclusion_for_channel` (the dispatcher's claim-exclusion builder, a free
function so it can fold together the cache's engaged set and the dispatcher's own draining map —
decision 4), and `run_refresh_loop` (the poll/`LISTEN` loop shared by both callers). Register
`pub mod kill_switch;` in `src/lib.rs`.

#### Task 3 — dispatcher integration
`src/dispatcher/repo.rs`: `claim` gains the `exclusion: &ChannelExclusion` parameter (decision 3);
new `claim_for_scope` (decision 4/11's inverse-of-exclusion query, via `sqlx::QueryBuilder` since
it branches on scope kind and an optional channel pin). `src/dispatcher/worker.rs`:
`DispatcherContext` gains `kill_switches: Arc<KillSwitchCache>` and `draining:
Arc<RwLock<HashMap<Uuid, KillSwitch>>>`, plus a `claim_exclusion` method folding both;
`run_channel_loop` consults it before calling `repo::claim`; `process_one` becomes `pub(crate)`
so the new `drain` module can reuse it. New `src/dispatcher/drain.rs` (not in the original file
list — needed once decision 4 was revised): `drain_released_scope` (one channel's ramp),
`run_release_drain` (fans a released switch out to every channel this process runs, then clears
`draining`), and `discard_engaged_scope` (decision 11). Registered as `pub mod drain;` in
`src/dispatcher/mod.rs`. `src/bin/dispatcher.rs`: build the shared cache, `draining` map, and
per-channel `DispatcherContext`s (as a `HashMap` keyed by channel, so the refresh task can hand
it to `run_release_drain`); load `tenant_config.kill_switch_release_rate` once at startup; spawn
the refresh task (dedicated `LISTEN kill_switch` connection) with a closure that spawns
`discard_engaged_scope`/`run_release_drain` off `RefreshDelta`; spawn each channel's
`run_channel_loop`.

#### Task 4 — `tenant_config.kill_switch_release_rate`
New `migrations/tenant/0010_tenant_config_kill_switch_release_rate.sql` (a fresh migration, not
an edit to the shipped `0002_tenant_config.sql` — T-022 already has its own, unrelated edits
queued against that file, and this is a new column, not a defect correction) doing `ALTER TABLE
tenant_config ADD COLUMN kill_switch_release_rate int NOT NULL DEFAULT 500`.
`src/tenant_config/{model.rs,repo.rs,configure.rs}` and `src/bin/control.rs`'s
`tenant-config-set` gain the field/flag (decision 5).

#### Task 5 — ingest-side enforcement
`src/tenant/registry.rs::TenantContext`/`get_or_open`: add `kill_switches` field and spawn its
poll (decision 6). `src/ingest/handler.rs::create_comms`: check it first. `src/ingest/model.rs`:
add `IngestError::KillSwitchEngaged` + `Display`/`IntoResponse` arms.

#### Task 6 — tenant status enforcement
`src/producer/cert_repo.rs::find_producer_cert`: join `tenant`, return `status`.
`src/producer/resolve.rs`: add `ResolutionError::TenantNotActive`, check `status ==
tenant::model::status::ACTIVE`. `src/ingest/model.rs`: add `IngestError::TenantNotActive` +
`From` mapping + `Display`/`IntoResponse` arms (decision 7).

#### Task 7 — `auth_enabled` schema
`migrations/control/0005_tenant_auth_enabled.sql` (decision 8). No Rust changes.

#### Task 8 — docs
New `docs/user-manual/kill-switches.adoc` (decision 8's runbook lives here too): the five
scopes, `on_queued`, the exact `psql` commands to engage/release a `kill_switch` row and to flip
`auth_enabled` (with its `platform_audit` insert), what "held" means operationally, and the
release-ramp behaviour. Register it in `docs/user-manual.adoc`'s include list. Add one sentence
each to `docs/user-manual/dispatcher.adoc` (kill-switch enforcement now exists) and
`docs/user-manual/ingest.adoc` (the two new rejection codes).

### Acceptance test

`just build`, `just lint`, `just test` — including a new `tests/kill_switch.rs`
(`tests/dispatcher.rs`/`tests/ingest.rs` conventions: real provisioning against the local
compose stack, `wiremock` for the provider) exercising:

- `engaged_kill_switch_rejects_the_matching_producer_but_not_another`: engaging a `producer`
  switch → `POST /comms` for that producer eventually (polling for the ingest-side cache to
  refresh) returns `503`; a different producer on the same tenant still gets `201`.
- `engaged_producer_switch_excludes_only_that_producers_rows`: a message already in `outbox`
  under an engaged `producer` switch is not claimed (`repo::claim` with the cache's exclusion
  applied), while an unrelated producer's row still is.
- `global_switch_excludes_every_channel`: a `global` switch's exclusion blocks claiming on every
  channel a dispatcher process might run, not just one.
- `release_ramp_admits_at_most_release_rate_rows_per_batch`: `claim_for_scope` with a 5-row
  backlog and `limit = 2` returns exactly 2, leaving 3 unleased for the next tick.
- `drain_sends_every_row_and_marks_an_already_expired_one_expired_instead`: running
  `drain_released_scope` to completion against a real `wiremock` provider sends every non-expired
  row and writes `expired` (never sends) for one whose `expires_at` had already passed.
- `release_immediately_stops_blocking_new_ingest_even_before_drain_finishes`: `blocking_scope`
  returns `None` the refresh cycle right after release, independent of any drain still running.
- `tests/producer.rs::resolve_producer_rejects_a_suspended_tenant` and
  `tests/ingest.rs::suspended_tenant_producer_is_rejected`: a `status = 'suspended'` tenant's
  still-`enabled` producer cert resolves to `TenantNotActive` / gets `403` from `POST /comms`.

`just docs-check` clean.

### Docs update (mandatory when user-facing)

New `docs/user-manual/kill-switches.adoc`, registered in `docs/user-manual.adoc`; short additions
to `dispatcher.adoc` and `ingest.adoc` (Task 8).

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. Docs updated and registered (Task 8).
3. Write a summary: files touched, decisions made, anything deferred (`auth_enabled`'s
   read/fail-open side, explicitly, to the step-17 OTP ticket).
4. Suggested commit message, ticket id in brackets, e.g.:
   ```
   feat(kill-switch): add five-scope kill switches, tenant-status and auth_enabled schema (T-016)
   ```
5. Tidy WIP commits into a small number of atomic commits (root-path child).
6. Commit locally on `feat/T-016-kill-switches`. Present the commit message; do not push or open
   an MR without explicit user approval. Verify `origin/main...HEAD` carries no `tickets/` path
   before pushing (`layout = "in-tree"`, rules §0). Hand back to the user.

## Review

Reviewer independence (step 0): **independent** — this session had no hand in `feat/T-016-kill-switches` (started fresh, `/clear`d, no memory of authoring the branch). Audits run directly, not delegated.

Checklist:
- [x] Reviewer independence settled: independent (see above)
- [x] Implementation audit — acceptance test re-run (`just build`, `just lint`, `just test`, `just docs-check`), all green; every task and confirmed decision checked against the actual diff (steps 1, 2)
- [x] Quality audit (step 3) — idiomatic, DESIGN.md-cited throughout; one gap found, see F1
- [x] Consistency audit (step 4) — no stale `§N` cross-references found; `tenant_id`-in-tenant-db-table convention respected (no new column); DESIGN.md corrections (decisions 28/29, kill_switch NULL-key fix, Still Open #9) already landed on `main` during refinement, not deferred
- [x] Documentation audit (step 4a) — `kill-switches.adoc` registered in `user-manual.adoc`; `dispatcher.adoc`/`ingest.adoc` updated; `just docs-check` clean
- [x] Docs-readability pass (step 4b) — conscious skip: no docs-readability reviewer available in this session
- [x] Findings recorded below with severity, class, disposition; cost line present (step 5)

### Implementation audit (step 2)

Re-ran verbatim: `just build` (clean), `just lint` (`cargo clippy -- -D warnings`, clean), `just test` (all 12 integration-test binaries green, including the new `tests/kill_switch.rs`'s 6 tests and the two new suspended-tenant tests in `tests/producer.rs`/`tests/ingest.rs`), `just docs-check` (clean). All 11 confirmed decisions and all 8 tasks are implemented where the plan says: `kill_switch` schema + corrected unique index + notify trigger (Task 1), the cache module (Task 2), dispatcher claim-exclusion + `claim_for_scope` (Task 3), `kill_switch_release_rate` end-to-end through the CLI (Task 4), ingest-side rejection (Task 5), tenant-status enforcement closing T-011/F3 (Task 6), `auth_enabled` schema-only (Task 7), docs (Task 8). Auth/OTP path confirmed untouched: `validate_class` in `src/ingest/handler.rs` rejects `class = "auth"` outright, so the new kill-switch/tenant-status checks in `create_comms` never see auth traffic (AGENTS.md hard invariant 1 intact).

### Findings

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | A DB error mid-sweep in either kill-switch drain task is treated as if the sweep finished, permanently losing the safety guarantee for whatever backlog remained | `src/dispatcher/drain.rs:44-59` (`drain_released_scope` logs and `return`s on a `claim_for_scope` error, ending the task); `src/dispatcher/drain.rs:112-119` (`run_release_drain` awaits every per-channel handle and unconditionally removes the scope from `draining` once they all return, whether by completion or by this early error return — nothing distinguishes the two); `src/dispatcher/drain.rs:134-168` (`discard_engaged_scope` does the same on a `claim_for_scope` error, and also on a per-row `write_terminal` error inside its loop, which it just logs and continues past). No test exercises a DB error mid-drain or mid-discard. | Once the scope drops out of `draining`/the engaged set, the normal claim loop (which no longer excludes it) claims whatever backlog remains at full, unthrottled speed — for a `hold` switch this is exactly the stampede DESIGN.md decision 12 exists to prevent ("Release is the dangerous half... 500k held messages become dispatchable in the same instant"); for a `discard` switch, the un-swept remainder is never terminal-written `discarded` — it sits excluded until the switch eventually releases, then gets dispatched and sent for real, the opposite of what engaging `on_queued = 'discard'` asked for. Contrast with the pre-existing `run_channel_loop`'s claim call (`src/dispatcher/worker.rs`), which `.expect()`s and crashes the process on the same class of error, forcing a full state rebuild on restart — these two new tasks should not silently swallow the same failure and call it done. Fix: on a `claim_for_scope`/`write_terminal` error, retry (with backoff) rather than returning, or propagate the failure so the caller does not clear `draining`/consider the scope finished until the backlog is actually confirmed empty. |

Disposition summary: 1 blocking (F1), 0 non-blocking.

cost: estimated L, actual L

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 4 (§14), filed after T-014 (step 2 work) landed.
- 2026-09-02 — scope widened (TO DO). source: audit: design/implementation audit folded in five findings touching this ticket's own schema/mechanism: kill_switch's NULL-key uniqueness bug, the held-state spin risk, ingest-side kill-switch propagation (LISTEN doesn't work behind PgBouncer), auth_enabled's fail-open correction, and T-011/F3's unchecked tenant.status.
- 2026-09-02 — re-graded medium/M → high/L during refinement: dispatcher, ingest, and two migrations all touched, plus a new release-drain task. auth_enabled scoped to schema+audit only (otp-api, its only reader, is step 17); DESIGN.md Still Open #9 resolved for this step (decision 29).
- 2026-09-02 — TO DO → READY: plan complete
- 2026-09-02 — READY → IN DEVELOPMENT: picked up
- 2026-09-02 — plan amended inline: decision 4's release mechanism as refined (bump
  `next_attempt_at`, let the normal claim loop re-admit the row) does not ramp anything — a
  released scope's rows already have `next_attempt_at` in the past, so the instant the shared
  cache stops excluding the scope, the normal claim loop sees the whole backlog at once
  regardless of any bump. Fixed during implementation: the dispatcher keeps a process-local
  `draining` map (populated from `KillSwitchCache::refresh`'s diff) that keeps excluding a
  released scope from the normal claim loop until a dedicated `drain_released_scope` task —
  which claims its own rate-limited batches directly via a new `claim_for_scope` query — empties
  it. Also added decision 11: the plan never specified a mechanism for `on_queued = 'discard'`
  at all, despite the ticket's own Outcome/Description promising it; `discard_engaged_scope`
  (same `claim_for_scope`, no pacing) closes that gap. `messgr-ingest`'s side (decision 6) is
  unaffected — it only ever reads the shared cache's *engaged* set, so a release still unblocks
  new sends immediately regardless of how long the dispatcher's drain takes.
- 2026-09-02 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-02 — IN REVIEW → REWORK: F1 blocking: kill-switch drain/discard tasks silently treat a mid-sweep DB error as completion, losing the release-ramp/discard guarantee
