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
   carries the previous platform entries over unchanged while the tenant's own rows still
   refresh — never "platform switch vanished because the control DB blipped", and never "a
   control-DB outage stopped the tenant's own switches taking effect".
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

### Reviewer independence (step 0)

Independent. This review session had no hand in
`feat/T-058-platform-kill-switches-platform-tier-override-on-tenant-kill-switches` (commit
`01b093f`); the branch was first seen here after it landed on the remote, so nothing needed
delegating.

### In-tree stale-branch check (step 0a)

`pickle` is not installed in this review's environment, so `pickle doctor` could not run —
recorded as a skip, not a pass. Checked by hand instead: the branch's merge-base is `652b3fc`
(the pickup commit on `main`), and `git diff --name-only origin/main...HEAD` lists no `tickets/`
path, so the worktree carried no stale ticket copy; the ticket was read from `origin/main` at
`aa6a81a`.

### Implementation audit (step 2)

- Tasks 1–9: all present in the files the plan names — `migrations/control/0006_…sql`,
  `src/platform_kill_switch/{mod,model,repo,configure}.rs`, `src/kill_switch/cache.rs`
  (`refresh_with_platform`, `PLATFORM_SCOPE`, `run_refresh_loop`'s `platform` arg),
  `src/bin/dispatcher.rs`, `src/dispatcher/{repo,drain}.rs`, `src/tenant/registry.rs`,
  `src/platform_console/{tenants,kill_switches,mod}.rs`, `src/bin/control.rs`,
  `src/query_api/admin.rs`, both templates. The console pane T-057 left as a stub (and flagged
  as unwired) is now a real view. **Met.**
- `just build` (`cargo build --all-targets`): clean. `just lint` (`cargo fmt --all -- --check`
  + `cargo clippy --all-targets --all-features -- -D warnings`): clean. **Met.**
- `just test`, re-run on local Postgres 16 + dev-mode Vault 1.20.4 with `cargo test
  --no-fail-fast`: every suite green, including `tests/platform_kill_switch.rs` 10/10,
  `tests/platform_console.rs` 7/7, `tests/admin_panel.rs` 4/4, `tests/kill_switch.rs` 6/6,
  `tests/ingest.rs` 22/22. Two environment notes, neither about this branch: (a) the four mTLS
  suites (`ingest`, `kill_switch`, `sms_sender`, `webhook`) need this sandbox's HTTPS proxy
  bypassed (`NO_PROXY='*'` for the local-only test process), exactly as the implementer
  recorded — without it all 22 `ingest` tests fail with a proxy connection reset; (b)
  `tests/dispatcher.rs::inserting_an_outbox_row_notifies_the_channels_listener`, untouched by
  this branch, hung once for more than 10 minutes in its setup (an idle `LISTEN "outbox_sms"`,
  no insert yet issued, so before its own 5 s timeout); the binary was killed and
  `tests/dispatcher.rs` then passed 30/30 on each of three isolated re-runs. **Met.**
- Acceptance-test bullets 2a–2g each map to a test in `tests/platform_kill_switch.rs`,
  `tests/platform_console.rs` or `tests/admin_panel.rs`. **Met.**
- Manual acceptance step 3 (compose stack, CLI engage, dispatcher stops within one 30 s tick):
  **not run** — no compose stack here, and the implementer did not run it either. The automated
  fan-out and cache tests cover the same path short of a live dispatcher process.
- Confirmed decisions 1–8: honoured. Decision 1 — `PlatformKillSwitch::as_kill_switch` is a
  `global`/`hold` row with the platform id; `blocking_scope` checks `platform_ids` first.
  Decision 2 — `NOTIFY kill_switch` into each tenant database, no control-DB listener. Decision
  3 — a failed control read carries the previous platform entries over while tenant rows still
  refresh (`a_failed_platform_read_keeps_…`). Decision 4 — no release path in the admin panel.
  Decision 5 — `claim_for_scope` takes `exclusion`, `drain_released_scope` rebuilds it from
  `active_snapshot()` each batch, discard passes the default. Decision 6 — partial unique index
  plus `ON CONFLICT … DO NOTHING`. Decision 7 — every engage/release/rejection audited via
  `record_tx` in the write's own transaction. Decision 8 — fan-out after commit, best-effort,
  status-filtered for region-wide switches.

**Addendum step 2.** (1) Not transcribed: the migration's own claims were checked
independently — a probe in a rolled-back transaction showed a second live `scope='platform'`
row (NULL `tenant_id`) deduped by `ON CONFLICT` (0 rows returned, 1 live row), and both CHECKs
firing (`platform_kill_switch_scope_tenant_check` on `('tenant', NULL)`,
`platform_kill_switch_scope_check` on `'region'`); the FK's premise holds — nothing in `src/` or
`migrations/` deletes a `tenant` row. (2) NULL semantics: handled by the `COALESCE` expression,
verified as above. (3) Leases: `claim_for_scope`'s new early return on `blocked_entirely`
leases nothing (`release_drain_sends_nothing_…` asserts `leased_until IS NULL`); the new "live
switch" state has a release path in the CLI and the console. (4) No secrets. (5) No
customer-data table; `erasure_coverage` green. (6) No new columns. (7) Invariant 1: only
`src/ingest/handler.rs:68` calls `blocking_scope`; `src/otp/` and `src/sms_sender/` never read
`kill_switches`, and the check is not in `resolve_producer`. Invariant 3: unchanged — a kill
switch rejection at ingest is the pre-existing T-016 pattern, not a gate. (8) `justfile` and
`.github/workflows/` untouched.

### Quality audit (step 3)

- Idiomatic and consistent with `kill_switch::configure`'s shape; `ConfigureError` leaves the
  caller's transaction committable on every domain outcome, which is what lets T-057's suspend
  treat `AlreadyEngaged` as success.
- Askama auto-escaping applies to the operator's free-text reason in the console; the admin
  panel never receives it (`platform_suspended_since` carries only timestamps).
- **Mutation checks (addendum step 3).** Dropping the platform merge in
  `refresh_with_platform` (rows read but never pushed into the active set) turned 3 tests red;
  removing `claim_for_scope`'s exclusion turned both release-drain tests red. Tree restored
  after each. The assertions can fail.
- Test isolation: the region-wide tests are serialized in-file and release their own switches
  on panic via `isolated`; cross-binary interference is ruled out by `cargo test` running
  binaries sequentially. See F1 for the one case not covered.

### Consistency audit (step 4)

- The `refresh`/`refresh_with_platform` split keeps every pre-T-058 caller's contract; the two
  production callers both pass a platform source.
- `claim_for_scope`'s exclusion SQL is the same `<> ALL` shape `claim` uses.
- The console's engage/release handlers follow `tenants::suspend`'s shape (role check, service
  call, re-render).
- Found: F2 (suspend engages a switch that has no automatic release counterpart), F3 (switch
  lifecycle vs. offboarding), F4 (registry poll in OTP processes).

### Documentation audit (step 4a)

- Coverage: `platform-kill-switch {engage,release,list}` documented in
  `control-plane-cli.adoc`; the platform tier (two-tier rule, release ramp, propagation,
  OTP unaffected, suspend relationship, what the tenant sees) in `kill-switches.adoc`; the
  console pane described alongside the other panes. The manual documents the console by pane,
  not by HTTP route (the T-057 suspend route is not listed either), so the two new `POST`
  routes follow the established convention. **Met.**
- Whole-tree sweep: no remaining "stub", "until T-058" or "unread until step 19" claims under
  `docs/`, `development/`, `src/` or `migrations/`.
- Build: `snowball` is not installed here (nor was it for the implementer). Approximated with
  `asciidoctor 2.0.26 --failure-level WARN docs/user-manual.adoc` on the branch and on
  `origin/main`: both exit 0 with no warnings. The PDF/EPUB renders were not built.

### Docs-readability pass (step 4b)

Conscious skip: no docs-readability reviewer is configured in this session.

### Findings (step 5)

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | test-gap | noted | No test engages a second live region-wide switch (`scope='platform'`, `tenant_id` NULL), which is the NULL case addendum step 2.2 exists for; only the tenant-scope duplicate is covered. The behaviour is correct. | Hand probe above: second `INSERT … ON CONFLICT` returned 0 rows, 1 live row. `tests/platform_kill_switch.rs:279` covers `scope::TENANT` only. | Add a region-wide duplicate-engage assertion the next time this suite is touched. |
| F2 | non-blocking | design | noted | Suspend engages a tenant-scope switch, but there is no reinstate path to release it, and releasing the switch on its own resumes dispatch for a tenant still `suspended` (ingest and OTP stay blocked). As designed (applicability-gate decision A2) and documented, but the two halves of "suspend" can now drift apart. | `src/platform_console/mod.rs:95-104`: `/tenants/{id}/suspend` has no counterpart. `control-plane-cli.adoc`'s "Suspend holds the queue too" tells the operator to release "when the tenant is reinstated". No ticket owns reinstatement. | Whoever builds reinstatement should release the suspend switch in the same transaction. |
| F3 | non-blocking | design | noted | A tenant-scope switch outlives its tenant's `offboard-destroy`: it stays live and listed indefinitely, and releasing it fans out to a dropped database (counted as `notify_failed`). The console's tenant picker also offers `provisioning` and `offboarding_destroy` tenants. Cosmetic: release still works. | `src/platform_kill_switch/configure.rs` `fan_out_targets`: the `scope::TENANT` arm does not filter by status. `templates/platform_console/kill_switches.html`: the tenant `<option>` loop is unfiltered. | None needed unless the console list gets noisy. |
| F4 | non-blocking | design | noted | `messgr-otp` and `messgr-sms-sender` open tenants through `TenantRegistry`, whose 5 s poll now also reads `platform_kill_switch` from the control database. Neither process ever reads the result. Invariant 1 is intact, and the tenant `kill_switch` read was already unused there before T-058. | `src/tenant/registry.rs:208-218`. `grep -rn kill_switch src/otp src/sms_sender` returns only doc comments. | Act only if control-DB load from OTP replicas shows up. |

Disposition summary: 4 noted (F1–F4); 0 fixed inline, 0 folded, 0 new tickets.
cost: estimated L, actual L

### Governing documents (step 7)

The branch reconciled `development/design/03-data-model.md` (drops "unread until step 19",
records the 0006 constraints) and `04-gate-chain.md` (new "How the two tiers meet" paragraph,
including the on-the-record correction that the T-016 drain ignored still-engaged switches).
Decision 12 ("Release is drain-rate limited") is still accurate as written, and no Still-open
item concerns platform switches. The branch did not bump `DESIGN.md`'s version stamp (still
Version 11), which matches T-056's precedent. This review changes nothing in `DESIGN.md`, so the
addendum's step 5 bump rule does not apply. `tickets/BOARD.md` was **not regenerated**: `pickle`
is unavailable here, and `main`'s board was already stale before this review (it still lists
T-058 under READY), so `pickle board sync` must be run where `pickle` is installed.

### Impact sweep (step 8)

No open ticket lists T-058 in `depends-on:` or names it. T-060 (READY) relies on "per-region
kill switch … independence". T-058 scopes a region-wide switch to every tenant in *one* control
database, and T-060 gives each region its own, so the assumption holds and is now a concrete,
testable property. Flagged in T-060's History for its implementer; its plan is unchanged.

### Checklist

- [x] Reviewer independence settled (step 0): independent — no hand in the branch
- [x] In-tree stale-branch check (step 0a): `pickle doctor` unavailable (skip recorded); checked by hand — branch diff carries no `tickets/` path
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2); manual compose step not run
- [x] Quality audit (step 3), including two mutation checks
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage met, whole-tree sweep clean, docs build approximated with asciidoctor at WARN (snowball unavailable) (step 4a)
- [x] Docs-readability pass — conscious skip: no reviewer configured (step 4b)
- [x] Findings recorded with severity, class and disposition; disposition summary and cost line present (step 5)
- [x] Ticket moved to `tickets/6-done/`; `## History` appended (step 6) — board regeneration pending `pickle board sync`
- [x] Governing documents reconciled by the branch; no review edits needed (step 7)
- [x] Remaining-tickets impact sweep done — T-060 flagged (step 8)
- [x] Summary + commit message & MR attributes presented for approval (step 9) — publish pending user approval

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
- 2026-09-25 — READY → IN DEVELOPMENT: picked up
- 2026-09-25 — plan amended inline: decision 3 — a failed control-database read no longer fails the whole refresh (that coupled the tenant's own kill switches to control-DB availability, a regression found in self-review); the last-known platform rows are kept and the tenant rows still refresh, covered by a new test
- 2026-09-25 — IN DEVELOPMENT → IN REVIEW: acceptance green — `feat/T-058-…` commit 01b093f; build/lint clean; full `cargo test` green on local Postgres 16 + dev Vault (the four mTLS-server suites needed NO_PROXY for this sandbox's HTTPS proxy); `just docs-check` not run (snowball unavailable here); console pane wired (no longer a stub)
- 2026-09-25 — IN REVIEW → DONE: validated, no blocking findings; 4 noted (F1–F4), 0 fixed inline, 0 folded, 0 new tickets; acceptance re-run green (mTLS suites need the sandbox proxy bypassed); board not regenerated here (pickle unavailable) — run `pickle board sync`
