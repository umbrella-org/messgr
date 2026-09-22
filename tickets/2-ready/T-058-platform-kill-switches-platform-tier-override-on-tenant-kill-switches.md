---
id: T-058
title: Platform kill switches: platform-tier override on tenant kill switches
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: medium
cost: L
---

# T-058 — Platform kill switches: platform-tier override on tenant kill switches

## Outcome

After this ships, an abusive or non-paying tenant can be suspended by the platform operator
without touching its data: a platform-tier kill switch overrides that tenant's own switch, and
the tenant's own admin panel shows the suspension as a distinct, non-actionable state rather than
looking like the tenant flipped its own switch off.

## Description

**`platform_kill_switch` already exists as a table** — `migrations/control/0001_control_schema.sql`
(T-001, done) already shipped the schema: `id, scope ('platform'|'tenant'), tenant_id (nullable),
engaged_by, engaged_at, reason, released_by, released_at`. No application code references it yet
(`grep -rn platform_kill_switch src/` is empty) — this ticket is entirely new model/repo/cache/
wiring code against an already-shipped table, no new migration needed for the base shape.

Deferred by T-016 ("Cloud-only, unread until step 19"; T-016 is done and merged, PR #19,
`6059a13`, and this ticket reuses its `KillSwitch`/`KillSwitchCache` shape — `src/kill_switch/` —
rather than inventing a second one).

**Two-tier: a platform switch overrides a tenant's switch, never the reverse (§5, `04-gate-chain.md`).**
A `scope = 'platform'` row (tenant_id `NULL`) blocks every tenant in the region; a
`scope = 'tenant'` row (tenant_id set) blocks one tenant. Either kind, while active, forces that
tenant's dispatcher/ingest exclusion to "blocked entirely" regardless of what that tenant's own
`kill_switch` table says — a tenant cannot release a platform switch.

**Propagation, per the design doc's explicit mechanism:** "Propagation still uses `NOTIFY`, but
the control plane must fan out to each tenant database rather than issuing one notification, so
the dispatcher's periodic re-read (30s) is the guaranteed path and `NOTIFY` is the fast path."
Concretely: engaging/releasing a platform switch in the control database also issues a bare
`NOTIFY kill_switch` on each *affected tenant's own database* (the same channel name
`messgr-dispatcher` and `messgr-ingest` already `LISTEN` on for their own tenant's `kill_switch`
table, per `src/kill_switch/cache.rs`'s doc comment) — a tenant-scope switch touches one tenant
database, a platform-scope (region-wide) switch touches every currently-registered tenant's
database. This is deliberately the *existing* channel, not a second one dispatcher/ingest must
additionally subscribe to — the fan-out is bounded, rare (an admin action, not a hot path), and
keeps every consumer's connection topology unchanged. The 30s guaranteed path is a *new* read on
top of that: the dispatcher/ingest refresh tick additionally re-reads `platform_kill_switch` from
`control_pool` (which both already hold) on every tick, independent of whether any NOTIFY fired.

Tenant admin panel (T-049, done) must show a platform suspension as a distinct, non-actionable
state — a tenant operator must not be able to mistake "platform suspended us" for "our own switch
is on" or be able to flip it back themselves.

Soft coupling: displayed as one pane of the platform console (T-057) — independently buildable,
no hard `depends-on:`.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-058-platform-kill-switches-platform-tier-override-on-tenant-kill-switches
```

### Prerequisite gate (hard)

None. T-016 (`src/kill_switch/`, tenant-level kill switches) is done and merged (PR #19,
`6059a13`) and the `platform_kill_switch` table already exists (T-001). T-057 (console) has not
landed — this ticket's console pane task is deferred to T-057's stub (Description soft coupling).

### Confirmed design decisions (do not deviate without asking)

1. **Platform-scope exclusion is modelled as an unconditional `blocked_entirely`, reusing
   `kill_switch::cache::ChannelExclusion` rather than inventing a parallel exclusion type.** A
   platform switch (either `scope='platform'` or `scope='tenant'` matching this process's own
   tenant) behaves, for exclusion purposes, exactly like a `scope::GLOBAL` tenant kill switch —
   nothing on any channel is claimable. Do not build a second, richer platform-scope exclusion
   model; the two-tier override design (§5) needs "blocks everything," not per-channel/per-
   producer platform switches.
2. **No new NOTIFY channel.** Fan-out writes into each affected tenant database's existing
   `kill_switch` NOTIFY channel (Description) — `messgr-dispatcher`/`messgr-ingest`'s existing
   `LISTEN kill_switch` on their own tenant database picks it up with zero change to their
   connection topology. Do not add a second `PgListener` on `control_pool` to either binary.
3. **The 30s guaranteed re-read polls `control_pool`, which both `messgr-dispatcher` and
   `messgr-ingest` already hold.** Add it to the same tick that already refreshes
   `kill_switch::cache::KillSwitchCache` (`src/bin/dispatcher.rs`'s existing refresh loop,
   `src/kill_switch/cache.rs::run_refresh_loop`), not a second independent timer.
4. **A tenant cannot release a platform switch.** `kill_switch::configure::release`'s existing
   signature (or its platform-scope equivalent) must reject a release attempt made through the
   tenant admin panel's own auth realm — enforced by simply never exposing a release action for
   platform-scope rows in T-049's admin panel, and only exposing it in T-057's platform console
   (operator-only auth realm). No new authorization code needed if the tenant-facing UI/API
   surface never offers the button/route at all.

### Tasks

#### Task 1 — `src/platform_kill_switch/` module (model + repo + configure)
Mirror `src/kill_switch/{model,repo,configure}.rs` exactly, against the control database's
`platform_kill_switch` table: `model.rs` (`PlatformKillSwitch` struct — `scope::PLATFORM`,
`scope::TENANT` constants; `is_active()`), `repo.rs` (`list_active(pool)`,
`list_active_for_tenant(pool, tenant_id)` — platform-scope rows OR tenant-scope rows matching
`tenant_id`), `configure.rs` (`engage`/`release`, same `ConfigureError` shape as
`kill_switch::configure`).

#### Task 2 — Fan-out NOTIFY on engage/release
In `platform_kill_switch::configure::engage`/`release`, after the control-database write:
resolve the affected tenant id(s) (one, for `scope='tenant'`; every row from
`tenant_repo::list` with `status = active`, for `scope='platform'`), and for each, open a
short-lived connection via `tenant::pool::connect_tenant_pool` and execute `NOTIFY kill_switch`
against it, then close it. This is an admin action (rare, not a hot path) — a bounded loop over
at most a few dozen tenants is an acceptable cost; do not build a persistent connection pool for
this.

#### Task 3 — Dispatcher wiring (`src/bin/dispatcher.rs`, `src/dispatcher/worker.rs`)
Add a `platform_kill_switches: Arc<PlatformKillSwitchCache>` (mirrors `KillSwitchCache`'s own
shape from task 1's model, keeping the two caches structurally parallel) to dispatcher's app
state, refreshed on the same tick as `KillSwitchCache` (decision 3), filtered to this
dispatcher's own `tenant.id` (already resolved at startup, `src/bin/dispatcher.rs:117`). In
`worker.rs`'s exclusion-building step (`exclusion_for_channel`, `src/kill_switch/cache.rs:118`),
OR in an unconditional `blocked_entirely = true` when the platform cache reports any active row
for this tenant (decision 1) — before folding in the tenant-level exclusion, so a platform
suspension is never weaker than a tenant's own global switch.

#### Task 4 — Ingest wiring (`src/bin/ingest.rs`, wherever `KillSwitchCache::blocking_scope` is
called for the per-request check)
Same shape: add the platform cache, check it first (cheapest-first ordering, matching the
`auth_flag` precedent in `sms_sender::handler::send_otp`), reject with the same "blocked" error
shape `messgr-ingest` already returns for a tenant-level switch, distinguishing the reported
scope as `"platform"` in the response/log so an operator can tell the two apart.

#### Task 5 — `messgr-control` CLI: engage/release subcommands
`Command::EngagePlatformKillSwitch { scope: PlatformScope, tenant_slug: Option<String>, reason:
String, actor: String }` / `Command::ReleasePlatformKillSwitch { id: Uuid, actor: String }` in
`src/bin/control.rs`, calling task 1's `configure::engage`/`release`. This is the only way to
engage a platform switch until T-057's console pane lands (soft coupling).

#### Task 6 — Tenant admin panel: distinct, non-actionable display (`src/query_api/admin.rs`,
`templates/admin/kill_switches.html`)
`ui_kill_switches` additionally reads `platform_kill_switch::repo::list_active_for_tenant` and
renders any active row in a visually distinct, non-interactive block (no engage/release form) —
labelled as a platform suspension, not a tenant-engaged switch.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including:
   - `platform_kill_switch::model`: a `scope='platform'` row and a `scope='tenant'` row matching
     this tenant both `matches()` (or equivalent) as blocking; a `scope='tenant'` row for a
     *different* tenant does not.
   - Dispatcher exclusion: with a platform switch active and the tenant's own `kill_switch` table
     empty, `exclusion_for_channel` still reports `blocked_entirely = true` (mutation test: assert
     this fails red if the platform-check branch is deleted).
   - Fan-out: engaging a `scope='tenant'` switch results in a `NOTIFY` observable on that tenant's
     database connection (a `PgListener` in the test subscribes and asserts it fires); engaging
     `scope='platform'` fires it on every registered tenant's database in the test fixture.
3. Manual: engage a platform switch via the new CLI subcommand against the dev compose stack,
   confirm `messgr-dispatcher` stops claiming for that tenant within one 30s tick even with
   `NOTIFY` disabled (simulating the guaranteed-path-only case), and that the tenant admin panel
   shows it as non-actionable.

### Docs update (mandatory when user-facing)

Update `docs/user-manual/kill-switches.adoc` with a new section on platform-tier switches: the
two-tier override rule, the new CLI subcommands, and what the tenant admin panel shows. Run
`just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated and registered.
3. Write a summary (files touched, decisions made, anything deferred — note T-057's console pane
   is still a stub until that ticket lands) and hand back for review.
4. Suggested commit message: `feat(kill-switch): platform-tier override on tenant kill switches
   (T-058)`.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
- 2026-09-22 — TO DO → READY: plan complete: platform_kill_switch table already existed unused (T-001); re-graded cost M to L given dispatcher+ingest+CLI+two console-panel wiring points; fan-out reuses the existing per-tenant kill_switch NOTIFY channel rather than adding a second one
