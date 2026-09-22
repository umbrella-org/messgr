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

Shows: tenant lifecycle (provision, suspend, offboard — read/trigger, reusing
`tenant::provision::provision_tenant` and T-059's destroy path rather than duplicating either),
`tenant_schema_version` drift across the fleet, per-tenant health/volume (T-028's existing
`stats::tenant_message_stats` query), platform kill switches (T-058), and the `platform_audit`
trail.

**Metadata only, never content, by construction.** This server holds only a `control_pool`
connection (`CONTROL_DATABASE_URL`) — it never opens a tenant pool and never holds a `KeyStore`
handle at all, so there is no code path by which it could reach a tenant's Transit mount or
decrypt a payload. It can see metadata (volume, health, lifecycle state), and that visibility
must be disclosed to tenants as such (§11.4: "operators see metadata and every access is
audited").

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

None. T-028 (`messgr-control stats`) and T-049 (tenant admin panel, for the Askama+htmx pattern
to mirror) are both done and merged. T-058 (platform kill switches) has not landed — this ticket
ships without the kill-switch pane and adds it later (soft coupling, Description).

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
4. **The server holds a `control_pool` only — never a tenant pool, never a `KeyStore`.** This is
   what makes "cannot read content" true by construction rather than by an application-level
   check that could be bypassed by a future change. Do not add a `KeyStore` field to this
   binary's app state under any circumstance.
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
  list-all query) with status, region, `tenant_schema_version` (join), created_at. A
  suspend/offboard-trigger action posts to a new route, calling `tenant::repo::mark_*`
  (extend `src/tenant/repo.rs` with `mark_suspended`/status-transition helpers if not already
  present) and T-059's destroy path for the offboard-trigger action once T-059 lands (soft
  coupling — until then, offboard-trigger is a visible but disabled/"coming soon" action, not a
  half-built destructive one).
- `health.rs`: one view calling `crate::stats::tenant_message_stats` per tenant (T-028) — the
  same query the CLI subcommand already uses, no new SQL.
- `audit.rs`: paginated `platform_audit` read (newest first, filterable by `tenant_id`/`action`).
- `kill_switches.rs`: stub module now (empty view returning "not yet available"), filled in by
  T-058 per the soft coupling.

#### Task 3 — Templates
`templates/platform_console/base.html`, `tenants.html`, `health.html`, `audit.html` — Askama,
following `templates/admin/base.html`'s structure (htmx fragment swaps, no client-side
framework, `prefers-color-scheme` only, per §11's stated stack and T-027's UI precedent).

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

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
- 2026-09-22 — TO DO → READY: plan complete: new serve subcommand on messgr-control (not a new binary, satisfying §11.4's separate-binary-from-the-tenant-admin-panel requirement), new PlatformAuthProvider realm (control_pool only, never a tenant pool/KeyStore)
