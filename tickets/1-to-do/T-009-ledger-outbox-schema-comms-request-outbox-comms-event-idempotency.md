---
id: T-009
title: Ledger + outbox schema: comms_request, outbox, comms_event, idempotency
project: messgr
depends-on: [T-005, T-007]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-009 — Ledger + outbox schema: comms_request, outbox, comms_event, idempotency

## Outcome

After this ships, the core data model of the system exists: a monthly-partitioned
`comms_request` ledger, an `outbox` a dispatcher can claim from, a `comms_event` append log,
and an `idempotency` table, all with the indexes the design specifies — the foundation every
later gate, dispatcher, and query surface builds on.

## Description

Build the ledger + outbox schema exactly as specified in design §4.1–§4.4: `comms_request` as
monthly RANGE partitions, `outbox`, `comms_event`, `idempotency`, and their indexes. The ledger
is self-contained by design (§4.1) — `customer_id` and the destination live on every row, and
the customer timeline must never join the customer projection later. Part of the step-2 ticket
family (`family: T-007`; see T-007). Depends on T-005 (producer registry, already merged) for
the FK target and T-007 (tenant_config) for retention/timezone defaults the partition and
schedule columns need.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
