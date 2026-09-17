---
id: T-039
title: Dispatcher leader election (HA)
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-039 — Dispatcher leader election (HA)

## Outcome

After this ships, only one dispatcher instance per tenant can ever claim and send messages —
mechanically enforced, not by operational convention — so a second instance started by accident
(bad deploy, stuck rolling restart) cannot double-send.

## Description

Closes the second half of build-order step 7 (§9, DESIGN.md). Retry/backoff (the first half)
shipped in T-021; leader election did not — confirmed directly in code:
`src/bin/dispatcher.rs`'s own doc comment says "leader election (`pg_try_advisory_lock`) is still
a later ticket (build order step 7's other half)," and `src/dispatcher/repo.rs:243-251`
(`clear_stale_leases`'s doc comment) says "exactly one dispatcher instance runs per tenant today
(no leader election yet, T-013 decision 3)."

This is also AGENTS.md hard invariant 9 ("dispatchers bypass the connection pooler") — the
invariant's rationale (session advisory locks and `LISTEN` don't survive transaction pooling,
failure mode is silent double-dispatch) is currently unenforced, resting on the assumption that
nobody ever runs two instances. Per §2.3, the dispatcher already holds a **direct** (non-pooled)
Postgres connection for `LISTEN` — this ticket adds `pg_try_advisory_lock` on that same
connection at startup, holding the lock for the process's lifetime, and exiting/retrying if it
can't acquire it.

Build-order.md's own isolation-suite requirement (line 33) already specifies the acceptance bar:
"Dispatcher leader election holds under a forced failover, over a direct connection, with exactly
one active dispatcher observed throughout (§2.3)" — this is not a new test to invent, it's a test
already committed to that currently has nothing to exercise.

Adding the lock is not the whole change. `repo::clear_stale_leases` (T-021 decision 4) is
currently called once at raw process startup, and its own doc comment says this is "safe because
exactly one dispatcher instance runs per tenant today" — once a second instance can exist, that
is no longer true: a standby starting up while a leader is actively dispatching would wipe leases
out from under it, causing double claims. This ticket moves that sweep to run once, immediately
after a process wins the advisory lock, rather than once at process start — the same reasoning
T-021 used, re-anchored to the moment it's actually still true.

## Implementation Plan

### 0. Feature branch (mandatory)

Before any change, create a feature branch inside the `messgr` repo (root of this repo,
`path = "."` in pickle.toml):

```
git checkout main
git checkout -b feat/T-039-dispatcher-leader-election-ha
```

Do all work on this branch, committing locally as you go. Publish only per the project's
commit policy — no push / no merge request without explicit user approval. Tidy WIP commits
into atomic ones before presenting (root-path child default), then keep that tidied history
(this project's default for a root-path child, tickets/README.md §0) unless the user asks to
squash instead.

### Prerequisite gate (hard)

None. `depends-on: []`. T-013 (minimal dispatcher) and T-021 (lease lifecycle/retry/backoff),
the two tickets this one builds directly on top of, are both in `6-done/` and merged to `main`.

### Confirmed design decisions (do not deviate without asking)

1. **One advisory lock per tenant database, fixed key.** The lock namespace is already
   partitioned per tenant database (§9 — "the lock namespace is naturally partitioned because
   advisory locks are scoped per database"). `pg_try_advisory_lock($1)` uses a single fixed
   `bigint` key (`LEADER_LOCK_KEY = 1`) — no per-tenant or per-channel key derivation, matching
   the design's "one active dispatcher process per tenant, handling all of that tenant's
   channels" (there is no per-channel dispatcher process to give a distinct key to).
2. **The lock is held on a dedicated, non-pooled connection, open for the process's lifetime.**
   Never acquired through `tenant_pool` (§2.3, AGENTS.md hard invariant 9): a connection checked
   out of a `PgPool` returns to the pool and can be handed to unrelated work, silently releasing
   a session-scoped lock. `leader::acquire` opens its own `PgConnection` via
   `db::with_database_name(&config.control_database_url, &tenant.database_name)` — the same
   base-URL-plus-database-name derivation `connect_tenant_pool` already uses internally, so this
   relies on the same existing operational assumption the dispatcher's kill-switch `LISTEN`
   connection already relies on: that `CONTROL_DATABASE_URL`, for this binary's deployment, is a
   direct (non-PgBouncer) connection string. That split is operational (§2.3's service table),
   not code-enforced anywhere in this codebase today, and this ticket does not change that.
3. **Retry every 3 seconds while standby.** §9: "the loser idles and retries every few seconds."
   3s matches that wording and keeps failover (lock release + at most one retry interval) inside
   the single-digit-seconds bar build-order.md's isolation-suite line names.
4. **Everything except the stale-lease sweep and the claim loops runs on both the leader and the
   standby.** The health listener, `tenant_pool`, Vault AppRole login, DEK cache, `tenant_config`
   load, provider credential loads, and the kill-switch refresh loop all start unconditionally,
   exactly as today. A process blocks on `leader::acquire` only immediately before
   `clear_stale_leases` and before spawning the `run_channel_loop` tasks. This is what makes
   §9's "takes over within seconds" true — deferring Vault login and `provider_config` queries
   until after winning would add a network round trip to the failover path for no reason. This
   relies on each dispatcher replica being provisioned its own distinct
   `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID` pair — `keystore.rs`'s own doc comment already calls
   `connect_as_tenant` "a one-shot startup login" (a wrapped secret id is single-use), so two
   replicas sharing one pair would fail the second login outright. This is a deployment
   provisioning concern, not a code change here; called out in the docs update below.
5. **`repo::clear_stale_leases` moves from "called once at raw process startup" to "called once,
   immediately after winning leadership."** Per the Description above: it was only ever safe
   because exactly one instance ran per tenant (T-021 decision 4, T-013 decision 3), and that
   stops being true once a standby can exist. Once a process holds the advisory lock, Postgres
   guarantees the previous holder's session — and therefore the previous leader — is gone, so any
   lease still set at that moment genuinely belongs to a dead run. Same reasoning as T-021,
   re-anchored to lock acquisition instead of process start.
6. **The `Leadership` value is held for the rest of `main`, never dropped before the final
   `for handle in handles { handle.await }` loop.** Dropping it early closes the connection and
   releases the lock out from under an otherwise-healthy leader.

### Tasks

#### Task 1 — `src/dispatcher/leader.rs` (new file)

- `pub const LEADER_LOCK_KEY: i64 = 1;` (decision 1)
- `pub const RETRY_INTERVAL: Duration = Duration::from_secs(3);` (decision 3)
- `pub struct Leadership { _conn: PgConnection }` — an opaque handle whose `Drop` (via the held
  connection closing) is what releases the lock; no `Drop` impl needed beyond the field itself.
- `pub async fn acquire(options: PgConnectOptions) -> Leadership` — loops: opens a fresh
  `PgConnection::connect_with(&options)`, runs
  `sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)").bind(LEADER_LOCK_KEY)` against
  it. `true` returns `Leadership { _conn: conn }` immediately. `false`, or a connection error,
  logs (`tracing::info!` / `tracing::warn!`) and sleeps `RETRY_INTERVAL` before retrying — never
  returns `Err`; a standby retries forever, matching "the loser idles and retries" (§9). A
  connection that can never succeed at all is already covered by every other startup `.expect()`
  in this binary failing first (Vault login, tenant lookup, etc. all happen regardless of
  leadership per decision 4).
- Add `pub mod leader;` to `src/dispatcher/mod.rs`.

#### Task 2 — `src/dispatcher/repo.rs`

Update `clear_stale_leases`'s doc comment (currently lines 243-251) to describe running once
after leadership acquisition rather than at raw process startup; drop the "no leader election
yet" framing (decision 5). No signature or query change — the function itself
(`UPDATE outbox SET leased_until = NULL WHERE leased_until IS NOT NULL`) stays the same, only
*when* it's safe to call changes.

#### Task 3 — `src/bin/dispatcher.rs`

- Remove the early `clear_stale_leases` call currently at lines 118-128 (right after
  `tenant_pool` connects).
- Right before the final `for (channel, ctx) in contexts { ... }` claim-loop-spawning block
  (currently line 298), i.e. after the health listener and kill-switch refresh task are already
  spawned and every context is already built, insert:
  - Derive this tenant's direct connection options:
    `db::with_database_name(&config.control_database_url, &tenant.database_name)`, `.expect(...)`.
  - `let _leadership = messgr::dispatcher::leader::acquire(leader_options).await;` followed by a
    `tracing::info!("messgr-dispatcher: acquired tenant leadership")`.
  - The `clear_stale_leases` call moved from Task 3's first bullet, now here, logging its count
    the same way it does today (decisions 4, 5, 6).
  - `_leadership` stays bound in `main`'s own scope for the rest of the function, so it is never
    dropped before the trailing `for handle in handles { let _ = handle.await; }` loop.
- Update the file's top doc comment (lines 1-15): drop "leader election
  (`pg_try_advisory_lock`) is still a later ticket" and the T-021 lease-sweep caveat that named
  "exactly one instance runs per tenant" as the reason it was safe; describe T-039's actual
  behaviour (advisory-lock leader election, sweep now gated on acquiring it) instead.

#### Task 4 — `tests/dispatcher_leader_election.rs` (new file)

Following `tests/tenancy.rs`'s real-Postgres, no-mocks convention: provision a real test tenant
(reuse the `provision_tenant` / `unique_name` / `drop_test_tenant` helper pattern
`tests/dispatcher.rs` and `tests/tenancy.rs` already use), derive its direct
`sqlx::postgres::PgConnectOptions` via `messgr::db::with_database_name`.

- `two_processes_contend_and_exactly_one_holds_leadership`: `tokio::spawn` two
  `messgr::dispatcher::leader::acquire(options.clone())` calls. Assert the first resolves.
  Assert the second does **not** resolve within `tokio::time::timeout(Duration::from_millis(500),
  ...)` — well under `RETRY_INTERVAL` — while the first is still held. Assert a third,
  independent `SELECT pg_try_advisory_lock(1)` on a fresh connection also returns `false` while
  the first holds it, proving "exactly one" from Postgres's own perspective, not just from the
  two tasks' outcomes.
- `standby_takes_over_within_seconds_of_a_forced_failover`: acquire leadership once, `drop` the
  returned `Leadership` (simulating a crashed leader — its session ends, Postgres releases the
  lock per §9), then assert a second, already-pending `acquire` call on the same options resolves
  within `RETRY_INTERVAL` plus a few seconds of slack — the acceptance bar build-order.md names
  ("holds under a forced failover ... with exactly one active dispatcher observed throughout").

### Acceptance test

```
cargo test --test dispatcher_leader_election
just test
just lint
just docs-check
```

All green. `just test` requires the local stack (Postgres + Vault dev server) already running,
same as every other integration test in `tests/` — no new fixture. Optional manual smoke, not
required for merge: run two `messgr-dispatcher` processes locally against the same tenant, each
with its own `VAULT_WRAPPED_SECRET_ID`; confirm only one logs "acquired tenant leadership" and
dispatches, kill it, confirm the other logs the same line and takes over.

### Docs update (mandatory when user-facing)

`docs/user-manual/dispatcher.adoc`: replace the "One process per tenant; leader election
(`pg_try_advisory_lock`-based HA) is still a later build-order step" line, and T-021's own "this
sweep is revisited once leader election ships" caveat, with a paragraph describing the shipped
behaviour: two processes may run per tenant, `pg_try_advisory_lock` on a direct connection
decides which one is active, a standby retries every 3 seconds, failover completes within
seconds of the leader's process or connection ending, and the stale-lease sweep now runs once
per acquired leadership rather than once at raw process startup. Note the one-shot-wrapped-
secret-id operational requirement from decision 4: each replica needs its own
`VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID` pair, not a shared one.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just lint`, `just docs-check` clean.
2. `docs/user-manual/dispatcher.adoc` updated per the Docs update section above.
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(dispatcher): add leader election for dispatcher HA (T-039)

   Two dispatcher processes may now run per tenant; pg_try_advisory_lock on a
   direct connection elects exactly one as active, with the loser retrying
   every 3s and taking over within seconds of the leader dying. The startup
   stale-lease sweep now runs once per acquired leadership instead of once at
   raw process start, which is what makes it still safe under two instances.
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits before
   presenting (root-path child default).
6. Commit locally on the ticket branch. Publish only per the commit policy — do not push or open
   a merge request without user approval. Present the commit message; only after approval,
   finalize (keep the tidied history, this project's root-path default, unless the user asks to
   squash), verify the remote base is not behind (`git fetch origin main && git diff --name-only
   origin/main...HEAD | grep '^tickets/'` prints nothing), push, and open the merge request.
   Merging is always the human's. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis, next after the gate-chain batch (T-036-T-038) — closes AGENTS.md invariant 9's live
  exposure, already flagged in-code as pending.
- 2026-09-17 — TO DO → READY: implementation plan complete
- 2026-09-17 — TO DO → READY: plan complete
