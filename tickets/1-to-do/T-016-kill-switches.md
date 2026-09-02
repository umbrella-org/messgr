---
id: T-016
title: Kill switches
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-016 — Kill switches

## Outcome

An operator can stop message flow at any of five scopes (global, channel, producer,
producer_channel, campaign): new ingests matching the scope are rejected with a distinct error
code, dispatch stops for matching queued rows, and queued rows are held (not discarded, unless
`on_queued = 'discard'` is set) — all within seconds of the switch firing, and fully audited.
Auth traffic is structurally unaffected by any switch scope.

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

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 4 (§14), filed after T-014 (step 2 work) landed.
- 2026-09-02 — scope widened (TO DO). source: audit: design/implementation audit folded in five findings touching this ticket's own schema/mechanism: kill_switch's NULL-key uniqueness bug, the held-state spin risk, ingest-side kill-switch propagation (LISTEN doesn't work behind PgBouncer), auth_enabled's fail-open correction, and T-011/F3's unchecked tenant.status.
