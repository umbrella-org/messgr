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
(T-001, done) shipped the columns: `id, scope ('platform'|'tenant'), tenant_id (nullable),
engaged_by, engaged_at, reason, released_by, released_at`. It carries **no constraints at all**
(no CHECK on `scope`, no scope/tenant_id pairing check, no FK, no "one live switch per scope"
unique index), so this ticket adds one small constraints migration on top. No application code
references the table yet.

Deferred by T-016 ("Cloud-only, unread until step 19"; T-016 merged, PR #19, `6059a13`). This
ticket reuses T-016's `KillSwitch`/`KillSwitchCache` machinery in `src/kill_switch/` rather than
inventing a second one.

**Two-tier: a platform switch overrides a tenant's switch, never the reverse (§5.2,
`04-gate-chain.md`).** A `scope = 'platform'` row (tenant_id `NULL`) blocks every tenant in the
region; a `scope = 'tenant'` row (tenant_id set) blocks one tenant. Either kind, while active,
forces that tenant's dispatch and ingest to "blocked entirely", regardless of what the tenant's
own `kill_switch` table says, and a tenant cannot release it. **Release ramps exactly as a tenant
switch's release does** (§5.2 "Release is the dangerous half", DESIGN decision 12) — a
suspension lifted after days must not dump its whole backlog on the provider in one instant.

**Propagation, per the design doc:** "the control plane must fan out to each tenant database
rather than issuing one notification, so the dispatcher's periodic re-read (30s) is the
guaranteed path and `NOTIFY` is the fast path." Engaging/releasing a platform switch issues a bare
`NOTIFY kill_switch` on each affected tenant's own database — the channel `messgr-dispatcher`
already `LISTEN`s on (direct connection, §2.3). `messgr-ingest` does **not** `LISTEN` (it sits
behind PgBouncer transaction pooling, §5.2's "ingest-side rejection" correction); it has only its
5-second per-tenant poll, and that poll additionally re-reads `platform_kill_switch` from the
control database. Both refreshes read the platform table **in the same iteration** as the tenant
`kill_switch` table, so a fan-out `NOTIFY` wakes the dispatcher into a refresh that actually sees
the platform row.

**Relationship to T-057's tenant suspend (user decision, applicability gate A2).** T-057's
console action `POST /tenants/{id}/suspend` sets `tenant.status = 'suspended'`, which blocks ingest
and the OTP paths via `resolve_producer` but not the dispatcher — a suspended tenant's queued
backlog kept sending. Suspend now *also* engages a `scope = 'tenant'` platform switch in the same
transaction, so one operator action stops ingest, OTP and dispatch together. A tenant-scope
platform switch can still be engaged on its own (without suspending) for an incident that must
not stop OTP.

**Auth/OTP is untouched (hard invariant 1).** A platform switch never affects `messgr-otp` or
`messgr-sms-sender`; the check must not be placed in `resolve_producer`/`find_producer_cert`,
which those paths share. Only `tenant.status = 'suspended'` (T-057, above) stops OTP.

Tenant admin panel (T-049) shows an active platform switch as a distinct, non-actionable state
with a **generic label** ("Suspended by the platform operator — contact support" plus the
engage time); the operator's free-text reason stays in the platform console (user decision).

The platform console's `kill_switches.rs` pane (T-057 stub) is wired in this ticket.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-058-platform-kill-switches-platform-tier-override-on-tenant-kill-switches
```

### Prerequisite gate (hard)

None. T-016 (`src/kill_switch/`) and T-057 (platform console, `src/platform_console/`) are done
and merged; the `platform_kill_switch` table exists (T-001).

### Confirmed design decisions (do not deviate without asking)

1. **A platform switch is represented, inside `KillSwitchCache`, as a synthetic `KillSwitch`
   with `scope = global`, `on_queued = hold` and the platform row's own `id`.** Everything
   downstream — `exclusion_for_channel`'s `blocked_entirely`, `RefreshDelta`'s
   `newly_engaged`/`released`, the dispatcher's `draining` map and `run_release_drain` ramp, the
   `expires_at` drop during drain — then works unchanged. The cache additionally remembers which
   ids are platform-tier so `blocking_scope` can report `"platform"` rather than `"global"`.
   No per-channel/per-producer platform scopes.
2. **No new NOTIFY channel.** Fan-out writes `NOTIFY kill_switch` into each affected tenant
   database; no second `PgListener` on `control_pool`.
3. **The platform re-read lives inside `KillSwitchCache`'s refresh**, which gains an optional
   `(control_pool, tenant_id)` source; `run_refresh_loop` passes it through. A failed control read
   fails the whole refresh (the previous snapshot is kept — never "platform switch vanished
   because the control DB blipped").
4. **A tenant cannot release a platform switch** — the tenant admin panel never offers the
   action; engage/release is exposed only in the platform console (operator realm) and the
   `messgr-control` CLI.
5. **Release drain respects every still-engaged switch** (user decision: fold the pre-existing
   T-016 bug in here). `claim_for_scope` takes the current engaged-set exclusion for its channel;
   `drain_released_scope` passes the exclusion built from `kill_switches.active_snapshot()` (the
   *engaged* set only — not other draining scopes, so two overlapping drains cannot starve each
   other). A platform release therefore never sends a campaign the tenant is still holding, and a
   tenant release never sends through a still-active platform switch. The discard-at-engage task
   passes an empty exclusion (discarding never sends; unchanged behaviour).
6. **Engage is idempotent-by-constraint:** a partial unique index allows one live switch per
   `(scope, tenant_id)`; `engage` uses `INSERT … ON CONFLICT DO NOTHING` so a duplicate maps to
   `AlreadyEngaged` without aborting a surrounding transaction (T-057's suspend calls it inside
   its own).
7. **Every engage/release/rejection is audited** in `platform_audit` via `record_tx`, in the same
   transaction as the control write (T-057 F1's rule). Actions: `platform_kill_switch.engage`,
   `platform_kill_switch.release`.
8. **Fan-out is after commit and best-effort.** Targets: the one tenant (tenant scope) or every
   tenant whose database still exists — `status IN ('active','suspended','offboarding_archive')`,
   never `provisioning`/`offboarding_destroy` (platform scope). One short-lived
   `connect_tenant_pool(..., max_connections = 1)` per tenant; a per-tenant failure is logged and
   counted, never fails the engage — the 30s/5s polls are the guaranteed path.

### Tasks

#### Task 1 — Constraints migration
`migrations/control/0006_platform_kill_switch_constraints.sql`: `CHECK (scope IN
('platform','tenant'))`; `CHECK ((scope = 'platform') = (tenant_id IS NULL))`; FK `tenant_id →
tenant(id)` (tenant rows are never deleted — offboarding only changes `status`); partial unique
index `platform_kill_switch_live_idx ON (scope, COALESCE(tenant_id,
'00000000-0000-0000-0000-000000000000'::uuid)) WHERE released_at IS NULL`.

#### Task 2 — `src/platform_kill_switch/` module
`model.rs` (`PlatformKillSwitch`, `scope::{PLATFORM, TENANT}`, `is_active`, `applies_to(tenant_id)`,
`as_kill_switch()` → decision 1's synthetic row); `repo.rs` (`list_active`,
`list_active_for_tenant`); `configure.rs` (`engage`/`engage_tx`/`release`, `ConfigureError`
mirroring `kill_switch::configure` incl. `AlreadyEngaged` and `UnknownTenant`, audited per
decision 7, fan-out per decision 8 after commit — `engage_tx` returns the fan-out targets for its
caller to notify once *it* commits).

#### Task 3 — Cache + refresh loop (`src/kill_switch/cache.rs`)
`KillSwitchCache::refresh_with_platform(pool, Option<(&PgPool, Uuid)>)` merges synthetic rows
into the active set and records platform ids; `refresh(pool)` stays as the tenant-only form.
`blocking_scope` checks platform ids first (cheapest-first) and reports `"platform"`.
`run_refresh_loop` gains `platform: Option<(PgPool, Uuid)>`.

#### Task 4 — Dispatcher (`src/bin/dispatcher.rs`) + drain fix (`src/dispatcher/{repo,drain}.rs`)
Pass `Some((control_pool.clone(), tenant.id))` to `run_refresh_loop`. `claim_for_scope` gains
`exclusion: &ChannelExclusion`, applied as `claim` applies it; `drain_released_scope` builds it
from `ctx.kill_switches.active_snapshot()` each batch (decision 5); `discard_engaged_scope` passes
the default.

#### Task 5 — Ingest (`src/tenant/registry.rs`)
`get_or_open`'s spawned poll passes `Some((control_pool.clone(), tenant_id))`. The existing check
in `src/ingest/handler.rs` then rejects with `IngestError::KillSwitchEngaged { scope: "platform" }`
(503) — no handler change beyond that.

#### Task 6 — T-057 suspend engages a tenant-scope switch (`src/platform_console/tenants.rs`)
Inside `suspend`'s existing transaction, call `platform_kill_switch::configure::engage_tx` with
`scope = tenant`, reason `"tenant suspended"`; `AlreadyEngaged` is fine; fan out after commit.

#### Task 7 — `messgr-control platform-kill-switch {engage,release,list}` (`src/bin/control.rs`)
`engage --scope platform|tenant [--tenant-slug] --reason --actor`, `release --id --actor`, `list`.

#### Task 8 — Platform console pane (`src/platform_console/kill_switches.rs`, new
`templates/platform_console/kill_switches.html`)
Replace the stub: active switches (scope, tenant slug, engaged by/at, reason) with a release
button each; an engage form (scope, tenant select, mandatory reason). Routes
`POST /platform-kill-switches` and `POST /platform-kill-switches/{id}/release`, `role::OPERATOR`
only, following `tenants::suspend`'s shape.

#### Task 9 — Tenant admin panel (`src/query_api/admin.rs`, `templates/admin/kill_switches.html`)
`kill_switches_page` (shared by the view, engage and release handlers) additionally reads
`platform_kill_switch::repo::list_active_for_tenant(&state.control_pool, tenant.tenant_id)` and
renders a distinct, non-interactive block with the generic label — no form, no release.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including a new `tests/platform_kill_switch.rs` (platform-scope tests
   serialized, each with a drop-guard that releases its switch so a panic cannot poison later
   tests; assertions scoped to the fixture's own tenants):
   - model: `scope='platform'` and matching `scope='tenant'` rows apply; another tenant's does not.
   - constraints: a second live switch for the same `(scope, tenant)` → `AlreadyEngaged`; a
     `scope='platform'` row with a tenant id is rejected by the CHECK.
   - cache: with the tenant's own `kill_switch` table empty and a platform switch active,
     `claim_exclusion` reports `blocked_entirely = true` and `blocking_scope` reports
     `"platform"`; after release the refresh delta lists it under `released` (drives the ramp).
     Mutation check: deleting the platform merge in `refresh_with_platform` turns this red.
   - drain fix: releasing a tenant `global` switch while a `campaign` switch is still engaged
     drains everything *except* that campaign's rows.
   - fan-out: engaging `scope='tenant'` fires `NOTIFY kill_switch` observable by a `PgListener`
     on that tenant's database; `scope='platform'` fires it on each fixture tenant's database.
   - suspend: the console suspend route leaves an active `scope='tenant'` switch behind.
   - admin panel: the tenant kill-switch page renders the generic platform-suspension label and
     no release control for it.
3. Manual (where a compose stack is available): engage via the CLI, confirm
   `messgr-dispatcher` stops claiming within one 30s tick, and the tenant panel shows it.

### Docs update (mandatory when user-facing)

`docs/user-manual/kill-switches.adoc`: new "Platform-tier switches" section (two-tier rule,
release ramp, OTP unaffected, suspend relationship, what the tenant panel shows).
`docs/user-manual/control-plane-cli.adoc`: the `platform-kill-switch` subcommands and the console
pane. `development/design/03-data-model.md:518` (and the schema file comment): drop "unread until
step 19". Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated.
3. Summary (files touched, decisions, anything deferred) and hand back for review.
4. Suggested commit message: `feat(kill-switch): platform-tier override on tenant kill switches
   (T-058)`.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
- 2026-09-22 — TO DO → READY: plan complete: platform_kill_switch table already existed unused (T-001); re-graded cost M to L given dispatcher+ingest+CLI+two console-panel wiring points; fan-out reuses the existing per-tenant kill_switch NOTIFY channel rather than adding a second one
- 2026-09-23 — impact sweep from T-057's re-review: T-057 landed, so "T-057 has not landed" was
  stale — corrected the Prerequisite gate, Task 5, and Finish step 3 to name the real stub
  (`src/platform_console/kill_switches.rs`) and flagged that neither T-057's task list nor this
  one's wires a real view into it; whoever refines this ticket further should add that task
- 2026-09-25 — plan amended inline (applicability gate, still READY): blocking A1 — release must ramp, so platform switches are merged into KillSwitchCache as synthetic global/hold rows reusing the drain machinery; blocking A2 — user decided T-057 suspend also engages a tenant-scope switch; user decided generic tenant-facing label and folding the T-016 drain-ignores-engaged-switches bug (A15) into this ticket; inline fixes A3–A14 (ingest polls not LISTENs, constraints migration, fan-out targets/after-commit/best-effort, audit, console pane task, admin-panel read site, grouped CLI, isolated tests, OTP-untouched note, stale doc xrefs)
