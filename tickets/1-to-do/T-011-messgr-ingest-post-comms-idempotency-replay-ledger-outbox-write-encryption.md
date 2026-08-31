---
id: T-011
title: messgr-ingest: POST /comms, idempotency replay, ledger + outbox write, encryption
project: messgr
depends-on: [T-006, T-008, T-009, T-010]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-011 — messgr-ingest: POST /comms, idempotency replay, ledger + outbox write, encryption

## Outcome

After this ships, a registered producer can `POST /comms` and get back a ledger row: the
request is recorded and the outbox row written in the same transaction, payload and destination
are encrypted under a per-customer DEK, and a repeated idempotency key replays instead of
double-sending.

## Description

Build `messgr-ingest`: `POST /comms`, idempotency replay, a single-transaction ledger + outbox
write, and payload/destination encryption + HMAC (design §4.1–§4.3, §7, §11). Producer identity
comes from the mTLS layer built in T-006, never from the request body (§11, §4.9) — that rule is
enforced here and every later ingest ticket inherits it. Part of the step-2 ticket family
(`family: T-007`; see T-007). Depends on T-006 (identity resolution, merged), T-008 (DEK
lifecycle), T-009 (ledger/outbox schema), and T-010 (template store).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
