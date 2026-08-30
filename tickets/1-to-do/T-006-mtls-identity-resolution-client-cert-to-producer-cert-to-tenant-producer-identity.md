---
id: T-006
title: mTLS identity resolution: client cert to producer_cert to tenant/producer identity
project: messgr
depends-on: [T-005]
spawned-by: []
family: T-005
impact: medium
complexity: medium
cost: M
---

# T-006 — mTLS identity resolution: client cert to producer_cert to tenant/producer identity

## Outcome

Every request into any ingest path carries a resolved `(tenant_id, producer_id)` that the
caller cannot assert about itself: a client certificate is matched against `producer_cert` in
the control database, an unknown or disabled producer is rejected at the edge before any
handler runs, and a resolved identity is available as a shared layer for every later ingest
ticket to consume. In dev, an internal PKI issues the certificates that exercise this path
without a real CA.

## Description

Reads what T-005 writes. T-005 shipped both halves of producer identity — the tenant-side
`producer` row and the control-database `producer_cert` mapping — but nothing yet consumes
`producer_cert` at request time; this ticket is that first reader (§4.9, §11.1, §2.2).

Resolution: client cert (CN/SAN) → look up `cert_subject` in `producer_cert` (control DB) →
`(tenant_id, producer_id)`. Unknown cert and known-but-`enabled = false` producer must be
distinguishable in the rejection (T-005 kept the `producer_cert` row on disable specifically so
this ticket could tell those two apart — T-005 decision 5). Built as a shared layer so every
ingest binary composes it rather than re-implementing cert parsing.

Producer identity is never read from the request body (§11, §4.9) — this ticket is where that
rule is enforced; every later ingest ticket inherits it rather than re-asserting it.

Also in scope: internal PKI issuance for dev, so the resolution path has real certificates to
test against without standing up an external CA.

Same family as T-005 (build step 1, "Producer registry and mTLS identity", §4.9/§11.1) — T-005
is the write side, this is the read side; together they are the step's whole outcome.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-30 — created (TO DO). source: chat: filed from PLAN.md build-step-1 row; same family as T-005 (producer registry write side / mTLS resolution read side)
