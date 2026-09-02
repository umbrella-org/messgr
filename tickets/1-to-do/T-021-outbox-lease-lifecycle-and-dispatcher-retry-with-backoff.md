---
id: T-021
title: Outbox lease lifecycle and dispatcher retry with backoff
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: high
cost: L
---

# T-021 — Outbox lease lifecycle and dispatcher retry with backoff

## Outcome

A transient provider failure (network timeout, 5xx, connection reset) reschedules its outbox row with backoff and jitter instead of ending it in a terminal `failed` status on the first attempt. A dispatcher killed mid-lease no longer needs the fixed 2-minute lease timeout to expire before another dispatcher can pick the row back up on restart — a forced-restart test proves this. A transient claim-query error no longer kills that channel's entire dispatch loop for the tenant.

## Description

`src/dispatcher/worker.rs::try_process` currently has no retry path at all: both `SenderError::Provider` and `SenderError::Http` — a permanent provider rejection and a transient network error alike — call `repo::write_terminal` with `final_status = "failed"`, which deletes the outbox row. DESIGN.md §2.4 step 6 (as corrected by this audit) requires "a retryable provider failure ... bumps `attempts` and reschedules `next_attempt_at` with backoff and jitter; the row stays in the outbox" — none of that exists. `outbox.attempts` is incremented at claim time (`repo::claim`'s `UPDATE ... SET attempts = attempts + 1`) but nothing ever reads it back to compute backoff or cap retries.

This also means the outbox's one lease-release path is `write_terminal` deleting the row, or the lease's own 2-minute timeout. A row that hits any error before a terminal write — a transient DB blip during `load_ciphertexts`, a DEK fetch failure, a decrypt error — is left leased with no path back to claimable except waiting out the timeout, every time, regardless of what actually happened. DESIGN.md's corrected §4.2 ("Correction: the claim predicate and the retry path were never reconciled") states the required invariant: a retryable failure clears `leased_until` to `NULL` in the same statement that reschedules `next_attempt_at`.

Separately, `run_channel_loop`'s `repo::claim(...).await.expect("dispatcher: claim query failed")` turns any transient database error (a dropped connection, a brief primary failover) into a permanent panic that ends that channel's task for the tenant — the standby dispatcher never takes over for a panicked task the way it would for a genuinely crashed process, since the process itself keeps running with one fewer channel loop. This needs supervision (retry with backoff, or a restart of just that task) rather than `.expect`.

Scope:

1. Classify `SenderError` variants as retryable vs terminal (a `Provider` rejection with a 4xx-equivalent status is terminal; `Http` transport errors and 5xx-equivalent provider statuses are retryable).
2. On a retryable failure: `UPDATE outbox SET leased_until = NULL, next_attempt_at = $backoff WHERE comms_request_id = $1`, computed with exponential backoff and jitter, capped at a maximum `attempts` beyond which the message becomes terminal (`failed`, exhausted retries) rather than retrying forever.
3. Give `run_channel_loop` a supervised retry around `repo::claim`'s error case instead of `.expect`.
4. Invert `tests/dispatcher.rs::failed_send_writes_failed_event_and_final_status_with_no_requeue` (or add its sibling) so a transient `SenderError::Http` is asserted to requeue with a cleared lease and a rescheduled `next_attempt_at`, and a forced-restart test proves a dispatcher killed mid-lease is reclaimed by a fresh claim after restart rather than only after the lease timeout.
5. Update `docs/user-manual/dispatcher.adoc`, which currently and accurately states "one attempt per message -- no leader election, no retry/backoff ... those are later build-order steps" — this ticket is that step; the manual needs to describe the new behaviour once it ships, not just drop the caveat.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found try_process has no retry path at all (every SenderError variant is written terminal) and run_channel_loop panics its whole channel task on a transient claim-query error.
