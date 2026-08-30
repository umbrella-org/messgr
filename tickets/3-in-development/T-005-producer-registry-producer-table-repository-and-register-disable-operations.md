---
id: T-005
title: "Producer registry: producer table, repository, and register/disable operations"
project: messgr
depends-on: [T-001]
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-005 — Producer registry: producer table, repository, and register/disable operations

## Outcome

An operator can run `messgr-control producer register` to register an upstream system (fraud,
statements, onboarding, marketing, collections, card services, ...) against a tenant, list the
registered producers, and disable one — without touching either database by hand. Every
registration lands both halves of the producer's identity: the `producer` row in the tenant's
database and the `producer_cert` mapping in the control database. No send path consumes it
yet; mTLS resolution (T-007) is the first reader.

## Description

Adds the `producer` table to the tenant database schema — the **first tenant migration**;
`migrations/tenant/` currently holds only `.gitkeep`, and `provision_tenant`
(`src/tenant/provision.rs`) already runs `sqlx::migrate!("./migrations/tenant")` against an
empty directory, so this ticket populates a runner that is already wired.

The table, per §4.9:

```sql
CREATE TABLE producer (
    id           uuid PRIMARY KEY,
    name         text UNIQUE NOT NULL,
    cert_subject text UNIQUE NOT NULL,   -- mTLS CN/SAN this producer authenticates with
    owner_team   text NOT NULL,
    contact      text NOT NULL,          -- who to page when its quota alerts fire
    enabled      bool NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL
);
```

Alongside the table: a repository layer (insert, look up by `id`/`name`/`cert_subject`, list,
disable), the register/disable/list operations, and a `messgr-control producer` subcommand group
exposing them.

**Registration spans two databases.** T-001's migration `0001_control_schema.sql` already
created `producer_cert (cert_subject PK, tenant_id, producer_id)` in the **control** database,
and to date nothing writes a row into it. Registering a producer therefore writes both halves:
the `producer` row in the tenant database and the `producer_cert` mapping in the control
database. Splitting those across two tickets would mint producers that cannot authenticate and
leave the duplicated `cert_subject` to drift between the two databases. T-007 remains the
*read* path (client cert → `producer_cert` → `(tenant_id, producer_id)`) plus dev PKI issuance;
it consumes what this ticket writes.

There is no distributed transaction available across the two databases, so the write order is
load-bearing and stated as decision 2 below, with idempotent re-registration as the repair.

Explicitly out of scope, reserved for later steps already named in PLAN.md: mTLS resolution
itself and dev PKI issuance (T-007); `producer_quota` / `producer_quota_override` /
`producer_usage` (build step 6, T-023/T-024); the admin panel surface with before/after audit
(T-043). Quota and kill-switch behaviour key on this identity but are built later.

Soft coupling: T-007 is the first consumer, looking producers up by `cert_subject` and then
checking `enabled` — decision 5 keeps the `producer_cert` row on disable precisely so T-007 can
tell "unknown cert" from "known but disabled".

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-005-producer-registry
```

This child is root-path (`path = "."`, pickle.toml), so WIP commits are encouraged during the
work and then interactive-rebased into atomic, correctly scoped commits before the summary is
presented (rules §0). Do not push and do not open a merge request without explicit user
approval. Ticket and board bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-001` is in `6-done/` and merged to `main` (it created `migrations/control/0001_control_schema.sql`,
  which this ticket's `producer_cert` writes depend on). Confirmed at refinement: merged.
- Clean working tree before branching.
- Local stack up: `just db-up` (Postgres + Vault), then `just control-migrate`, then
  `just vault-dev-init`. The integration tests provision real tenants and need all three.

### Confirmed design decisions (do not deviate without asking)

1. **Registration writes both databases — the tenant `producer` row and the control
   `producer_cert` row.** T-001 created `producer_cert` and nothing has written to it since. A
   producer registered only tenant-side can never authenticate, and the `cert_subject` value
   duplicated across the two databases would be written by two different tickets at two
   different times, which is how the two copies drift. T-007 is the read path only.
2. **Write the tenant `producer` row first, then the control `producer_cert` row.** No
   distributed transaction exists across the two databases, so one of the two crash windows has
   to be chosen deliberately. Tenant-first leaves, on a crash between the writes, a producer
   that no cert resolves to — inert and invisible, repaired by re-running the command.
   Control-first would leave a `producer_cert` resolving to a `producer_id` that does not exist,
   so the mTLS edge would accept the certificate and only fail after opening the tenant
   database: a worse failure, later, with a misleading error. Do not "improve" this by reordering.
3. **Re-registration is idempotent on identical inputs and rejected on conflicting ones**,
   mirroring `provision_tenant`'s established semantics (`src/tenant/provision.rs`). Same
   `(name, cert_subject, owner_team, contact)` for an existing `name` → no-op that re-confirms
   both rows (this is also how decision 2's partial-write window is repaired). Same `name` with
   a *different* `cert_subject` — or the same `cert_subject` already bound to a different
   producer or a different tenant — → rejected with a non-zero exit and an audit row. Idempotence
   means repeating an operation, not overwriting it with a different one.
4. **Every register/disable attempt writes exactly one `platform_audit` row** via
   `crate::platform_audit::record` — including the rejected ones. T-002 exists because an audit
   table was created and never written; shipping a second unaudited mutation path repeats that
   exact defect. Actions: `producer.register`, `producer.disable`. The `detail` JSON carries the
   producer name, the `cert_subject`, and the outcome (`created` | `idempotent` | `rejected`),
   matching the shape `provision_tenant` already uses. `actor` is supplied via `--actor`, as with
   `provision` — `messgr-control` still has no auth realm to infer it from.
5. **Disable sets `enabled = false` on the tenant row and leaves the `producer_cert` row in
   place.** Deleting the mapping would make a disabled producer indistinguishable from an
   unregistered one at the mTLS edge; keeping it lets T-007 return a distinct, diagnosable error
   instead of a generic rejection. Disable is idempotent — disabling an already-disabled producer
   succeeds and still audits.
6. **`producer` carries no `tenant_id` column.** It lives inside the tenant's own database;
   the tenant is the database (§2.1). The `tenant_id` in `producer_cert` exists only because the
   control database must resolve a cert *before* any tenant database is open.
7. **No RLS, no tenant filter** (§2.1, decision recorded in PLAN.md's "Deliberately not
   tickets"). Tenant-scoped access goes through `connect_tenant_pool`, which carries the
   `current_database()` assertion.

### Tasks

#### Task 1 — First tenant migration

Create `migrations/tenant/0001_producer.sql` with the §4.9 `producer` table verbatim (`id`,
`name UNIQUE`, `cert_subject UNIQUE`, `owner_team`, `contact`, `enabled DEFAULT true`,
`created_at`). Head the file with a comment naming DESIGN.md §4.9 and noting this is the first
tenant-database migration, in the style of `migrations/control/0001_control_schema.sql`.

Note for the implementer: `provision_tenant` already runs this directory and records the
resulting version via `repo::record_schema_version`, so an existing dev tenant picks the
migration up on its next (idempotent) re-provision. No new runner is needed here — the fleet-wide
runner is T-057.

#### Task 2 — Producer model and tenant-side repository

Add `src/producer/mod.rs`, `src/producer/model.rs`, `src/producer/repo.rs`; register
`pub mod producer;` in `src/lib.rs`.

- `model.rs`: a `Producer` struct deriving `Debug, Clone, sqlx::FromRow`, mirroring the table
  exactly — follow `src/tenant/model.rs`.
- `repo.rs`: `insert`, `find_by_name`, `find_by_cert_subject`, `list`, `set_enabled`. Take
  `&PgPool` (the tenant pool) and return `Result<_, sqlx::Error>`, following
  `src/tenant/repo.rs`'s shape and doc-comment style.

#### Task 3 — Control-side `producer_cert` repository

Add `upsert_producer_cert`, `find_producer_cert`, and `delete_producer_cert` (the last for test
cleanup only) operating on the **control** pool. Put them in `src/producer/cert_repo.rs`, with a
doc comment stating why this one table lives in the control database (resolution precedes
opening any tenant database — decision 1) so the split does not read as an accident.

#### Task 4 — Register / disable / list operations

Add `src/producer/register.rs` holding the orchestration, in the shape of
`src/tenant/provision.rs`:

- `register_producer(control_pool, base_db_url, tenant_slug, name, cert_subject, owner_team, contact, profile, actor)`.
  Resolve the tenant via `tenant::repo::find_by_slug`; open its pool via
  `tenant::pool::connect_tenant_pool`; apply decision 3's idempotent/rejected branching; write
  tenant row then control mapping in decision 2's order; write the decision 4 audit row on all
  three outcomes; close the tenant pool.
- `disable_producer(...)` per decision 5.
- `list_producers(...)` returning `Vec<Producer>` for the CLI.
- A `ProducerError` enum in the shape of `ProvisionError`, with `Display`/`Error`/`From` impls.

#### Task 5 — `messgr-control producer` subcommands

Extend `src/bin/control.rs` with a `Producer` subcommand group: `register` (`--tenant-slug`,
`--name`, `--cert-subject`, `--owner-team`, `--contact`, `--actor`), `disable`
(`--tenant-slug`, `--name`, `--actor`), `list` (`--tenant-slug`). Follow the existing `Provision`
arm: no Vault client is connected for these (they touch neither Transit nor AppRole — the same
reason `Migrate` does not), a rejected operation exits non-zero with a message naming the
conflict, and `list` prints one producer per line including `enabled`.

#### Task 6 — Extend the two-tenant integration suite

Add `tests/producer.rs`, following `tests/tenancy.rs`'s conventions exactly (`unique_name`,
real provisioning, best-effort `drop_test_tenant`-style cleanup extended to delete the
`producer_cert` rows the tests create). Cover:

1. Register → the tenant row exists **and** the control `producer_cert` row exists and points at
   the right `(tenant_id, producer_id)`.
2. **Two-tenant isolation** (§14's standing requirement): the same producer `name` registered
   for tenant A and tenant B yields two distinct `producer_id`s in two databases, and A's
   producer is not visible from B's pool.
3. Idempotent re-registration — no duplicate row, second `platform_audit` row with
   `outcome=idempotent` (decision 3).
4. Conflicting re-registration (same `name`, different `cert_subject`) — rejected, and a
   `producer.register` audit row with `outcome=rejected` written (decisions 3 and 4).
5. A `cert_subject` already registered to a *different tenant* — rejected (decision 3); this is
   the cross-tenant impersonation case, so assert it explicitly rather than relying on the
   tenant-local `UNIQUE`, which cannot see across databases.
6. Disable — `enabled = false`, `producer_cert` row still present (decision 5), and disabling
   twice succeeds.

### Acceptance test

Run from the repository root with the local stack up:

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/producer.rs
```

Then exercise the CLI end to end against a real tenant:

```
just provision acme eu tenant_acme operator@example.com
cargo run --bin messgr-control -- producer register \
    --tenant-slug acme --name fraud-alerts \
    --cert-subject "CN=fraud-alerts.internal" \
    --owner-team fraud --contact fraud-oncall@example.com --actor operator@example.com
cargo run --bin messgr-control -- producer list --tenant-slug acme
cargo run --bin messgr-control -- producer disable \
    --tenant-slug acme --name fraud-alerts --actor operator@example.com
cargo run --bin messgr-control -- producer list --tenant-slug acme
```

Expected: `register` prints the new producer id; the first `list` shows `fraud-alerts` as
enabled; `disable` succeeds; the second `list` shows it disabled. Re-running the identical
`register` command succeeds as a no-op (decision 3). Running it again with a different
`--cert-subject` exits non-zero and names the conflict.

Verify both halves and the audit trail:

```
psql postgres://messgr:messgr@localhost:5432/tenant_acme \
     -c "SELECT name, cert_subject, enabled FROM producer"
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT cert_subject, tenant_id, producer_id FROM producer_cert"
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT action, detail->>'outcome' FROM platform_audit WHERE action LIKE 'producer.%' ORDER BY at"
```

Expected: the `producer_id` in the control row equals the `id` in the tenant row; the audit
query lists one row per attempt, including `rejected` for the conflicting one.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control producer` subcommands.

- `README.md` — under "Local development", document `producer register|disable|list` alongside
  the existing `provision` walkthrough, including that registration writes both databases and
  that disable is reversible-by-re-register but never deletes the cert mapping.
- `justfile` — add convenience recipes mirroring `provision` (e.g. `producer-register`,
  `producer-list`).
- No `DESIGN.md` change expected: this implements §4.9 as written. If implementation forces a
  deviation from §4.9, stop and raise it rather than editing the design to match the code.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README and justfile updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(producer): add producer registry with tenant and control-plane registration (T-005)

   Adds the first tenant migration (producer table, DESIGN.md §4.9), a tenant-side
   repository, and register/disable/list operations exposed as messgr-control
   subcommands. Registration writes both the tenant producer row and the control
   producer_cert mapping, tenant-first, with idempotent re-registration as the
   repair for a partial write. Every attempt writes a platform_audit row.
   ```

5. Root-path child: interactive-rebase the WIP commits into a small number of atomic, correctly
   scoped commits (migration / repository / operations+CLI / tests / docs is a natural split)
   before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-29 — created (TO DO). source: chat: filed from PLAN.md's build-step-1 row (provisional id `T-006` there; the per-prefix counter assigned `T-005`, as PLAN.md warned it would once tickets file out of the original draft order).
- 2026-08-29 — TO DO → READY: plan complete; scope corrected to include the control-DB producer_cert write, re-graded S to M
- 2026-08-30 — READY → IN DEVELOPMENT: picked up
