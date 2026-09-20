---
id: T-049
title: Admin panel: quota dashboard, kill-switch console, scheduled queue, producer registry
project: messgr
depends-on: [T-048]
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-049 — Admin panel: quota dashboard, kill-switch console, scheduled queue, producer registry

## Outcome

After this ships, a `comms_ops` user can see per-producer quota consumption, pull or release a
kill switch after seeing its blast radius, and view/cancel pending scheduled sends; an `admin`
user can register/disable producers and set their quotas and overrides — all without `psql` and
without touching the primary for read-only views (§11.3). The T-016 `psql` runbook for
engage/release becomes the documented fallback, not the only way to do it; the auth kill
switch's runbook (T-016 decision 29) is untouched — this panel still never engages or releases
it.

## Description

Build-order step 14 (§14): the admin panel, same binary and same server-rendered stack as the
query API/UI (T-048, §11.3) — Askama + htmx, no SPA, matching `templates/query_api/*.html` —
gated per §11.1's role table, which splits the four items below across two roles, not evenly:

- **Quota dashboard** (`comms_ops`). Per producer × channel × class: current-minute/current-day
  consumption vs. limit, a trailing-24h sparkline, blocked/deferred count. Reads `producer_usage`
  (dispatcher-flushed every few seconds, so sub-minute freshness with no metrics stack in the
  path) — deliberately Postgres, not Prometheus, because "the operational view must not depend
  on the monitoring system being healthy." `producer_usage` already carries per-minute rows
  (T-042), enough for the sparkline; no new table.
- **Kill-switch console** (`comms_ops`). Engage/release by scope with a mandatory reason; shows
  blast radius (queued-message count by class) *before* engaging. Permanently displays that auth
  traffic is never affected by any switch shown here (§5.2). **Resolved at refinement (user
  confirmed): the auth kill switch stays fully out-of-band, per T-016 decision 29's `psql`
  runbook** — this panel never reads or writes `auth_enabled` and builds no dual-control
  workflow. That closes decision-29/still-open-#9's question for build-order step 14 (see the
  docs task below). Engage/release itself has no existing code to reuse: T-016 built the
  `kill_switch` table, the dispatcher's read-side cache, and the release-drain mechanism, but
  scoped mutation to a `psql` runbook explicitly ("the operator-facing panel is step 14 and out
  of scope here") — this ticket is what actually builds it.
- **Scheduled queue view** (`comms_ops`). Pending future-dated messages by producer, campaign,
  and due window, with cancel actions. Queries `outbox` directly (not `comms_query::repo::list`,
  which filters `comms_request.created_at` — a different column from the due-window
  `next_attempt_at` this view needs, and `comms_request` is the 7-year partitioned ledger, not
  the small live queue). Confirmed present: the `(producer_id, next_attempt_at)` /
  `(campaign_id, next_attempt_at)` indexes §4's correction note added for this exact view
  (`development/design/03-data-model.md` line 83-84, verified in the current schema at
  refinement). Cancelling calls `ingest::repo::cancel` in-process (same crate as
  `messgr-ingest`'s `DELETE /comms/{id}`, T-041) rather than an HTTP hop to that separate binary
  — same "must not depend on another service being up" reasoning as the quota dashboard.
- **Producer registry** (`admin`). Register/disable, set quotas, grant time-boxed overrides
  (reuses T-005's registry and T-042's quota/override machinery). Every mutation audited with
  actor, timestamp, and before/after values via the existing `platform_audit::record` calls
  already wired into `producer::register` and `producer_quota::configure` — no new audit
  mechanism.

**Hard dependency on T-048 (`depends-on:`, user-confirmed, done and merged — PR #63).** §11.3
opens with "same binary, same server-rendered stack" as the query-api/UI T-048 stands up — this
panel's routes need T-048's `AuthProvider`/role-gating in place to be gated at all, not just to
share a process.

**Out of scope, resolved at refinement (user confirmed a follow-up ticket, not this one):**
template approval and quiet-hours-policy UI, and provider-config UI — all three are under the
`admin` role's scope in §11.1 but not itemized under §11.3's four admin-panel items above, and
all three already have CLI-only configure modules (`template::approve`, `quiet_hours::configure`,
`provider_config::configure`) that a follow-up could expose the same way this ticket exposes
`producer_quota::configure`. Also out of scope: the platform console (§11.4, already a separate
binary/auth realm).

`docs/mockups/admin-dashboard.html` (a T-027 static mockup, "Overview" tab only, "Kill switches"
present only as an inert nav link) is a visual/layout reference for KPI-grid and top-nav styling
only — its own comment floats "htmx or Datastar" as still open, which predates and is superseded
by §11 and T-048's shipped decision (Askama + htmx). Follow T-048's stack, not the mockup's
comment.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .   # messgr is the root-path child (path = "." in pickle.toml)
git checkout main
git checkout -b feat/T-049-admin-panel-quota-dashboard-kill-switch-console-scheduled-queue-producer-registry
```

WIP commits locally as you go. Publish (push / MR) only after user approval, per the project's
commit policy — see Finish.

### Prerequisite gate (hard)

`depends-on: [T-048]` — done and merged to `main` (PR #63, `adaced5`). Board WIP clear:
`3-in-development/` 0/1, `4-in-review/` 0/1 before pickup.

### Confirmed design decisions (do not deviate without asking)

1. **Role split follows §11.1 literally, not "both roles see everything."** `comms_ops` gets
   the quota dashboard (view), the kill-switch console (view/engage/release), and the scheduled
   queue (view/cancel) — §11.1's comms_ops row lists exactly those three verbs. `admin` gets the
   producer registry (view/register/disable/set-quota/add-override) — §11.1's admin row lists
   "producer registry, quota overrides." Neither role is a superset of the other here; a route
   gated to one is `403` for the other. `require_role` (already in `src/query_api/auth_mw.rs`)
   takes each route's own single-role slice, not `&[role::COMMS_OPS, role::ADMIN]`.
2. **The auth kill switch is untouched by this ticket** (user-confirmed at refinement, closing
   decision-29/still-open-#9 for build-order step 14). No `auth_enabled` read, no write, no
   dual-control UI. The kill-switch console's permanent on-screen note (§5.2, "auth traffic is
   not affected by any switch shown here") is static copy in the template, not derived from a
   query.
3. **Admin-panel handlers call the tenant-pool-taking inner functions, not the CLI-shaped outer
   ones.** `producer::register::{register_producer, disable_producer}` and
   `producer_quota::configure::set_producer_quota` each resolve `tenant_slug` to a fresh pool via
   `connect_tenant_pool`, do the work through a private `*_inner` function, then close that pool
   — correct for a one-shot CLI invocation, wasteful for a request handler that already has the
   tenant's pool open via `TenantContext` (backed by `state.pool_cache`, T-048 decision 4).
   Promote `register_producer_inner`, `disable_producer_inner`, and `set_producer_quota_inner`
   from private to `pub(crate)` in their modules (`src/producer/register.rs`,
   `src/producer_quota/configure.rs`) and call them directly from the new handlers with
   `tenant.pool`, `tenant.tenant_id`, and `identity.actor` — no new tenant pool, no duplicate
   `tenant_repo::find_by_slug` lookup. `list_producers`, `list_producer_quota`,
   `list_producer_quota_overrides` already take a pool directly; call them unchanged.
4. **Kill-switch engage/release is new code in `src/kill_switch/`, mirroring the existing
   configure-module shape** (`producer_quota/configure.rs` is the closest precedent: a
   `ConfigureError`/outcome enum, an `audit_*` helper calling `platform_audit::record`). Add
   `src/kill_switch/configure.rs`:
   - `engage(pool, control_pool, tenant_id, scope, scope_key, on_queued, reason, actor)` —
     validates `scope` against `kill_switch::model::scope`'s five constants and `on_queued`
     against `on_queued::{HOLD, DISCARD}` before inserting (reject, audited, on either being
     unrecognised); maps the unique-index violation on
     `(scope, COALESCE(scope_key, '')) WHERE released_at IS NULL` (already in
     `migrations/tenant/0009_kill_switch.sql`) to a domain `AlreadyEngaged` outcome via
     `sqlx::Error::Database` + `.constraint()` name check, not a raw `500`.
   - `release(pool, control_pool, tenant_id, id, actor)` — `UPDATE kill_switch SET released_by =
     $1, released_at = now() WHERE id = $2 AND released_at IS NULL RETURNING id`; `None` from
     `fetch_optional` means "already released or unknown id" (idempotent `204`-equivalent, same
     pattern as T-041's `cancel`).
   Both audit via `platform_audit::record(control_pool, actor, "kill_switch.engage"|"release",
   Some(tenant_id), json!({...}))`, matching every existing configure module's pattern. Engaged
   rows already fire `pg_notify('kill_switch', ...)` via the existing trigger — the dispatcher
   picks up the change through its existing 30s poll/notify listener; no new wiring needed there.
5. **Blast radius is a live count against `outbox`, computed the moment the operator asks, not
   cached.** New `src/kill_switch/repo.rs::blast_radius(pool, scope, scope_key) -> Result<Vec<(String,
   i64)>, sqlx::Error>`: `SELECT class, count(*) FROM outbox WHERE cancelled_at IS NULL AND
   <predicate> GROUP BY class`, built with `sqlx::QueryBuilder` and a `match scope` with the same
   five arms as `dispatcher::repo::claim_for_scope` (`src/dispatcher/repo.rs:68`) — global: no
   filter; channel: `channel = scope_key`; producer: `producer_id = scope_key::uuid`;
   producer_channel: split `scope_key` on `:` (reuse
   `kill_switch::model::split_producer_channel`'s logic — make it `pub(crate)`, currently
   private in `model.rs`) and filter both columns; campaign: `campaign_id = scope_key`. An
   unparseable `producer`/`producer_channel` `scope_key` returns an empty result (matches
   `KillSwitch::matches`'s own "treat as matching nothing" behaviour), not an error.
6. **Scheduled queue is a new query-only module, `src/outbox_query/{model.rs,repo.rs}`,
   mirroring `comms_query`'s existing shape** (a filter struct + one `list` function). `filter.rs`
   fields: `producer_id: Option<Uuid>`, `campaign_id: Option<String>`, `due_before: Option<DateTime<Utc>>`,
   `due_after: Option<DateTime<Utc>>`. `repo::list(pool, filter, limit)` queries `outbox` (not
   `comms_request`) — `SELECT comms_request_id, created_at, channel, class, producer_id,
   campaign_id, next_attempt_at, expires_at FROM outbox WHERE cancelled_at IS NULL AND
   ($1::uuid IS NULL OR producer_id = $1) AND ($2::text IS NULL OR campaign_id = $2) AND
   ($3::timestamptz IS NULL OR next_attempt_at <= $3) AND ($4::timestamptz IS NULL OR
   next_attempt_at >= $4) ORDER BY next_attempt_at LIMIT $5` — served by the existing
   `(producer_id, next_attempt_at)` / `(campaign_id, next_attempt_at)` indexes. Cancelling a row
   from this view calls `ingest::repo::cancel(pool, comms_request_id, producer_id)` unchanged —
   the listing already carries each row's own `producer_id`, so no signature change to T-041's
   producer-scoped cancel is needed.
7. **New routes are server-rendered UI only — no OpenAPI entries, no JSON contract.** They live
   under `/t/{tenant_slug}/admin/...` (mutations, POST, form-encoded) and
   `/t/{tenant_slug}/ui/admin/...` (views, GET, HTML/htmx fragments), added to the existing
   `data_routes` router in `src/query_api/mod.rs` alongside the current six. `openapi/query-api.yaml`
   documents only the pre-existing REST data routes (per its current scope) and is not extended —
   these are operator UI actions, not a published API. `docs/user-manual/query-api.adoc` gets a
   new `== Admin panel` section instead (see Docs update).
8. **Producer-registry input surface stays minimal, matching the `psql`-runbook precedent
   T-016/T-042's docs already use for scope_key formats.** The `producer_channel` kill-switch
   scope takes one raw `<producer_id>:<channel>` text field (the literal storage format
   `KillSwitch::split_producer_channel` parses), not two composed dropdowns — avoids a client-side
   composition step for a scope that's rarely used (§5.2's common cases are global/channel/
   producer). Same minimalism for the quota-override form: the three existing
   `ProducerQuotaOverrideInput` fields (`per_day`, `valid_from`/`valid_to`, `reason`) map directly
   to form fields, no new derived UI state.

### Tasks

#### Task 1 — Kill-switch engage/release + blast radius

`src/kill_switch/configure.rs` (new): `engage`, `release`, both audited, per decision 4.
`src/kill_switch/repo.rs`: add `blast_radius` per decision 5.
`src/kill_switch/model.rs`: make `split_producer_channel` `pub(crate)` (currently private,
needed by `blast_radius`).

#### Task 2 — Visibility bumps for admin-panel reuse

`src/producer/register.rs`: `register_producer_inner`, `disable_producer_inner` → `pub(crate)`.
`src/producer_quota/configure.rs`: `set_producer_quota_inner` → `pub(crate)`. No behaviour
change — visibility only.

#### Task 3 — Scheduled-queue query module

New `src/outbox_query/{mod.rs,model.rs,filter.rs,repo.rs}` per decision 6. Register `pub mod
outbox_query;` in `src/lib.rs` alongside `pub mod comms_query;`.

#### Task 4 — Quota-dashboard usage history for the sparkline

`src/producer_quota/repo.rs`: add `load_usage_history(pool, since: DateTime<Utc>) ->
Result<Vec<UsageRow>, sqlx::Error>` — `SELECT * FROM producer_usage WHERE granularity = 'minute'
AND window_start >= $1 ORDER BY window_start`, trailing-24h window computed by the caller
(`Utc::now() - Duration::hours(24)`). `load_current_usage` (existing) still serves the
current-minute/current-day numbers; this is additive for the sparkline series only.

#### Task 5 — Admin routes and handlers

`src/query_api/mod.rs`: add to `data_routes`:
```
.route("/ui/admin/quota", get(admin::ui_quota_dashboard))
.route("/ui/admin/kill-switches", get(admin::ui_kill_switches))
.route("/ui/admin/kill-switches/blast-radius", get(admin::ui_blast_radius))
.route("/admin/kill-switches", post(admin::engage_kill_switch))
.route("/admin/kill-switches/{id}/release", post(admin::release_kill_switch))
.route("/ui/admin/scheduled", get(admin::ui_scheduled_queue))
.route("/admin/scheduled/{id}/cancel", post(admin::cancel_scheduled))
.route("/ui/admin/producers", get(admin::ui_producer_registry))
.route("/admin/producers", post(admin::register_producer))
.route("/admin/producers/{id}/disable", post(admin::disable_producer))
.route("/admin/producers/{id}/quota", post(admin::set_producer_quota))
.route("/admin/producers/{id}/quota/override", post(admin::add_quota_override))
```
New `src/query_api/admin.rs` (handlers, mirroring `views.rs`/`handlers.rs`'s existing
`AuthedUser`/`TenantContext`/`require_role` pattern per decision 1). Each mutation handler
returns the refreshed htmx fragment (matching the `Html`/`render()` helper already in
`views.rs`) rather than a redirect, consistent with the rest of the UI.

#### Task 6 — Templates

New `templates/query_api/admin_quota.html`, `admin_kill_switches.html`,
`admin_scheduled.html`, `admin_producers.html`, extending `templates/query_api/base.html`
(existing layout/nav, matching `timeline.html`/`campaign_reach.html`'s structure). Kill-switch
engage form includes the static "auth traffic is not affected by any switch shown here" note
(decision 2) and an htmx `hx-get` to `/ui/admin/kill-switches/blast-radius` on scope/scope_key
input change, showing the count *before* the engage button is enabled.

### Acceptance test

`cargo test admin` after adding: a `tests/admin_panel.rs`, following `tests/query_api.rs`'s
existing `provision_test_tenant`/`build_router`/`MockProvider` helpers (see
`producers_usage_and_quota_are_comms_ops_only` at `tests/query_api.rs:677` for the pattern):

- Engage a `producer`-scope switch as `comms_ops`; assert `403` for `admin` and every other
  role. Assert the blast-radius endpoint returns the correct per-class counts for seeded
  `outbox` rows matching that producer, and zero for rows outside its scope.
- Engage the same scope twice; assert the second call is rejected (already engaged), not a raw
  `500`. Release it; assert a second release is idempotent.
- Register a producer as `admin`; assert `403` for `comms_ops`. Set its quota, add a
  time-boxed override; assert both round-trip through `producer_quota_repo::load_one` /
  `list_overrides`, and that a `class = "auth"` quota request is rejected (existing invariant,
  now reachable through the new route).
- Seed three `outbox` rows (two matching a producer, one not); list the scheduled queue filtered
  by that `producer_id`; assert exactly two returned, ordered by `next_attempt_at`. Cancel one
  via the new route; assert `cancelled_at` is set and the row drops out of a second listing.
- `just build`, `just lint`, `just test` all green.

### Docs update (mandatory when user-facing)

- `docs/user-manual/query-api.adoc`: retitle away from "read-only" (no longer accurate once
  admin mutations exist) and add an `== Admin panel` section covering the four views, their
  role gating, and that the auth kill switch is deliberately absent from it.
- `docs/user-manual/kill-switches.adoc`: "Engaging a switch" / "Releasing a switch" sections
  get a note that the admin panel is now the primary path, with the `psql` runbook kept as the
  documented fallback (matches the Outcome's own wording) — the exact commands stay, since
  `auth_enabled` and any panel-unreachable scope still need them.
- `development/design/14-decisions-and-open-questions.md`: mark still-open item 9 resolved for
  build-order step 14 (strikethrough, matching item 8's existing style) — "resolved: the admin
  panel (T-049) does not enforce dual control for the auth switch; it remains out-of-band per
  the T-016 runbook." Leave the `otp-api` (step 17) half of item 9 open, since that reader still
  doesn't exist.
- Register any new `.adoc` file in `docs/user-manual.adoc`'s include list if a new file is added
  rather than extending the two above (current plan extends existing files, so likely no new
  include needed — confirm during implementation).

### Finish (mandatory)

1. Acceptance test green; `just build`, `just lint`, `just test`, `just docs-check` all clean.
2. Docs updated and registered per above.
3. Write a summary: files touched, decisions made, anything deferred (the follow-up ticket for
   template-approval/quiet-hours/provider-config admin UI, noted in the Description, is not
   filed by this ticket — leave that to whoever picks it up next).
4. Suggest a Conventional Commit message, ticket id in brackets at the end of the subject, e.g.:
   ```
   feat(query-api): add admin panel — quota, kill switches, scheduled queue, producers (T-049)
   ```
5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child, `path = "."`).
6. Commit locally on the ticket branch. Publish only per the project's commit policy (no push /
   MR without explicit user approval). Before pushing, verify the remote base is not behind:
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` must
   print nothing. Present the commit message for approval; only after approval push and open the
   merge request — merging is always the human's. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 14, remaining gap identified when auditing unticketed steps against the board
- 2026-09-19 — added hard depends-on: [T-048], user-confirmed (shared binary + role-gating, §11.3)
- 2026-09-20 — refined: user confirmed the auth kill switch stays out-of-band (closes decision-29/still-open-#9 for step 14) and template-approval/quiet-hours/provider-config admin UI is deferred to a follow-up ticket, not built here; re-graded complexity medium → high; Implementation Plan written
- 2026-09-20 — TO DO → READY: plan complete
