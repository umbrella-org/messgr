---
id: T-041
title: Cancellation endpoint with dispatch-time race check
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: low-medium
cost: S-M
---

# T-041 — Cancellation endpoint with dispatch-time race check

## Outcome

After this ships, a producer can cancel a not-yet-sent message via `DELETE /comms/{id}`; if the
dispatcher already holds the lease and is about to send, the cancel loses and the caller gets
`409 Already Sent` instead of a false "cancelled" success.

## Description

Closes the cancellation half of build-order step 9 (§6.2: "Cancellation is mandatory, not
optional. Anything schedulable must be cancellable"). Verified in code: `outbox.cancelled_at`
exists as a column (`src/dispatcher/model.rs`) and is carried through every SELECT/INSERT
(`src/dispatcher/repo.rs`, `src/ingest/repo.rs`), but nothing in the codebase ever sets it to a
non-NULL value, and nothing branches on it — there is no cancel endpoint on `messgr-ingest`
(`src/bin/ingest.rs` registers only `POST /comms`), and the dispatcher's send path does not
re-check the column before calling the provider.

Per §6.2's exact mechanism: "`DELETE /comms/{id}` sets `outbox.cancelled_at`. The race is real —
a cancel can arrive while the dispatcher holds the lease — so the dispatcher **re-reads
`cancelled_at` immediately before the provider call**, and the API returns `409 Already Sent`
when it lost. Reporting a cancellation that did not happen is worse than failing to cancel." Both
halves (the endpoint and the dispatcher-side race check) are needed together — shipping the
endpoint alone would let a cancel silently lose the race and still report success.

Soft coupling: shares the outbox row and ingest/dispatcher code paths touched by T-040 (scheduled
delivery/expiry wiring) — no hard dependency, since cancellation of an immediately-claimable
message is meaningful on its own, but refining both together may be more efficient than
sequencing them if picked up close together.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — `outbox.cancelled_at` exists but has no write path and is never checked before
  send, contradicting §6.2's "cancellation is mandatory" requirement.
