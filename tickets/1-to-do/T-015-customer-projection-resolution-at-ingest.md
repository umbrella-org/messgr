---
id: T-015
title: Customer projection + resolution at ingest
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-015 — Customer projection + resolution at ingest

## Outcome

Ingest resolves any inbound send request (explicit `customer_id`, external id + system, or
address alone) to a `customer_id` and `address_id`, minting a provisional shell when nothing
resolves. Every ledger row from this point on carries a real customer key instead of nothing to
key a DEK or a consent record against.

## Description

Builds the customer projection schema (`customer`, `customer_external_id`,
`customer_address`, `customer_alias` — §4.6) and wires resolution into the ingest path (§4.7):
`customer_id` used directly after alias expansion, external id resolved via
`customer_external_id`, address resolved via `value_hmac` against active addresses, and a
provisional customer + address minted when nothing resolves. Resolution never rejects a send —
a missing timeline entry is an acceptable outcome, a blocked OTP is not (§4.8, though OTP itself
bypasses this path entirely per §3).

This is build-order step 3 (§14), and it has to land before step 4/5's gates because consent
keys on `customer_address.id` (§5) — the gates need the resolution path's `address_id` to exist
first. The event feed consumer that keeps the projection fresh (build-order step 10) is stubbed
for this ticket; the schema and the resolution logic are not. Contact values are encrypted under
the customer DEK from the first write (§7, invariant #7 in AGENTS.md) — depends on T-008's DEK
lifecycle being in place, which is already done.

Out of scope: the event feed consumer itself and nightly reconciliation (step 10), staleness-gated
deferral for transactional/marketing resolution (§4.8 — needs the staleness threshold, an open
question, §"Still open" #2), and customer-split adjudication tooling (§4.7, flagged for a human
rather than automated).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 3 (§14), filed after T-014 (step 2 work) landed.
