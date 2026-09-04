---
id: T-029
title: Nightly idempotency-sweep job
project: messgr
depends-on: []
spawned-by: [T-022]
impact: low
complexity: low
cost: S
---

# T-029 — Nightly idempotency-sweep job

## Outcome

After this ships, the `idempotency` table is actually bounded at the 30-day retention DESIGN.md
§4.3 already promises ("retained 30 days, swept nightly"), instead of growing forever.

## Description

`idempotency` (§4.3) has never had its sweep built. T-009 shipped the table and T-011 shipped
the write path, and both deferred the nightly sweep without either claiming it — it has sat
unowned since. Every row currently lives forever; nothing deletes an expired one. This is
narrow, bounded hygiene work, not a design question: a scheduled job (or a `messgr-control`
subcommand invoked by an external cron, matching this project's existing operational pattern —
see `partition-lifecycle run`) that runs `DELETE FROM idempotency WHERE expires_at < now()` per
tenant, on a nightly cadence. No gate-chain, consent, or encryption surface is touched — the
table holds no PII (the key and `comms_request_id` are opaque; the request payload itself lives
in `comms_request`).

T-022 rescopes `idempotency`'s primary key to `(producer_id, key)` — this ticket's sweep query
is unaffected either way, since it deletes on `expires_at` alone.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-04 — created (TO DO). source: field-use: spawned while refining T-022, which named this deferred, unowned sweep job (deferred by T-009 and T-011, neither claiming it) as a follow-up to file if no ticket already existed.
