---
id: T-013
title: Minimal dispatcher: per-channel claim loop, LISTEN/NOTIFY wakeup, comms_event write
project: messgr
depends-on: [T-011, T-012]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-013 — Minimal dispatcher: per-channel claim loop, LISTEN/NOTIFY wakeup, comms_event write

## Outcome

After this ships, an outbox row actually gets sent: a per-channel claim loop with leases and
`SKIP LOCKED` picks it up, `LISTEN`/`NOTIFY` wakes the dispatcher with a 1s poll fallback, and
every state change lands as a `comms_event` alongside a single `final_status` update. This is
the ticket that completes the first real end-to-end send in build step 2.

## Description

Build the minimal dispatcher per design §4.2, §4.1, §9: a per-channel claim loop using leases
and `SKIP LOCKED`, `LISTEN`/`NOTIFY` wakeup with a 1s poll fallback, `comms_event` writes, and a
single `final_status` update. No gates exist yet at this step — the point is proving queue
mechanics end to end, deliberately a system that always sends (design §14 step 2). Part of the
step-2 ticket family (`family: T-007`; see T-007). Depends on T-011 (ingest, so there is
something in the outbox to claim) and T-012 (the `Sender` trait and first adapter it calls).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
