---
id: T-001
title: Control database, tenant registry, and single-tenant provisioning
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium-high
cost: L
---

# T-001 — Control database, tenant registry, and single-tenant provisioning

## Outcome

After this ships, an operator can run one command to provision a tenant — create its
database, apply migrations, and register it in the control database — and every later
piece of messgr (producer identity, the ledger, dispatchers) has a `tenant` row and an
isolated database to key against. This replaces the current single-pool, single-table
prototype, which has no tenant concept at all.

## Description

DESIGN.md §2.1 requires one Postgres database per tenant, with a separate **control**
database (§4.11, outside every tenant database) holding no customer data: `tenant`,
`producer_cert` (cert-subject → tenant resolution, needed *before* any tenant database is
opened), `tenant_schema_version` (migration drift across the fleet), `platform_kill_switch`,
and `platform_audit`. Build order step 0b (§14) puts this first, ahead of the producer
registry (step 1) and the ledger (step 2), because `producer_cert` resolution and
`tenant_id` both have to exist before anything else is built on top of them — retrofitting
a tenant column onto a partitioned ledger later is a rewrite, not a migration.

This ticket covers:

- The control database schema (§4.11), migrated with the same `sqlx migrate` mechanism
  the project already uses.
- A single-command provisioning path (§11.4) that creates a tenant's database, runs its
  (currently near-empty) migrations, and records the tenant in `tenant` +
  `tenant_schema_version`. Vault mount/Transit key creation and dispatcher startup are
  explicitly **out of scope** here — those subsystems don't exist yet (Vault lands with
  encryption at step 2, the dispatcher at step 7) — the provisioning path should leave an
  obvious seam for them rather than stubbing them silently.
- The `current_database()` pool-mismatch assertion (§2.1) that stands in for RLS: on pool
  creation (and checkout, in debug/staging), assert the connected database matches the
  tenant the pool was meant for. This is the *only* isolation mechanism the design uses,
  since there is no RLS and no shared schema to filter.
- The two-tenant isolation test §14 explicitly ties to this step ("From step 0b onward, CI
  runs a two-tenant integration suite"): at minimum, that a pool deliberately mis-wired to
  the wrong tenant trips the assertion above, and that work done against tenant A's pool
  never reads or writes a row in tenant B's database.
- Removing the pre-design prototype (`src/sms/`, `src/web/`,
  `migrations/0001_create_sms_messages.sql`, and the now-obsolete single-pool wiring in
  `src/db.rs`/`src/state.rs`/`src/main.rs`). It predates DESIGN.md, conflates the ledger and
  the queue (the one thing §1 says never to do), has no tenant concept, and stores
  plaintext content that could never be reconciled with the per-customer-DEK-from-first-write
  rule in §7/§14. It is being discarded, not migrated — this is the natural point to remove
  it, since provisioning is already rewriting the app's bootstrap and connection story in
  `main.rs`/`db.rs`/`state.rs`.

No hard dependencies (this is the first ticket), but nearly everything filed after it will
depend on it: the producer registry (build step 1) needs `producer_cert` to resolve a
tenant before it can look up a producer within that tenant's database, and the ledger
(step 2) needs `tenant_id` and a real per-tenant database to write into.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-20 — created (TO DO). source: chat: build order step 0b (DESIGN.md §14) — first ticket of the from-scratch rebuild against the current design, replacing the pre-design SMS prototype.
