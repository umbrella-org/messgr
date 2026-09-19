---
id: T-045
title: Customer event feed consumer and nightly reconciliation
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: high
cost: L
---

# T-045 — Customer event feed consumer and nightly reconciliation

## Outcome

After this ships, the customer projection (`customer`, `customer_external_id`,
`customer_address`, `customer_alias`) stays current from the upstream master system's own
event feed instead of only from inbound sends, and a nightly checksum job surfaces drift
between the projection and that source instead of letting it go unnoticed.

## Description

Build-order step 10 (§14): replaces the event-feed stub left by T-015 with the real consumer,
plus the nightly checksum reconciliation named in decision 8 (`14-decisions-and-open-questions.md`)
and `03-data-model.md` §4.6 ("Feed mechanism: event feed ... Batch reconciliation runs nightly
as a safety net against missed events, comparing checksums rather than replaying everything").
Consumes customer created/updated and address added/changed/verified/removed events, applying
them through the same merge machinery provisional-shell reconciliation already uses
(`customer_alias`, per `03-data-model.md` §4.7) rather than a second path.

This is purely a freshness improvement to a cache: `11-failure-modes.md` is explicit that the
ledger, timeline, and OTP are unaffected by the feed being down or stopped — only identity
resolution degrades, and a provisional customer can simply persist longer than it should. No
gate or send-path behaviour depends on this ticket landing.

**Open question this ticket cannot resolve on its own:** Still-open item #5
(`14-decisions-and-open-questions.md`) — which upstream systems appear in
`customer_external_id.system` and which one is canonical for the feed — is a business/upstream
question, not an implementation one. Refinement should surface it to the user rather than guess
at a feed contract.

Soft coupling: T-030 (orphan_event reconciliation, done) is a different reconciliation loop
entirely — it promotes ledger-side delivery-receipt rows into `comms_event`, not
projection-side customer/address rows. No dependency between them, but worth not confusing in
implementation.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 10, remaining gap identified when auditing unticketed steps against the board
