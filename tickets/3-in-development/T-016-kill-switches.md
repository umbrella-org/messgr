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
4. **Release ramps via a one-shot background drain task, not a partially-excluded cache.** When
   the cache observes a scope's `released_at` flip from NULL to non-NULL, it spawns a task that
   walks that scope's still-held `outbox` rows in claim order
   (`ORDER BY priority, next_attempt_at`), in batches of `tenant_config.kill_switch_release_rate`
   rows per second: a row whose `expires_at` has passed gets the existing `expired` terminal
   write (`repo::write_terminal`, reusing T-013's `comms_event`/`final_status` path — no new
   terminal-state plumbing); every other row gets `next_attempt_at = now()` so it re-enters the
   normal claim path unassisted. No new `outbox` column — `next_attempt_at`/`expires_at` already
   exist (T-009).
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

### Tasks

#### Task 1 — `kill_switch` schema
`migrations/tenant/0009_kill_switch.sql`: the `CREATE TABLE kill_switch` + corrected unique
index from DESIGN.md §4.9, plus the `kill_switch_notify` trigger (decision 1).

#### Task 2 — kill-switch cache module
New `src/kill_switch/{mod.rs,model.rs,repo.rs,cache.rs}` (mirrors `partition_lifecycle`'s
layout): `model.rs` the `KillSwitch` row + `scope`/`on_queued` constants (matching
`tenant::model::status`'s `&'static str` convention); `repo.rs` `list_active`,
`find_released_since` (for the drain trigger); `cache.rs` `KillSwitchCache` — the in-memory
active-scope set, a method to test `(channel, producer_id, campaign_id)` against it, and the
poll/refresh loop shared by both callers (dispatcher passes a `PgListener`, ingest does not —
same struct, an `Option` for the listen half). Register `pub mod kill_switch;` in `src/lib.rs`.

#### Task 3 — dispatcher integration
`src/dispatcher/repo.rs::claim`: candidate-set exclusion (decision 3). `src/dispatcher/worker.rs`:
`DispatcherContext` gains `kill_switches: Arc<KillSwitchCache>`; `run_channel_loop` consults it
before calling `repo::claim`. `src/bin/dispatcher.rs`: construct the shared cache once, spawn its
refresh task, spawn the release-drain task (decision 4), thread the `Arc` into every
`DispatcherContext`.

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

- Engaging a `producer` switch → `POST /comms` for that producer returns `503` with a
  `kill_switch_engaged`-shaped body; a different producer on the same tenant still succeeds.
- A message already in `outbox` under an engaged `producer`/`channel`/`global` switch is not
  claimed by a running dispatcher (assert the row is still present and `leased_until IS NULL`
  after several claim-loop ticks).
- Releasing the switch drains the backlog at `kill_switch_release_rate`: immediately after
  release, at most one rate-sized batch is claimable; the rest remain held until the next tick.
- A held row whose `expires_at` has passed is written `expired` on drain, never sent.
- A `global` switch blocks claiming on every channel, not just one.
- A tenant with `status = 'suspended'` gets `403` from `POST /comms` even with an `enabled`
  producer cert; `status = 'active'` still succeeds.

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

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 4 (§14), filed after T-014 (step 2 work) landed.
- 2026-09-02 — scope widened (TO DO). source: audit: design/implementation audit folded in five findings touching this ticket's own schema/mechanism: kill_switch's NULL-key uniqueness bug, the held-state spin risk, ingest-side kill-switch propagation (LISTEN doesn't work behind PgBouncer), auth_enabled's fail-open correction, and T-011/F3's unchecked tenant.status.
- 2026-09-02 — re-graded medium/M → high/L during refinement: dispatcher, ingest, and two migrations all touched, plus a new release-drain task. auth_enabled scoped to schema+audit only (otp-api, its only reader, is step 17); DESIGN.md Still Open #9 resolved for this step (decision 29).
- 2026-09-02 — TO DO → READY: plan complete
- 2026-09-02 — READY → IN DEVELOPMENT: picked up
