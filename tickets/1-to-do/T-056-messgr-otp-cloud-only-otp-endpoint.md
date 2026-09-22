---
id: T-056
title: messgr-otp: cloud-only OTP endpoint
project: messgr
depends-on: []
spawned-by: [T-054]
impact: critical
complexity: medium
cost: M
---

# T-056 — messgr-otp: cloud-only OTP endpoint

## Outcome

After this ships, a cloud tenant's staff can complete OTP without linking a Rust library:
`messgr-otp` gives them a dedicated, mTLS-authenticated network endpoint per region that answers
synchronously, with an async best-effort audit record, exactly matching the on-prem library's
observable behaviour and latency profile.

## Description

The cloud-only variant of T-052's OTP mechanism (`01-overview-architecture.md`/`12-deployment.md`
§3.1): a dedicated minimal endpoint per region, synchronous, mTLS-authenticated, no
queue/gate-chain/Postgres write on the request path — it reuses T-052's `sms-sender`
audit-write/Vault-caching pattern rather than inventing a second one (T-052 is done and merged,
PR #65, `8182cf7`).

Fetches its provider credential from Vault **at startup**, holds it in memory for the process
lifetime, refreshed on a background timer — not per-request — per the correction already on
record in §3.1 (an earlier draft implied a per-request Vault call with no stated behaviour for a
sealed Vault; fixed to match §7.6's DEK-cache discipline).

Resolves design-doc still-open item #10 (cloud OTP posture) as: build it, per user confirmation
during T-054's refinement — cloud tenants get a hosted endpoint rather than being steered to an
on-prem auth pattern.

Soft coupling: shares the region concept with T-060 (second region) — `messgr-otp` must be
deployable per-region from the start, but does not depend on T-060 landing first; a single-region
deployment is a valid intermediate state.

Hard invariant #1 (`AGENTS.md`) applies in full here: this endpoint is synchronous and bypasses
the gate chain/queue entirely, same as on-prem OTP — a marketing incident must not be able to
stop cloud customers logging in either.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed cloud OTP posture (design-doc still-open item #12: build messgr-otp,
  do not defer it in favour of an on-prem-only recommendation)
