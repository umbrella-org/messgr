---
id: T-031
title: TenantRegistry eviction: idle-TTL sweep with poll-loop cancellation
project: messgr
depends-on: []
spawned-by: [T-025]
impact: medium
complexity: medium
cost: M
---

# T-031 — TenantRegistry eviction: idle-TTL sweep with poll-loop cancellation

## Outcome

`messgr-ingest`'s `TenantRegistry` stops growing forever: a tenant context idle past a fixed TTL
is evicted, its pool closed, and its background kill-switch poll loop actually stops running
(not just orphaned) — so a suspended/offboarded/rarely-seen tenant no longer holds an open pool
and a live task indefinitely.

## Description

Split out of T-025's item 7 at refinement: what looked like "add an LRU/TTL bound to a
`HashMap`" turns out to require more, because `TenantRegistry::get_or_open`
(`src/tenant/registry.rs:177-186`) spawns a `tokio::spawn`'d kill-switch poll loop
(`kill_switch::cache::run_refresh_loop`) per tenant context the first time it's opened, and that
loop has no exit condition — it's `loop { refresh; sleep-or-listen }` forever
(`src/kill_switch/cache.rs:158-181`). Removing a `TenantContext` from the registry's `HashMap`
alone would not stop that loop: it holds its own clone of the tenant's `PgPool` and keeps polling
it forever, so "eviction" would silently leak exactly the resources it was meant to free.

Scope:

1. Add a cancellation signal to `run_refresh_loop` — a `tokio::sync::Notify` (already available
   via `tokio`, no new dependency) raced in a third `tokio::select!` branch alongside the
   existing listener/sleep branches. `messgr-dispatcher`'s own call site
   (`src/bin/dispatcher.rs:219`) passes `None`/never cancels, unchanged from today (a dispatcher
   is one-tenant-per-process for its own lifetime — it doesn't need per-tenant eviction).
2. `TenantRegistry` tracks each context's last-access time and holds the cancellation handle
   alongside it. A periodic sweep (spawned once, e.g. from `TenantRegistry::new()` or an explicit
   `start_eviction_sweep` the binary calls) removes entries idle past a fixed TTL, signaling
   cancellation before dropping the entry so the poll loop actually exits and the pool actually
   closes.
3. **Idle-TTL only, not `tenant.status`-based** — T-025's original text already noted
   status-transition eviction needs suspension-checking machinery that doesn't exist yet; that
   stays out of scope here too, for the same reason.
4. Not in scope: anything about `messgr-dispatcher`'s own lifecycle (it isn't a multi-tenant
   cache) or about `KillSwitchCache` itself (only the loop that refreshes it).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-031-tenant-registry-idle-eviction
```

WIP commits locally as you go. Publish only per the project's commit policy (no push/MR
without user approval; `path = "."` means tidy WIP into atomic commits before presenting).

### Prerequisite gate (hard)

None. `depends-on: []` — `KillSwitchCache`/`run_refresh_loop` and `TenantRegistry` both already
exist and ship in `main`.

### Confirmed design decisions (do not deviate without asking)

1. **Idle TTL is a hardcoded constant, 1 hour, not env-configurable.** No number is given
   anywhere in DESIGN.md or the ticket; this matches this same file's own precedent
   (`DEK_CACHE_TTL` = 3600s) rather than introducing a new tunable. Confirmed with the user
   during refinement.
2. **Sweep interval is a hardcoded constant, 5 minutes.** Not specified anywhere; picked as a
   fraction of the TTL so staleness is bounded to at most `TTL + one interval` without adding
   sweep overhead on the order of the kill-switch poll itself. Same "no number given, pick a
   reasonable default" treatment `KILL_SWITCH_POLL_INTERVAL` already got.
3. **The sweep is started explicitly by the binary (`start_eviction_sweep`), never auto-spawned
   from `TenantRegistry::new()`.** Mirrors this codebase's existing convention:
   `KillSwitchCache::new()` spawns nothing either — `run_refresh_loop` is always spawned
   explicitly by the caller (`registry.rs`'s own `get_or_open`, `dispatcher.rs:219`). Keeping
   `new()` side-effect-free also means the existing test fixtures that call
   `TenantRegistry::new()` (`tests/kill_switch.rs:612`, `tests/ingest.rs:147`) do not
   unexpectedly gain a background task running production timings.
4. **Cancellation is a new `Option<Arc<tokio::sync::Notify>>` parameter on `run_refresh_loop`,
   not a new method or a second loop variant.** One shared loop body for both callers, per this
   function's own existing doc comment style (dispatcher passes `Some(listener)` /
   `messgr-ingest` passes `None` for the listener already — cancel follows the same optional-arg
   shape). `messgr-dispatcher`'s call site (`src/bin/dispatcher.rs:219`) passes `None` and is
   otherwise unchanged — a dispatcher is one-tenant-per-process for its own lifetime (§2.3) and
   never evicts.
5. **Eviction awaits the cancelled task's `JoinHandle` before `evict_idle` returns.** This is
   what turns "signal cancellation and hope" into a check that can fail: if the `select!` branch
   wiring is wrong and the loop never exits, `evict_idle` (and the acceptance test's
   `tokio::time::timeout` around it) hangs/times out instead of silently reporting eviction as
   done. Matches the review addendum's "an assertion must be able to fail" rule (Step 3).
6. **Last-access is tracked per-entry with a `std::sync::Mutex<Instant>`, not a coarser
   registry-wide lock.** `get_or_open`'s cache-hit path (the hot path) only takes the outer
   `contexts` `RwLock` in read mode (`registry.rs:135`); touching last-access on every hit must
   not force a write-lock upgrade, so it needs its own fine-grained, independently-lockable
   field per entry — the same reasoning `KeyCache` already applies with its own `std::sync::Mutex`
   around the LRU map.

### Tasks

#### Task 1 — add cancellation to `run_refresh_loop` (`src/kill_switch/cache.rs`)

- Add `use tokio::sync::Notify;`.
- Change the signature (lines 158-164) to add a trailing parameter:
  `cancel: Option<Arc<Notify>>`.
- Restructure the loop body (lines 165-181) so cancellation races the existing listener/sleep
  branches as a third arm, checked once per iteration after each refresh:

  ```rust
  loop {
      match cache.refresh(&pool).await {
          Ok(delta) => on_delta(delta),
          Err(err) => tracing::error!(%err, "kill_switch cache refresh failed"),
      }

      let sleep = tokio::time::sleep(poll_interval);
      let cancelled = async {
          match &cancel {
              Some(notify) => notify.notified().await,
              None => std::future::pending().await,
          }
      };

      match &mut listener {
          Some(listener) => tokio::select! {
              _ = listener.recv() => {}
              _ = sleep => {}
              _ = cancelled => break,
          },
          None => tokio::select! {
              _ = sleep => {}
              _ = cancelled => break,
          },
      }
  }
  ```

- Update the doc comment above `run_refresh_loop` (lines 152-157) to mention the new `cancel`
  parameter and that `messgr-dispatcher` passes `None` for it, same as it already does for
  `listener`.
- Update the two call sites for the new parameter:
  - `src/bin/dispatcher.rs:219` — pass `None`.
  - `src/tenant/registry.rs` (rewritten in Task 2) — pass `Some(cancel.clone())`.

#### Task 2 — idle-TTL eviction in `TenantRegistry` (`src/tenant/registry.rs`)

- Add imports: `use std::sync::Mutex as StdMutex;` (or plain `std::sync::Mutex`, disambiguated
  from `tokio::sync::RwLock` already imported), `use std::time::Instant;`,
  `use tokio::sync::Notify;`, `use tokio::task::JoinHandle;`.
- Add constants next to the existing `DEK_CACHE_*`/`KILL_SWITCH_POLL_INTERVAL` ones:
  ```rust
  pub const TENANT_IDLE_TTL: Duration = Duration::from_secs(3600);
  pub const EVICTION_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
  ```
  (`pub` because `src/bin/ingest.rs` needs them at the `start_eviction_sweep` call site.)
- Replace the map's value type with a private struct:
  ```rust
  struct RegistryEntry {
      context: Arc<TenantContext>,
      cancel: Arc<Notify>,
      poll_task: JoinHandle<()>,
      last_access: StdMutex<Instant>,
  }
  ```
  `contexts: RwLock<HashMap<Uuid, RegistryEntry>>` replaces the current
  `RwLock<HashMap<Uuid, Arc<TenantContext>>>` (`registry.rs:112`).
- `get_or_open` (`registry.rs:127-198`):
  - On both cache-hit paths (the read-lock fast path at line 135 and the write-lock re-check at
    line 142), before returning, touch the entry's `last_access`:
    `*entry.last_access.lock().expect("TenantRegistry last_access mutex poisoned") = Instant::now();`
    then return `entry.context.clone()`.
  - On the true-miss path: create `let cancel = Arc::new(Notify::new());`, pass
    `Some(cancel.clone())` as the new argument to `run_refresh_loop` in the existing
    `tokio::spawn` block (lines 177-186), capture its `JoinHandle` (`tokio::spawn` already
    returns one — bind it instead of discarding), and insert
    `RegistryEntry { context: context.clone(), cancel, poll_task, last_access: StdMutex::new(Instant::now()) }`
    in place of the current `contexts.insert(tenant_id, context.clone())` (line 196). Return
    `context` as today.
- Add:
  ```rust
  impl TenantRegistry {
      /// Removes every entry idle past `idle_ttl`, signalling its poll loop to
      /// stop and waiting for it to actually exit before returning — proof
      /// the pool is no longer being polled, not just orphaned.
      pub async fn evict_idle(&self, idle_ttl: Duration) {
          let removed: Vec<RegistryEntry> = {
              let mut contexts = self.contexts.write().await;
              let expired: Vec<Uuid> = contexts
                  .iter()
                  .filter(|(_, entry)| {
                      entry
                          .last_access
                          .lock()
                          .expect("TenantRegistry last_access mutex poisoned")
                          .elapsed()
                          >= idle_ttl
                  })
                  .map(|(id, _)| *id)
                  .collect();
              expired.into_iter().filter_map(|id| contexts.remove(&id)).collect()
          };
          for entry in removed {
              entry.cancel.notify_one();
              let _ = entry.poll_task.await;
          }
      }

      /// Spawns the periodic idle-TTL sweep once. Never called from `new()` —
      /// see the Implementation Plan's confirmed decision 3.
      pub fn start_eviction_sweep(self: &Arc<Self>, idle_ttl: Duration, sweep_interval: Duration) {
          let registry = self.clone();
          tokio::spawn(async move {
              loop {
                  tokio::time::sleep(sweep_interval).await;
                  registry.evict_idle(idle_ttl).await;
              }
          });
      }
  }
  ```
- Update `TenantContext`'s doc comment (`registry.rs:37-41`), which currently reads "no eviction
  exists yet" — no longer true; replace with a short note pointing at `evict_idle`.

#### Task 3 — start the sweep in `messgr-ingest` (`src/bin/ingest.rs`)

- Change `registry: Arc::new(TenantRegistry::new())` (line 77) to bind the `Arc` first:
  ```rust
  let registry = Arc::new(TenantRegistry::new());
  registry.start_eviction_sweep(
      messgr::tenant::registry::TENANT_IDLE_TTL,
      messgr::tenant::registry::EVICTION_SWEEP_INTERVAL,
  );
  ```
  then use `registry.clone()` (or `registry` directly, since it's only read once) in the
  `AppState { registry, .. }` literal.

### Acceptance test

New integration test file `tests/tenant_registry.rs`, following `tests/kill_switch.rs`'s own
setup convention (`provision_tenant` + `set_tenant_config` against a real local Postgres +
dev Vault, `CONTROL_DATABASE_URL` env var required — same as every other test in `tests/`):

1. Provision a test tenant and set its config (mirrors `tests/ingest.rs`'s `setup` /
   `tests/kill_switch.rs`'s `provision_test_tenant`, minus the HTTP server — this test drives
   `TenantRegistry` directly, no `axum` server needed).
2. `let registry = Arc::new(TenantRegistry::new());`
3. `let first = registry.get_or_open(&control_pool, &control_url, &vault, tenant_id, 5).await.expect(...);`
   — this spawns the poll loop.
4. `tokio::time::timeout(Duration::from_secs(5), registry.evict_idle(Duration::from_millis(0))).await.expect("evict_idle must not hang if cancellation works");`
   — TTL of `0` makes the just-opened entry immediately "idle" without a real sleep.
5. `let second = registry.get_or_open(&control_pool, &control_url, &vault, tenant_id, 5).await.expect(...);`
   `assert!(!Arc::ptr_eq(&first, &second), "evicted tenant must reopen a fresh context, not reuse the evicted one");`
   — proves the entry was actually removed (a no-op eviction would return the same `Arc`).
6. Step 4's `timeout` succeeding is itself the proof the poll loop actually exited (per
   confirmed decision 5): `evict_idle` only returns after `poll_task` completes, so a broken
   `select!` wire-up (e.g. the `cancel` branch missing) manifests as a timeout, not a silent
   pass.
7. Clean up: drop `first`/`second`, close pools, drop the test tenant/database (mirrors
   `TestTenant::cleanup` / `drop_test_tenant` in the existing test files).

Run: `just test` (runs the full suite including this new file; requires the local Postgres +
Vault dev stack the other integration tests already need — no new setup).
Also run `just build` and `just lint` clean.

### Docs update (mandatory when user-facing)

No user-facing surface — `TenantRegistry` and `run_refresh_loop` are internal to
`messgr-ingest`/`messgr-dispatcher`, no CLI flag, HTTP route, or config surface changes.
`just docs-check` is expected to pass unchanged.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` clean.
2. `just docs-check` clean (no doc changes expected).
3. Write a summary: files touched (`src/kill_switch/cache.rs`, `src/tenant/registry.rs`,
   `src/bin/dispatcher.rs`, `src/bin/ingest.rs`, new `tests/tenant_registry.rs`), the TTL/sweep
   values chosen, anything deferred.
4. Suggested commit message:
   ```
   feat(tenant): evict idle TenantRegistry entries, cancelling their kill-switch poll loop (T-031)
   ```
5. Tidy WIP commits into atomic ones (root-path child, `path = "."`).
6. Commit locally; present for approval before publish (no push/MR without user approval); then
   finalize, push, verify remote base isn't behind (`tickets/` path check), open the MR. Hand
   back.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-05 — created (TO DO). source: review: split out of T-025's item 7 at refinement — eviction requires cancelling the per-tenant kill-switch poll loop too, not just a HashMap TTL, a scope big enough to warrant its own ticket.
- 2026-09-07 — TO DO → READY: plan complete
