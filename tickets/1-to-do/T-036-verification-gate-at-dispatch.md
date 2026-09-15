---
id: T-036
title: Verification gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-036 — Verification gate at dispatch

## Outcome

After this ships, a message no longer dispatches to an unverified address unnoticed: the
dispatcher checks `customer_address.verified_at` at send time and, per the tenant's
`verification_mode`, either blocks it (`enforce`, terminal `unverified_address` event) or lets it
through while recording and counting the outcome (`observe`).

## Description

Wires the verification gate from DESIGN.md §5 into the dispatcher's send path — the first of the
three regulatory gates (verification, consent [T-037], suppression [T-038]) named in build-order
step 5, none of which are enforced yet despite steps 0-4 and later hardening tickets being done.

Schema already exists: `customer_address.verified_at` (migration 0008) and
`tenant_config.verification_mode` (`enforce`|`observe`, migration 0002, default `observe`) — this
ticket is enforcement wiring, not new schema. Rule (§5): for `transactional` and `marketing`
class messages, `verified_at` must be set; `enforce` blocks with terminal event `unverified_address`
on `comms_event`, `observe` allows the send through but still records the outcome and exposes a
count (§5's own reasoning: a gate whose input might always be NULL must never silently no-op).
Auth class skips this gate entirely (§5, AGENTS.md hard invariant 1) — must not touch the OTP
path, which does not go through the dispatcher.

Runs after kill switch and quota gates and before consent/suppression, per §5's ordering table
(cheapest/most terminal-first). Soft coupling: shares the same dispatcher gate-chain evaluation
point that T-037 and T-038 will land in — sequencing among the three is left to pickup order
(WIP limit is 1 anyway), not a hard dependency, since each is independently observable once
built.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — step 5 (the gate chain) is unbuilt despite steps 0-4 and 17 later hardening tickets
  being done; split into three independently-schedulable gates (this one, T-037, T-038).
