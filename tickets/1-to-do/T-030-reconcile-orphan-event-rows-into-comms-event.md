---
id: T-030
title: Reconcile orphan_event rows into comms_event
project: messgr
depends-on: []
spawned-by: [T-022]
impact: low
complexity: low
cost: S
---

# T-030 — Reconcile orphan_event rows into comms_event

## Outcome

A background reconciliation job periodically retries `orphan_event` rows (delivery-receipt
webhooks that arrived before, or without, a matching `comms_request`), promoting each match into
a real `comms_event` row and aging out ones that never match. `orphan_event` stops being a schema
with zero readers.

## Description

T-022 (DESIGN.md §4.4/§10, `development/design/09-delivery-receipts.md`) shipped the
`orphan_event` table schema-only, by design (decision 5): "a receipt for an unknown
`provider_ref` goes to a small `orphan_event` table and is reconciled on a short delay rather
than discarded" — but nothing yet reads or writes to it outside the ticket's own round-trip test,
and no ticket owned building the reconciliation job itself. Spawned during T-022's review (finding
F4, the same class of gap T-022 itself found and filed as T-029 for the idempotency-sweep job) so
it doesn't drift a third time.

Scope: a job (matching T-029/T-014's polling-job shape) that periodically re-attempts matching
each `orphan_event.provider_ref` against `comms_request` rows written since, on a short delay
per the design note above; on match, writes the equivalent `comms_event` row via the existing
`write_terminal`-style path and removes (or marks resolved) the `orphan_event` row; rows that
exceed `reconcile_attempts`'s intended bound (schema already has the column) age out — DESIGN.md
does not yet specify the cap or the final disposition of a permanently-orphaned row, which
refinement must pin down with the user before this can go READY.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-04 — created (TO DO). source: review: T-022's review (finding F4) found `orphan_event`'s reconciliation job named in design but never ticketed, unlike the parallel idempotency-sweep gap T-022 itself filed as T-029 — filed here rather than left to drift a third time.
