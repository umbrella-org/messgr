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

A transient provider failure (network timeout, 5xx, connection reset) reschedules its outbox row with backoff and jitter instead of ending it in a terminal `failed` status on the first attempt, up to 8 attempts before it is finally given up on as `failed`. A dispatcher killed mid-lease — or any pre-terminal-write failure that leaves a row leased with nothing to clear it — no longer needs an unbounded wait before another dispatcher instance can pick the row back up: restarting the process reclaims it immediately, proven by a forced-restart test. A transient claim-query error no longer kills that channel's entire dispatch loop for the tenant.

## Description

`src/dispatcher/worker.rs::try_process` currently has no retry path at all: both `SenderError::Provider` and `SenderError::Http` — a permanent provider rejection and a transient network error alike — call `repo::write_terminal` with `final_status = "failed"`, which deletes the outbox row. DESIGN.md §2.4 step 6 (as corrected by this audit) requires "a retryable provider failure ... bumps `attempts` and reschedules `next_attempt_at` with backoff and jitter; the row stays in the outbox" — none of that exists. `outbox.attempts` is incremented at claim time (`repo::claim`'s `UPDATE ... SET attempts = attempts + 1`) but nothing ever reads it back to compute backoff or cap retries.

This also means the outbox's one lease-release path is `write_terminal` deleting the row — there is no lease timeout at all today. **Correction to this ticket's own original framing:** the "2-minute lease timeout" referenced above and in the Outcome is aspirational, not implemented — `repo::claim`'s `WHERE ... AND leased_until IS NULL` never compares `leased_until` to `now()`, so a row that hits any error before a terminal write (a transient DB blip during `load_ciphertexts`, a DEK fetch failure, a decrypt error, or a hard process crash) stays leased **forever**, not just for 2 minutes, until something explicitly writes `leased_until` back to `NULL`. DESIGN.md's corrected §4.2 ("Correction: the claim predicate and the retry path were never reconciled") states the required invariant for the in-process retry case: a retryable failure clears `leased_until` to `NULL` in the same statement that reschedules `next_attempt_at`. It does not, on its own, cover a hard crash (no code runs to clear anything) — DESIGN.md line 205 separately promises "a crashed dispatcher's leases simply expire ... no reaper", which nothing in this codebase implements yet, and which leader election/HA (`pg_try_advisory_lock`, DESIGN.md build order step 7's other half) doesn't exist to make safe in the general multi-instance sense. Given exactly one `messgr-dispatcher` instance runs per tenant today (no HA), this ticket closes that gap the way that fact makes safe: on process startup, before spawning any claim loop, clear every stale lease for this tenant (`UPDATE outbox SET leased_until = NULL WHERE leased_until IS NOT NULL`) — correct because a fresh process start is, by construction, the only claimant that could exist, so any lease still set belongs to a run that is no longer around to finish it.

Separately, `run_channel_loop`'s `repo::claim(...).await.expect("dispatcher: claim query failed")` turns any transient database error (a dropped connection, a brief primary failover) into a permanent panic that ends that channel's task for the tenant — the standby dispatcher never takes over for a panicked task the way it would for a genuinely crashed process, since the process itself keeps running with one fewer channel loop. This needs supervision (retry with backoff, or a restart of just that task) rather than `.expect`.

Scope:

1. Classify `SenderError` variants as retryable vs terminal (a `Provider` rejection with a 4xx-equivalent status is terminal; `Http` transport errors and 5xx-equivalent provider statuses are retryable).
2. On a retryable failure: `UPDATE outbox SET leased_until = NULL, next_attempt_at = $backoff WHERE comms_request_id = $1`, computed with exponential backoff and jitter, capped at a maximum `attempts` beyond which the message becomes terminal (`failed`, exhausted retries) rather than retrying forever.
3. Give `run_channel_loop` a supervised retry around `repo::claim`'s error case instead of `.expect`.
4. Clear every stale lease for this tenant at `messgr-dispatcher` process startup, before any claim loop starts — the fix for the crash/kill-mid-lease case described above, safe under the current single-instance-per-tenant deployment.
5. Fix `tests/dispatcher.rs::failed_send_writes_failed_event_and_final_status_with_no_requeue` — it currently exercises a `500` provider response and asserts it terminal-fails with no requeue; under the new classification (item 1) a `500` is retryable, so this fixture must move to a genuine terminal case (a 4xx-equivalent status) to keep testing what its name says. Add new coverage: a `500`/transport-level failure is asserted to requeue with a cleared lease and a rescheduled `next_attempt_at`; retries are asserted to stop (terminal, exhausted) once the attempt cap is reached; and a forced-restart scenario (simulate a crash by leaving a row leased with no terminal write, then run the startup sweep) proves the row is reclaimed by a fresh claim immediately, not only after some elapsed time.
6. Update `docs/user-manual/dispatcher.adoc`, which currently and accurately states "one attempt per message -- no leader election, no retry/backoff ... those are later build-order steps" — this ticket is that step; the manual needs to describe the new behaviour once it ships, not just drop the caveat.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-021-outbox-lease-lifecycle-and-dispatcher-retry-with-backoff
```

Root-path child (`path = "."`) — tidy WIP commits into atomic ones before presenting (rules §0).

### Prerequisite gate (hard)

None. `depends-on: []`; branch cuts cleanly from `main` at T-020's merge.

### Confirmed design decisions (do not deviate without asking)

1. **`SenderError::is_retryable(&self) -> bool`** (`src/sender/mod.rs`) — `Http(_)` is always
   retryable (transport-level: connection refused, timeout, malformed body); `Provider { status,
   .. }` is retryable iff `status >= 500`, terminal otherwise (a 4xx-equivalent status is a
   permanent rejection — bad destination, bad payload, auth failure with this credential).
   Confirmed with the user during refinement.
2. **Backoff: 30s base, ×2 multiplier, 30-minute cap, 8 attempts before terminal, uniform jitter
   0–50% added on top.** `outbox.attempts` is already bumped by `repo::claim`'s own `UPDATE`
   before `try_process` runs, so on a retryable failure the row's `attempts` value *is* the
   attempt number that just failed. `attempts` sequence 1..7 reschedule (delays 30s, 60s, 2m,
   4m, 8m, 16m, 30m — the 7th and any later one hits the 30-minute cap); `attempts == 8` failing
   is terminal (`failed`, exhausted retries) instead of rescheduling. Confirmed with the user
   during refinement — DESIGN.md specifies "exponential backoff and jitter" (§2.4 step 6, §12.2)
   but no numbers.
3. **`repo::reschedule_retry` writes no `comms_event` row.** DESIGN.md §2.4 step 6 describes a
   retryable failure as bumping `attempts` and rescheding `next_attempt_at`, "the row stays in
   the outbox" — no event write is named, unlike the `sent`/`failed` terminal writes (§4.4).
   Keep the reschedule path to exactly the one `UPDATE` the corrected §4.2 requires:
   `UPDATE outbox SET leased_until = NULL, next_attempt_at = $2 WHERE comms_request_id = $1`.
4. **Startup lease sweep, not a time-based claim predicate.** `messgr-dispatcher`'s `main`
   (`src/bin/dispatcher.rs`) calls a new `repo::clear_stale_leases(pool) -> Result<u64,
   sqlx::Error>` (`UPDATE outbox SET leased_until = NULL WHERE leased_until IS NOT NULL`) once,
   right after connecting the tenant pool and before spawning any `run_channel_loop` task or
   kill-switch refresh task. Safe today because exactly one dispatcher instance runs per tenant
   (no leader election yet, T-013 decision 3, still true) — a fresh process start cannot be
   racing a still-live claimant. Confirmed with the user during refinement in preference to
   reworking `repo::claim`'s predicate to compare `leased_until` against `now()` (which would
   make the documented 2-minute figure real but still leaves a restart waiting out the full
   window) — this can be revisited once leader election (build order step 7's other half) ships
   and a restart is no longer guaranteed to be the sole claimant.
5. **`run_channel_loop`'s claim retry is unbounded, not bounded-then-panic.** Mirrors
   `drain.rs`'s existing precedent for `claim_for_scope` errors (`RETRY_DELAY =
   Duration::from_secs(1)`, log and retry forever) — a `claim` failure blocks nothing but this
   channel's own next iteration, so there is no other row or task an unbounded wait could stall.
   Add a local `const CLAIM_RETRY_DELAY: StdDuration = StdDuration::from_secs(1);` to
   `worker.rs` (same value, not shared — `drain.rs`'s constant is private to that module).
6. **`rand = "0.8"` added as a direct dependency**, matching the version `sqlx-postgres` already
   resolves transitively (`cargo tree -i rand@0.8.7`) — avoids pulling a second `rand` major
   version into the dependency graph for one `gen_range` call.
7. **No new tenant-scoped configuration.** Backoff base/multiplier/cap/max-attempts and the
   claim-retry delay are hardcoded constants in `worker.rs`, matching `LEASE_DURATION`/
   `POLL_INTERVAL`'s own precedent — DESIGN.md names no per-tenant tuning knob for retry timing,
   and adding one now would be speculative.

### Tasks

#### Task 1 — Cargo.toml

Add `rand = "0.8"` to `[dependencies]` (decision 6).

#### Task 2 — `SenderError` classification (`src/sender/mod.rs`)

Add (decision 1):

```rust
impl SenderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            SenderError::Http(_) => true,
            SenderError::Provider { status, .. } => *status >= 500,
        }
    }
}
```

#### Task 3 — `src/dispatcher/repo.rs`: reschedule and startup sweep

Add:

- `pub async fn reschedule_retry(pool: &PgPool, comms_request_id: Uuid, next_attempt_at: DateTime<Utc>) -> Result<(), sqlx::Error>`
  (decision 3) — `UPDATE outbox SET leased_until = NULL, next_attempt_at = $2 WHERE
  comms_request_id = $1`.
- `pub async fn clear_stale_leases(pool: &PgPool) -> Result<u64, sqlx::Error>` (decision 4) —
  `UPDATE outbox SET leased_until = NULL WHERE leased_until IS NOT NULL`, returning
  `.rows_affected()`.

#### Task 4 — `src/dispatcher/worker.rs`: backoff, retry, supervised claim

- Add `const MAX_SEND_ATTEMPTS: i16 = 8;`, `const BACKOFF_BASE: ChronoDuration =
  ChronoDuration::seconds(30);`, `const BACKOFF_MULTIPLIER: i64 = 2;`, `const BACKOFF_CAP:
  ChronoDuration = ChronoDuration::minutes(30);`, `const CLAIM_RETRY_DELAY: StdDuration =
  StdDuration::from_secs(1);` (decisions 2, 5).
- Add `fn backoff_delay(attempts: i16) -> ChronoDuration` — exponent `(attempts - 1).min(6)` (6
  is where `30s * 2^6 = 1920s` already exceeds the 30-minute cap), `delay_ms =
  (BACKOFF_BASE.num_milliseconds() * BACKOFF_MULTIPLIER.pow(exponent as u32))
  .min(BACKOFF_CAP.num_milliseconds())`, jitter `rand::thread_rng().gen_range(0..=delay_ms /
  2)`, return `ChronoDuration::milliseconds(delay_ms + jitter)`.
- Rewrite `try_process`'s `match ctx.sender.send(...)` (currently three arms: `Ok`,
  `Err(SenderError::Provider { .. })`, `Err(SenderError::Http(_))`) into:
  - `Ok(outcome)` — unchanged (`write_terminal(..., "sent", ...)`).
  - `Err(err) if err.is_retryable() && row.attempts < MAX_SEND_ATTEMPTS` —
    `repo::reschedule_retry(&ctx.pool, row.comms_request_id, Utc::now() +
    backoff_delay(row.attempts)).await?`.
  - `Err(err)` (everything else: non-retryable, or retryable but `attempts >=
    MAX_SEND_ATTEMPTS`) — `write_terminal(..., "failed", None, provider_status_of(&err),
    "failed")`, where `provider_status_of` extracts `Some(status.to_string())` for
    `SenderError::Provider` and `None` for `SenderError::Http`, matching the existing two arms'
    behaviour.
- In `run_channel_loop`, replace `repo::claim(...).await.expect("dispatcher: claim query
  failed")` with a loop that retries on `Err` (decision 5):
  ```rust
  let claimed = loop {
      match repo::claim(&ctx.pool, &channel, CLAIM_BATCH_SIZE, leased_until, &exclusion).await {
          Ok(rows) => break rows,
          Err(err) => {
              tracing::error!(%channel, %err, "dispatcher: claim query failed, retrying");
              tokio::time::sleep(CLAIM_RETRY_DELAY).await;
          }
      }
  };
  ```

#### Task 5 — `src/bin/dispatcher.rs`: startup sweep

Right after connecting `tenant_pool` and before building any `DispatcherContext` or spawning any
task, call `repo::clear_stale_leases(&tenant_pool)`, `.expect("clearing stale outbox leases
failed")`, and log the count at `tracing::info!` (decision 4).

#### Task 6 — Tests (`tests/dispatcher.rs`)

- Fix `failed_send_writes_failed_event_and_final_status_with_no_requeue`: change the mock's
  `ResponseTemplate::new(500)` to `ResponseTemplate::new(400)` (a 4xx-equivalent, terminal under
  decision 1) so the existing assertions (`final_status = "failed"`, one `failed` event, outbox
  row gone) still hold; update the event assertion's expected `provider_status` from
  `Some("500")` to `Some("400")`.
- Add `transient_provider_failure_requeues_with_cleared_lease_and_backoff`: mock a `500`, call
  `try_process`, assert `comms_request.final_status` is still `NULL` (not finalized), no
  `comms_event` row exists, and the `outbox` row has `leased_until IS NULL` and `next_attempt_at
  > now()`.
- Add `transient_http_failure_requeues`: same shape using an unreachable base URL (`HttpSender`
  transport error) instead of a mocked status, asserting the same requeue outcome.
- Add `retries_exhausted_after_max_attempts_terminal_fails`: write a ready row, manually set
  `outbox.attempts = 8` (simulating the 8th claim), mock a `500`, call `try_process`, assert it
  terminal-fails (`final_status = "failed"`, `failed` event present, outbox row gone) rather than
  rescheduling.
- Add `startup_sweep_reclaims_a_lease_left_by_a_simulated_crash`: write a ready row, claim it
  (`repo::claim`, leaving it leased with no terminal write — simulating a crash mid-`try_process`
  before this test does anything else with it), call `repo::clear_stale_leases`, then call
  `repo::claim` again and assert the same row comes back — proving the row is claimable
  immediately, with no wait.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including the new tests/dispatcher.rs cases
```

Manual walkthrough (extends T-013's own): provision a tenant, start a local mock provider that
always returns `500`, run `messgr-dispatcher` against it, submit one transactional message via
`messgr-ingest`, and confirm via `psql` that `outbox.attempts` climbs and `next_attempt_at` moves
forward on each poll interval rather than the row disappearing after the first failed send; kill
the dispatcher process, restart it, and confirm (via the startup log line from Task 5) that any
row still showing `leased_until` set is swept and reclaimed on the very next claim rather than
staying stuck.

### Docs update (mandatory when user-facing)

`docs/user-manual/dispatcher.adoc` — replace the "one attempt per message -- no leader election,
no retry/backoff ... those are later build-order steps" sentence with a description of the
shipped behaviour: retryable failures (`5xx`/transport) requeue with exponential backoff and
jitter up to 8 attempts before terminal-failing; a `4xx`-equivalent provider rejection still
fails on the first attempt; a killed-and-restarted dispatcher reclaims any stale lease
immediately on startup. Note leader election/HA is still not implemented (that half of build
order step 7 remains open).

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. Docs updated per the docs step above.
3. Write a summary: files touched, decisions honoured (1–7 above), anything deferred (leader
   election/HA, per-provider circuit breakers — both still open per DESIGN.md build order step
   7 and §9's "still not solved" framing).
4. Suggested Conventional Commit message:

   ```
   feat(dispatcher): retry transient send failures with backoff, fix lease reclaim (T-021)

   A retryable send failure (5xx-equivalent provider status, or a transport
   error) now clears the outbox row's lease and reschedules next_attempt_at
   with exponential backoff and jitter, up to 8 attempts, instead of
   terminal-failing on the first try. A 4xx-equivalent provider rejection
   still fails immediately. A transient repo::claim error retries instead of
   panicking the channel's task. messgr-dispatcher also sweeps stale leases
   at startup, so a killed-and-restarted process reclaims rows immediately
   rather than leaving them stuck (the claim query never actually compared
   leased_until to now(), so this was previously an unbounded wait).
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (sender classification / repo + backoff / worker retry / startup sweep / tests
   / docs is a natural split) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify `git fetch
   origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints nothing,
   push, and open the merge request. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found try_process has no retry path at all (every SenderError variant is written terminal) and run_channel_loop panics its whole channel task on a transient claim-query error.
- 2026-09-03 — TO DO → READY: plan complete
- 2026-09-03 — READY → IN DEVELOPMENT: picked up
