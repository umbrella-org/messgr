---
id: T-034
title: Validate orphan-reconcile event_type before promoting and make find_match deterministic
project: messgr
depends-on: []
spawned-by: [T-030]
impact: medium
complexity: low
cost: S
---

# T-034 — Validate orphan-reconcile event_type before promoting and make find_match deterministic

## Outcome

`orphan_reconcile` refuses to promote a row whose `event_type` isn't a recognized status/event
value instead of writing it straight into `comms_request.final_status`, and `find_match`'s row
choice is deterministic (and tested) when more than one `comms_event` row shares a
`provider_ref`.

## Description

T-030's review (findings F4, F5) found two related gaps in `src/orphan_reconcile`'s
matching/promotion path, both surfaced because this is the first codepath where a value that
can originate from third-party (eventually `messgr-webhook`) input reaches
`comms_request.final_status` without validation:

- **F4 (event_type validation).** `reconcile::should_advance`'s `None => true` arm advances
  unconditionally regardless of what `new_event_type` actually is, and `repo::promote` then
  binds `orphan.event_type` directly into `comms_request.final_status`. Neither
  `comms_event.event_type` nor `orphan_event.event_type` has a database `CHECK` constraint
  (`migrations/tenant/0004_ledger_outbox_schema.sql` only documents the valid set in a
  comment). `dispatcher::repo::write_terminal` has the same latitude today, but that path's
  `final_status` argument is dispatcher-internal, never third-party-controlled — this ticket's
  gap is specific to `orphan_reconcile` being the first path where it can be.
- **F5 (nondeterministic match).** `repo::find_match`'s `WHERE ce.provider_ref = $1 LIMIT 1`
  has no `ORDER BY`, so its row choice is undefined when multiple `comms_event` rows
  legitimately share a `provider_ref` (e.g. `sent` and `delivered` on the same request). It
  happens to resolve to the same `comms_request_id` either way today, but that is untested and
  unstated.

Scope: validate `orphan.event_type` against the documented `comms_event.event_type` set before
promoting (treat an unrecognized value as a non-match, aged out through the existing cap path
rather than promoted) and add `ORDER BY occurred_at DESC` to `find_match` plus a test pinning
the multi-match behavior. Soft coupling: touches the same module as T-033
(`src/orphan_reconcile`), filed separately because the two are independently schedulable.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F4, F5) found orphan_reconcile promotes an unvalidated event_type into comms_request.final_status and find_match's row choice is nondeterministic when provider_ref is shared — batched into one follow-up ticket.
