---
id: T-058
title: Platform kill switches: platform-tier override on tenant kill switches
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: medium
cost: M
---

# T-058 — Platform kill switches: platform-tier override on tenant kill switches

## Outcome

After this ships, an abusive or non-paying tenant can be suspended by the platform operator
without touching its data: a platform-tier kill switch overrides that tenant's own switch, and
the tenant's own admin panel shows the suspension as a distinct, non-actionable state rather than
looking like the tenant flipped its own switch off.

## Description

`platform_kill_switch` in the control database (§4.11/§4 gate-chain doc) — explicitly deferred by
T-016 ("Cloud-only, unread until step 19"; T-016 itself is done and merged, PR #19, `6059a13`,
and this ticket reuses its two-tier propagation shape rather than inventing a new one, so there
is no additional gating from that side).

**Two-tier: a platform switch overrides a tenant's switch, never the reverse.** Propagation fans
out per-tenant since there is no single `NOTIFY` target across databases — the dispatcher's 30s
periodic re-read is the guaranteed path, `NOTIFY` is the fast path, matching T-016's existing
tenant-switch mechanism.

Tenant admin panel (T-049, done) must show a platform suspension as a distinct, non-actionable
state — a tenant operator must not be able to mistake "platform suspended us" for "our own switch
is on" or be able to flip it back themselves.

Soft coupling: displayed as one pane of the platform console (T-057) — independently buildable,
no hard `depends-on:`.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
