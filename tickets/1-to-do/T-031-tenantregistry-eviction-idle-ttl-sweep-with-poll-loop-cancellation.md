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

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-05 — created (TO DO). source: review: split out of T-025's item 7 at refinement — eviction requires cancelling the per-tenant kill-switch poll loop too, not just a HashMap TTL, a scope big enough to warrant its own ticket.
