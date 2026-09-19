---
id: T-052
title: OTP fast path: sms-sender bypassing the queue
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: high
cost: L
---

# T-052 — OTP fast path: sms-sender bypassing the queue

## Outcome

After this ships, customer login stops depending on the messaging platform at all — a marketing
incident, a full outbox, or a dead dispatcher can no longer lock a customer out of the bank
(AGENTS.md hard invariant #1), because OTP sends go through `sms-sender`, synchronous and
outside the queue/gate chain entirely, while still appearing in messgr's ledger and UI.

## Description

Build-order step 17 (§14), deliberately last among the pre-cloud steps — "because it touches the
auth flow and should not be the thing shaking out bugs in the sender adapters." Builds the
on-prem shape of §3's OTP design (`02-otp.md`):

- `sms-sender`: a thin Rust library (or a dedicated single-purpose HTTP service if the calling
  auth service is not Rust) that talks to the SMS provider synchronously — no queue, no gate
  chain, no dispatcher involvement.
- The audit record write to `comms_request` is asynchronous and best-effort: if Postgres is
  unavailable, the send still succeeds and the record is buffered to local disk and backfilled.
  `payload_ciphertext` is NULL for the auth class (§7.4) — the code is a live credential and must
  never be retained.
- Quiet hours are not evaluated on this path — OTP is exempt by policy (§6, decision-table
  confirmation already reflected in T-043's `quiet_hours_policy`, which OTP never reads).
- Accepted cost of this design, stated in §3: "OTP rate limiting and provider failover are
  duplicated in `sms-sender` rather than centralized" — a deliberate trade against centralizing
  and reintroducing the shared-fate problem this whole design exists to avoid. This ticket
  reuses T-051's `provider_config`-ordering approach for that duplicated failover logic where
  practical, rather than inventing a second failover mechanism.

Out of scope: `otp-api`, the cloud-only network-hop variant of this same mechanism (§3.1) — that
is part of T-054 (cloud enablement, step 19), not this ticket, since on-prem links `sms-sender`
directly into the bank's own auth service with no network hop at all.

Soft coupling: see T-051's Description for the recommended (not yet hard-dependency) sequencing
of SMS failover landing before this ticket.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 17, remaining gap identified when auditing unticketed steps against the board
