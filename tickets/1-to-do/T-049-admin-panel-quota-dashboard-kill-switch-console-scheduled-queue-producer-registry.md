---
id: T-049
title: Admin panel: quota dashboard, kill-switch console, scheduled queue, producer registry
project: messgr
depends-on: [T-048]
spawned-by: []
impact: high
complexity: medium
cost: L
---

# T-049 — Admin panel: quota dashboard, kill-switch console, scheduled queue, producer registry

## Outcome

After this ships, a `comms_ops` or `admin` user can see per-producer quota consumption, pull or
release a kill switch after seeing its blast radius, view and cancel pending scheduled sends,
and manage the producer registry — without `psql` and without touching the primary for
read-only views (§11.3). The step-4/9 `psql` runbooks become the documented fallback, not the
only way to do any of this.

## Description

Build-order step 14 (§14): the admin panel, same binary and same server-rendered stack as the
query API/UI (T-048, §11.3), gated to the `comms_ops` and `admin` roles that ticket's
`AuthProvider`/RBAC already defines. Scope, per §11.3:

- **Quota dashboard.** Per producer × channel × class: current-minute/current-day consumption
  vs. limit, a trailing-24h sparkline, blocked/deferred count. Reads `producer_usage`
  (dispatcher-flushed every few seconds, so sub-minute freshness with no metrics stack in the
  path) — deliberately Postgres, not Prometheus, because "the operational view must not depend
  on the monitoring system being healthy."
- **Kill-switch console.** Engage/release by scope with a mandatory reason; shows blast radius
  (queued-message count by class) *before* engaging. Permanently displays that auth traffic is
  never affected by any switch shown here (§5.2) — the auth kill switch is a separate,
  differently-styled control. Decision 29 / still-open #9 (`14-decisions-and-open-questions.md`):
  whether this panel should mechanically enforce the auth switch's two-person approval, or
  continue recording it out-of-band per the T-016 runbook, is an open call for refinement to
  put to the user — not something to default silently.
- **Scheduled queue view.** Pending future-dated messages by producer/campaign/due-window, with
  cancel actions (uses T-041's cancellation endpoint). Needs the `(producer_id, next_attempt_at)`
  / `(campaign_id, next_attempt_at)` indexes §4's correction note already added for this exact
  view — confirm they're still present in the current schema at refinement.
- **Producer registry.** Register/disable, set quotas, grant time-boxed overrides (reuses
  T-005's registry and T-042's quota-override machinery). Every mutation audited with actor,
  timestamp, before/after values.

**Hard dependency on T-048 (`depends-on:`, user-confirmed).** §11.3 opens with "same binary,
same server-rendered stack" as the query-api/UI T-048 stands up — this panel's routes need
T-048's `AuthProvider`/role-gating in place to be gated at all, not just to share a process.

Out of scope: template approval and quiet-hours-policy UI (mentioned in the `admin` role's
scope in §11.1 but not itemized under §11.3's four admin-panel items above — confirm at
refinement whether they belong in this ticket or a follow-up), the platform console (§11.4,
already a separate binary/auth realm), and whether the panel should mechanically enforce
two-person approval for the auth switch (flagged above as an open call, not decided here).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 14, remaining gap identified when auditing unticketed steps against the board
- 2026-09-19 — added hard depends-on: [T-048], user-confirmed (shared binary + role-gating, §11.3)
