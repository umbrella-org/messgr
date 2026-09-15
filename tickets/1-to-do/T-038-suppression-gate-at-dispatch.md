---
id: T-038
title: Suppression gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-038 — Suppression gate at dispatch

## Outcome

After this ships, a destination on the suppression list (hard bounce, complaint, regulatory hold)
no longer receives any further send: the dispatcher checks `destination_hmac` against a
suppression list at send time and blocks with a terminal `suppressed_list` event.

## Description

Third of the three regulatory gates from DESIGN.md §5 (build-order step 5; see T-036 for the
family context). **No schema exists yet** — needs a new `suppression` table keyed on
`destination_hmac` (deliberately the raw address hash, not `address_id` or `customer_id` — §5:
suppression is fail-safe, so over-suppressing a recycled number is the acceptable direction to
err). Entries carry a review date rather than living forever (§5) — the schema should include
that from the start rather than retrofitting it. Applies to all classes per §5's table; unlike
consent/verification there is no auth exemption named in §5, so confirm that reading during
refinement rather than assuming symmetry with the other two gates.

**Scope note — same population gap as T-037.** §5 lists the real sources as hard bounce,
complaint, and regulatory hold — the first two normally arrive via the webhook/delivery-receipt
path (build-order step 12, unbuilt). This ticket needs a minimal manual/CLI way to add and expire
entries (e.g. `messgr-control suppression add/list`, mirroring the existing config-CLI pattern);
wiring automatic population from delivery receipts is out of scope and belongs with step 12.

Runs after verification (T-036) and consent (T-037), per §5's ordering table. No hard dependency
on either — independently observable and buildable; WIP=1 serializes pickup anyway.

Migration note for whoever refines this: `migrations/tenant/0004_ledger_outbox_schema.sql`'s
header comment says "suppression (T-021)" — that referred to a planned ticket number from when
T-009 was written; T-021 ended up being a different, unrelated ticket ("Outbox lease lifecycle
and dispatcher retry with backoff"). That comment is stale and should be corrected (to point at
this ticket, or removed) as part of the schema change here.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed alongside T-036/T-037 from a
  build-order-vs-shipped-tickets gap analysis — step 5 (the gate chain) is unbuilt despite steps
  0-4 and 17 later hardening tickets being done.
