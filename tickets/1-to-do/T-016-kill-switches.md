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

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 4 (§14), filed after T-014 (step 2 work) landed.
