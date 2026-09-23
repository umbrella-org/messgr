---
id: T-057
title: Platform console: tenant lifecycle, health, platform_audit
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: medium
cost: L
---

# T-057 — Platform console: tenant lifecycle, health, platform_audit

## Outcome

After this ships, provider staff operate cloud tenants through a console instead of `psql`:
tenant lifecycle, `tenant_schema_version` drift, per-tenant health/volume, platform kill
switches, and the `platform_audit` trail are all visible in one authenticated screen, with no
path to message content.

## Description

`10-query-api-ui.md` §11.4: "A separate surface, served by `messgr-control` against the control
database, for provider staff. It is **not** the tenant admin panel with extra permissions — a
different binary and a different authentication realm[.]" Concretely: the tenant admin panel
(T-049, done) lives in `messgr-query-api`; the platform console is a **new `serve` subcommand
added to the existing `messgr-control` binary** (`src/bin/control.rs`) — a different binary from
`messgr-query-api`, satisfying §11.4 without inventing a third binary. `messgr-control` today is
a pure CLI with no HTTP server at all; this ticket adds one.

Shows: tenant lifecycle (provision, suspend — read/trigger, reusing
`tenant::provision::provision_tenant`; offboard is visible but stays a disabled "coming soon"
action — see decision 4a below, `destroy_tenant` needs a `VaultClient` this server must never
hold), `tenant_schema_version` drift across the fleet, per-tenant health/volume (T-028's existing
`stats::tenant_message_stats` query), platform kill switches (T-058), and the `platform_audit`
trail.

**Never decrypts content, never holds a `KeyStore`, by construction.** This server holds only a
`control_pool` connection (`CONTROL_DATABASE_URL`) and never holds a `KeyStore` handle at all, so
there is no code path by which it could reach a tenant's Transit mount or decrypt a payload. (The
reused T-028 health query does open a short-lived tenant-database connection to read
`comms_request` counts — metadata, no payload columns, same pattern T-028's CLI subcommand
already uses in production; "never opens a tenant pool" was overstated in an earlier draft of
this ticket.) It can see metadata (volume, health, lifecycle state), and that visibility must be
disclosed to tenants as such (§11.4: "operators see metadata and every access is audited").

**New, separate auth realm — not `auth::provider::AuthProvider` reused.** That trait's `Identity`
carries a `tenant_id` (`src/auth/provider.rs`) — it is shaped for a request that is always
scoped to one tenant, which is the wrong shape for an operator identity that spans tenants by
design. This ticket adds a small parallel `PlatformAuthProvider` trait/`PlatformIdentity` (no
`tenant_id` field) plus a dev-only `MockPlatformProvider`, mirroring `auth::mock::MockProvider`'s
`profile.is_dev()` guard exactly (a mock auth provider reachable in production is a full
authentication bypass, same reasoning as §11.1). Confirmed decision (this refinement): a single
`operator` role is enough for the first slice — no RBAC differentiation among provider staff
until a concrete need for one demands it.

T-028 already shipped a `messgr-control stats` CLI subcommand (per-tenant message volume) — this
console is a strict superset in surface (a web UI, tenant lifecycle, drift, kill switches, audit
trail) and does not duplicate it; T-028's subcommand stays as the CLI-only path, the console adds
a `ui_health` view calling the same `stats::tenant_message_stats` query.

Soft coupling: displays platform kill switches (T-058) as one of its panes. The two are
independently buildable — this console can ship its other panes first and add the kill-switch
pane once T-058 lands — so this is a soft coupling, not a hard `depends-on:`.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-057-platform-console-tenant-lifecycle-health-platform-audit
```

### Prerequisite gate (hard)

None. T-028 (`messgr-control stats`) and T-049 (tenant admin panel, for the Askama+Datastar
pattern to mirror — T-049 shipped Datastar, not htmx; an earlier draft of this ticket said htmx)
are both done and merged. T-058 (platform kill switches) has not landed — this ticket ships
without the kill-switch pane and adds it later (soft coupling, Description). T-059 (tenant
offboarding destroy path) has landed but is **not** wired into this console — see decision 4a.

### Confirmed design decisions (do not deviate without asking)

1. **New `serve` subcommand on the existing `messgr-control` binary, not a new binary.** §11.4:
   "served by `messgr-control` against the control database." Add `Command::Serve { ... }` to
   `src/bin/control.rs`'s existing `Cli`/`Command` enum; it starts a long-lived axum server
   instead of returning, the same way every other subcommand there executes one operation.
2. **Plain TLS, not mTLS.** This is a human-facing console for provider staff, not a
   service-to-service call — mirror `messgr-query-api`'s `mtls::load_plain_server_config`
   (`src/bin/query_api.rs`), not `messgr-sms-sender`'s `ClientCertAcceptor`.
3. **New auth module `src/platform_auth/` (trait + mock), not `auth::provider::AuthProvider`
   reused.** `Identity` in `src/auth/provider.rs` carries `tenant_id` — wrong shape for an
   operator identity. `PlatformIdentity { actor: String, role: String }` (no `tenant_id`),
   `PlatformAuthProvider` trait with one method `authenticate(&self) -> Result<PlatformIdentity,
   AuthError>` (no `tenant_id` parameter — nothing to scope it to), `MockPlatformProvider`
   mirroring `auth::mock::MockProvider`'s `profile.is_dev()`-guarded constructor exactly. A single
   `operator` role constant for now (decision, this refinement) — do not build a role hierarchy
   speculatively.
4. **The server holds a `control_pool` — never a `KeyStore`.** This is what makes "cannot read
   content" true by construction rather than by an application-level check that could be
   bypassed by a future change. Do not add a `KeyStore` field to this binary's app state under
   any circumstance. (The T-028 health query's short-lived tenant-DB connection, task 2, is not a
   `KeyStore` and carries no Transit access — it's the same metadata-only pattern T-028's CLI
   subcommand already uses.)
4a. **Offboard-trigger stays a permanent stub, not a T-059-landed stub.** Applicability check on
    pickup found `tenant::offboard::destroy_tenant` (T-059, merged) takes a `&VaultClient`
    (`src/tenant/offboard.rs:26-31`) — a Transit-capable credential. Wiring it into this console
    would violate decision 4 and §11.4's "operators hold no Transit policy for any tenant mount"
    claim, regardless of whether T-059 has landed. The offboard-trigger action in the console
    stays a disabled/"coming soon" UI element indefinitely; actual destroy execution stays on the
    `messgr-control` CLI path T-059 already shipped, run by a human out-of-band. Do not call
    `destroy_tenant` from `platform_console` or give its app state a `VaultClient`.
5. **`platform_audit` gets a row for every console action that changes state** (suspend, offboard
   trigger, kill-switch engage/release once T-058 lands), via the existing
   `crate::platform_audit::record` function — same call shape `tenant::provision::provision_tenant`
   already uses. A read-only view (tenant list, schema-drift, health) does not need its own audit
   row — only state-changing actions do, matching `platform_audit`'s existing purpose
   ("provisioning, suspension, break-glass").

### Tasks

#### Task 1 — `src/platform_auth/` module
`mod.rs`, `provider.rs` (`PlatformIdentity`, `AuthError` reuse from `crate::auth::provider`'s
existing enum — same shape, no need for a second one — `PlatformAuthProvider` trait), `mock.rs`
(`MockPlatformProvider`), `role.rs` (`pub const OPERATOR: &str = "operator";`).

#### Task 2 — `src/platform_console/` module (views)
- `tenants.rs`: list tenants (`tenant::repo::list` — add if it doesn't already exist as a bare
  list-all query) with status, region, `tenant_schema_version` (join), created_at. A suspend
  action posts to a new route, calling `tenant::repo::mark_*` (extend `src/tenant/repo.rs` with
  `mark_suspended`/status-transition helpers if not already present) and writes a
  `platform_audit` row. Offboard-trigger renders as a disabled/"coming soon" action —
  permanently, per decision 4a, not until T-059 lands — and is not wired to `destroy_tenant`.
- `health.rs`: one view calling `crate::stats::tenant_message_stats` per tenant (T-028) — the
  same query the CLI subcommand already uses, no new SQL.
- `audit.rs`: paginated `platform_audit` read (newest first, filterable by `tenant_id`/`action`).
- `kill_switches.rs`: stub module now (empty view returning "not yet available"), filled in by
  T-058 per the soft coupling.

#### Task 3 — Templates
`templates/platform_console/base.html`, `tenants.html`, `health.html`, `audit.html` — Askama,
following `templates/admin/base.html`'s actual shipped structure (Datastar, `/assets/datastar.js`,
not htmx — T-049 diverged from §11's stale htmx text; T-055 will reconcile the doc), no other
client-side framework, `prefers-color-scheme` only.

#### Task 4 — Router + `Command::Serve`
`src/platform_console/mod.rs::router(app_state) -> Router` wiring the four views plus a login
stub reading `PLATFORM_MOCK_AUTH_ACTOR`/`PLATFORM_MOCK_AUTH_ROLE` env vars (mirroring
`query_api.rs`'s `mock_auth_actor`/`mock_auth_role` pattern). In `src/bin/control.rs`, add
`Command::Serve { listen_addr, health_listen_addr, cert_file, key_file }`: connects
`control_pool` only, builds `MockPlatformProvider`, starts the health listener
(`messgr::health::router()`, same as every other binary) and the TLS listener via
`mtls::load_plain_server_config` + `axum_server::bind_rustls`.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including:
   - `MockPlatformProvider` panics outside `profile = dev` (mirrors
     `mock_provider_refuses_to_construct_outside_dev_profile`).
   - A suspend action writes exactly one `platform_audit` row with the correct `action` string.
   - The health view returns the same counts `messgr-control stats` returns for the same tenant
     (mutation check: change a `comms_request.final_status` in the test fixture and confirm the
     view's number changes too, not just that it returns *some* number).
3. Manual: `cargo run --bin control -- serve`, load the console in a browser, confirm the tenant
   list, health view, and audit trail render, and that no code path in `src/platform_console/` or
   `src/platform_auth/` references `KeyStore` or `connect_tenant_pool` (grep as a cheap
   double-check: `grep -rn "KeyStore\|connect_tenant_pool" src/platform_console/ src/platform_auth/`
   must print nothing).

### Docs update (mandatory when user-facing)

Add a "Platform console" section to `docs/user-manual/control-plane-cli.adoc` (the `serve`
subcommand, its env vars, what the console shows and does not show, the metadata-not-content
disclosure). Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated and registered.
3. Write a summary (files touched, decisions made, anything deferred — note the kill-switch pane
   stub, filled by T-058) and hand back for review.
4. Suggested commit message: `feat(control): platform console — tenant lifecycle, health,
   platform_audit (T-057)`.

## Review

### Reviewer independence (step 0)

Independent. This review session has no prior involvement with
`feat/T-057-platform-console-tenant-lifecycle-health-platform-audit` — first touch on this
ticket in a fresh session — so nothing needed delegating.

### In-tree stale-branch check (step 0a)

`pickle doctor` on first checkout of the feature branch reported the ticket copy stale (`this
branch has it in "3-in-development" but main has it in "4-in-review"`). Rebased onto `main`
(one commit replayed cleanly); re-ran `pickle doctor` clean (only the unrelated
`payload_version "0.21.0" differs from binary "0.21.1"` warning remains).

### Implementation audit (step 2)

- Tasks 1–4: all present as specified — `src/platform_auth/{mod,provider,mock,role}.rs`,
  `src/platform_console/{mod,tenants,health,audit,kill_switches}.rs`, the four templates,
  `Command::Serve` in `src/bin/control.rs`. **Met.**
- `just build`, `just lint` (`cargo fmt --all -- --check` + `cargo clippy --all-targets
  --all-features -- -D warnings`): clean. **Met.**
- `just docs-check`: clean. **Met.**
- `cargo test --test platform_console`: `suspend_writes_exactly_one_platform_audit_row` passed;
  `health_view_matches_stats_and_reflects_a_status_mutation` **failed** on this re-run (F2
  below) — it evidently passed once, since the implementer's own successful run is what left
  the fixture poisoned for this one. **Not met as re-run.**
- Manual acceptance step 3: `cargo run --bin messgr-control -- serve` with a throwaway
  self-signed cert; `/ui/tenants`, `/ui/health`, `/ui/audit`, `/ui/kill-switches` all returned
  200 with the expected rendered content; the health listener's `/healthz` returned 200;
  `grep -rn "KeyStore\|connect_tenant_pool" src/platform_console/ src/platform_auth/` printed
  nothing. **Met.**
- Confirmed design decisions 1–4, 4a: honoured — plain TLS, the new `platform_auth` realm with
  no `tenant_id`, `control_pool`-only app state, offboard-trigger a permanent disabled stub
  never wired to `destroy_tenant`/`VaultClient`.
- Confirmed design decision 5 ("`platform_audit` gets a row for every console action that
  changes state"): **not honoured** under a DB write failure (F1 below).

### Quality audit (step 3)

- Idiomatic, matches `query_api`/`admin`'s existing conventions throughout (state extraction,
  `require_role`, template rendering).
- Askama's default HTML auto-escaping is in effect on every template; no `|safe` filter used
  anywhere in `templates/platform_console/`.
- `tenant_repo::mark_status` (pre-existing, shared with T-059) doesn't check `rows_affected`, so
  `suspend` against a nonexistent tenant id returns 200 rather than 404 — inherited behaviour,
  not introduced by this ticket (F3, non-blocking).
- Test coverage matches the two acceptance-test assertions plus `mock.rs`'s two
  `MockPlatformProvider` unit tests; no coverage of `require_role`'s reject path, acceptable
  given the closed one-role vocabulary this slice ships (decision 3).

### Consistency audit (step 4)

- `platform_console`'s `require_role`/`PlatformAuthedUser` pattern is structurally identical to
  `query_api::auth_mw`'s, as its own comments claim.
- `tenants::suspend`'s `.ok()` on `platform_audit::record`'s `Result` is inconsistent with every
  other call site in the codebase (`src/tenant/provision.rs:100,169`,
  `src/tenant/offboard.rs:52`), which all propagate the error with `?` (F1).
- `tenant_repo::list`'s full-row `SELECT` (including `vault_role_id`/`vault_pepper_wrapped`/
  `webhook_token`) matches the existing `find_by_slug`/`find_by_webhook_token` precedent in the
  same file; none of those columns reach a template. No new exposure.
- `stats::tenant_message_stats`'s `base_db_url` argument is fed `config.control_database_url`,
  the same value every existing `messgr-control` subcommand already passes it. Consistent.

### Documentation audit (step 4a)

- `docs/user-manual/control-plane-cli.adoc` gained a "Platform console" section covering the
  `serve` subcommand, its flags, its env vars, and the metadata-not-content disclosure.
  Coverage present.
- `just docs-check` clean.
- Whole-tree sweep: `grep -rln "platform console\|platform_console" docs/` returns only the file
  this ticket edited — no stale references elsewhere still describing the console as unbuilt.
- The doc's "every state-changing action... writes a `platform_audit` row" claim is only true on
  the happy path today (F1); it needs no separate doc fix, since fixing F1 makes it true again.

### Docs-readability pass (step 4b)

Conscious skip — no `docs_readability` tool or `docs-readability` subagent available in this
session.

### Findings

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | `tenants::suspend` discards the `Result` of `platform_audit::record` with `.ok()`, so a tenant can be suspended while its mandated `platform_audit` row silently fails to write on a DB error — violates T-057 decision 5 | `src/platform_console/tenants.rs:100-109`; contrast every other caller of `platform_audit::record`, which propagates with `?` (`src/tenant/provision.rs:100,169`, `src/tenant/offboard.rs:52`) | Propagate the error like every other call site (return 500, don't render the tenants page) instead of `.ok()` |
| F2 | blocking | test-gap | — | `health_view_matches_stats_and_reflects_a_status_mutation` hardcodes `cert_subject = "CN=test-producer"` (unlike the slug/db name in the same test, which use `unique_name`) against a control-wide uniqueness constraint on `producer_cert.cert_subject`, and `drop_test_tenant` never deletes that row — so the mandated acceptance test cannot be re-run in a persistent environment once it has passed once | `tests/platform_console.rs:258`; reproduced live: `cargo test --test platform_console` failed with `cert_subject "CN=test-producer" is already registered to a different tenant`; the poisoning `producer_cert` row (`tenant_id ce0a4f0b…`) was confirmed present in the control DB, left by the implementer's own prior successful run | Uniquify the cert_subject (`unique_name`-style) and have `drop_test_tenant` (or a new helper) delete the matching `producer_cert` row on cleanup |
| F3 | non-blocking | design | note-and-close | `tenant_repo::mark_status` (pre-existing, shared with T-059) doesn't check `rows_affected`, so `suspend` against a nonexistent tenant id returns 200 and writes an audit row rather than 404 | `src/tenant/repo.rs:132-138`; `src/platform_console/tenants.rs` `suspend` handler | Not reachable via the shipped UI today; worth a `rows_affected() == 0 -> 404` guard if a second caller ever makes the id externally addressable |

**Disposition summary:** 2 blocking (F1, F2) — routed to a scoped rework pass, not
dispositioned. 1 non-blocking, disposition note-and-close (F3).

cost: estimated L, actual L

### Rework fix record — round 1 (commit 4273d96)

- **F1** — `tenants::suspend` now propagates `platform_audit::record`'s `Result` the same way
  every other call site does: on error, logs and returns `500` instead of rendering the tenants
  page, so a suspend can no longer succeed while its mandated audit row silently fails to write.
  `src/platform_console/tenants.rs`.
- **F2** — `health_view_matches_stats_and_reflects_a_status_mutation` now registers the producer
  with a `unique_name`-derived `cert_subject` instead of the hardcoded `"CN=test-producer"`, and
  `drop_test_tenant` now deletes the tenant's `producer_cert` row(s) before deleting the tenant
  row, so a second run no longer collides on `producer_cert`'s control-wide `cert_subject`
  uniqueness constraint. Checked the live control DB for the poisoning row the review found
  (`cert_subject = 'CN=test-producer'`) — already absent in this environment — then ran
  `cargo test --test platform_console` twice back-to-back: both runs green
  (`suspend_writes_exactly_one_platform_audit_row`,
  `health_view_matches_stats_and_reflects_a_status_mutation`), proving the fixture no longer
  poisons a second run. `tests/platform_console.rs`.

`cargo build`, `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test` (full suite), and `just docs-check` all clean after the fix.

### Impact sweep (step 8)

`tickets/2-ready/T-058-platform-kill-switches-platform-tier-override-on-tenant-kill-switches.md`
references T-057 in three places, all keyed to "T-057 has not landed" (prerequisite gate) /
"until T-057... lands" (Task 5, Finish). This review routes T-057 to `5-rework/` rather than
`6-done/`, so that assumption is **still true** — T-057 has not landed — and no patch is needed
this round. Worth flagging for whoever runs T-057's scoped re-review after the fix pass: once
that re-review concludes to `6-done/`, its own step 8 should catch that T-058's prerequisite gate
can then name the real stub location (`src/platform_console/kill_switches.rs`) instead of just
"T-057's stub", and that **T-058's own task list (1–6) has no task that wires a real view into
it** — Task 5 only adds a CLI path.

## Scoped re-review — round 1

### Reviewer independence (step 0)

Independent. This review session has no hand in `feat/T-057-platform-console-tenant-lifecycle-
health-platform-audit` — first touch on this ticket, including the round-1 fix commit
(`4273d96`) — so nothing needed delegating.

### In-tree stale-branch check (step 0a)

`pickle doctor` on this branch reported the ticket copy stale (`this branch has it in
"5-rework" but main has it in "4-in-review"` — main had already picked up the "findings fixed"
board move). Rebased onto `main` (two commits replayed cleanly); re-ran `pickle doctor` clean
(only the unrelated `payload_version "0.21.0" differs from binary "0.21.1"` warning remains).

### Scoped re-review scope

Per the rules, this round reads only the two listed findings (F1, F2) plus the diff that closed
them — the round-1 fix commit `4273d96` (`git show 4273d96 -- src/platform_console/tenants.rs
tests/platform_console.rs`) — not a re-audit of the whole branch.

- **F1** — `tenants::suspend` (`src/platform_console/tenants.rs:97-109`) now wraps
  `platform_audit::record`'s call in `if let Err(err) = ... { tracing::error!(...); return
  StatusCode::INTERNAL_SERVER_ERROR.into_response(); }`, matching every other call site's `?`
  propagation in effect (log + 500 instead of silently rendering the tenants page). Confirmed
  the tenant-status write (`mark_status`) still happens first and the audit write second, so the
  fix targets exactly the finding: an audit-write failure now surfaces instead of vanishing.
  **Verified fixed.**
- **F2** — `drop_test_tenant` (`tests/platform_console.rs:39-67`) now deletes the tenant's
  `producer_cert` row(s) before deleting the `tenant` row itself, in the same position as the
  other pre-existing per-tenant cleanup deletes (`tenant_schema_version`, `platform_audit`) — FK
  order is respected. `health_view_matches_stats_and_reflects_a_status_mutation` now registers
  its producer with `&format!("CN={}", unique_name("test-producer"))` instead of the hardcoded
  `"CN=test-producer"`. Re-ran `cargo test --test platform_console` twice back-to-back in this
  session (not just re-reading the round-1 record): both runs green
  (`suspend_writes_exactly_one_platform_audit_row`,
  `health_view_matches_stats_and_reflects_a_status_mutation`, ~103s each), confirming the fixture
  no longer poisons a second run. **Verified fixed.**

No new defect found in the fix diff itself — it is a minimal, targeted change with no
side effects on the surrounding handler or test.

### Full-suite re-run (steps 2–4a, re-run in full since the fix touched shared test scaffolding)

- `cargo build`: clean.
- `just lint` (`cargo fmt --all -- --check` + `cargo clippy --all-targets --all-features -- -D
  warnings`): clean.
- `just test` (full workspace suite, not just `platform_console`): all suites green, 0 failed.
- `just docs-check`: clean (exit 0).

### Findings (round 1 re-review)

No new findings. F1 and F2 verified fixed as above; no other rows to add to the findings table.

**Disposition summary:** 0 new findings this round — F1 and F2 (both blocking, round 1) closed
by the fix commit; nothing carried forward.

cost: estimated L, actual L (unchanged — the rework round was a small, targeted fix, not a
re-estimate-worthy scope change)

### Impact sweep (step 8, round 1 re-review)

As flagged in the round-1 impact sweep above: this review concludes to `tickets/6-done/`, so
`T-058`'s "T-057 has not landed" assumption is now false. Patched
`tickets/2-ready/T-058-platform-kill-switches-platform-tier-override-on-tenant-kill-switches.md`:
the Prerequisite gate, Task 5, and Finish step 3 now name the real stub
(`src/platform_console/kill_switches.rs`) instead of "T-057's stub"/"until T-057 lands", and each
now flags that **neither T-057's own task list nor T-058's wires a real view into that stub** —
left for whoever refines T-058 further to add as a task, not invented here since it is a scope
decision on a different ticket, not a stale-reference fix. Recorded as a dated `## History` line
on T-058 itself.

No other `tickets/2-ready/` or `tickets/1-to-do/` ticket references T-057 in `depends-on:` or
Description (`grep -rl "T-057" tickets/1-to-do tickets/2-ready`).

### Checklist (round 1 re-review)

- [x] Reviewer independence settled (step 0): independent — this session has no hand in the branch
- [x] In-tree stale-branch check (step 0a): `pickle doctor` run, stale warning found and resolved by rebase, re-run clean
- [x] Scoped re-review — round-1 findings (F1, F2) verified against the fix diff (`4273d96`), plus full `just build`/`just test`/`just lint`/`just docs-check` re-run (steps 1, 2)
- [x] Quality audit — fix diff is minimal and targeted, no new issues (step 3)
- [x] Consistency audit — fix matches every other `platform_audit::record` call site's error propagation; cleanup delete ordering matches existing precedent (step 4)
- [x] Documentation audit — no doc changes needed for this fix; `just docs-check` clean (step 4a)
- [x] Docs-readability pass — n/a, no `.adoc`/`.md` changed this round (step 4b)
- [x] Findings recorded — none new; disposition summary + cost line present (step 5)
- [x] Ticket moved to `tickets/6-done/`; `## History` appended (step 6)
- [x] Other references checked — T-058 patched to reflect T-057 landing, with a flagged missing wiring task; governing documents reconciled — DESIGN.md §11.4 unchanged by this round's fix, still accurate (step 7)
- [x] Remaining-tickets impact sweep done — T-058 patched, no other ticket references T-057 (step 8)
- [x] Summary + commit message & MR attributes presented for approval (step 9)

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
- 2026-09-22 — TO DO → READY: plan complete: new serve subcommand on messgr-control (not a new binary, satisfying §11.4's separate-binary-from-the-tenant-admin-panel requirement), new PlatformAuthProvider realm (control_pool only, never a tenant pool/KeyStore)
- 2026-09-22 — plan amended inline: applicability gate (fresh sub-agent audit) on pickup found T-059's `destroy_tenant` (merged since this ticket went READY) requires a `VaultClient`, conflicting with decision 4's "never a KeyStore" invariant and §11.4 — offboard-trigger changed from "stub until T-059 lands" to a permanent stub (decision 4a added), never wired to `destroy_tenant`; Task 3's template precedent corrected from htmx to Datastar to match what T-049 actually shipped; Description's "never opens a tenant pool" claim softened to "never holds a KeyStore" since the reused T-028 health query does open a short-lived tenant-DB connection for metadata (non-blocking finding, disposition: fixed inline). User approved both routing decisions (offboard-trigger stays a stub; Task 3 follows Datastar).
- 2026-09-22 — READY → IN DEVELOPMENT: picked up
- 2026-09-22 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-23 — IN REVIEW → REWORK: review: 2 blocking findings (F1 platform_audit write silently swallowed on suspend, F2 acceptance test not idempotent — hardcoded cert_subject leaves permanent producer_cert pollution); 1 non-blocking (F3, note-and-close)
- 2026-09-23 — REWORK → IN REVIEW: findings fixed
- 2026-09-23 — IN REVIEW → DONE: scoped re-review: F1, F2 verified fixed, no new findings
- 2026-09-23 — post-done, pre-merge: `/code-review high` on PR #68 found 7 additional findings
  (empty `tenant_id` filter 400ing, `suspend` writing a `platform_audit` row on a nonexistent
  tenant with a non-transactional status/audit write pair, the platform-console serve command
  masking a crashed listener, serial per-tenant health queries, reflected JS injection via
  `data-signals`, and `base.html` duplicating `admin/base.html`); fixed 6 inline (commit
  `b456514`) with 2 new regression tests, left the `base.html` duplication as a non-blocking
  cleanup — disposition: note-and-close
- 2026-09-23 — merged to main (PR #68, `14b2271`)
