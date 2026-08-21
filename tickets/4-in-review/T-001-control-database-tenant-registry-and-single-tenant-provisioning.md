---
id: T-001
title: Control database, tenant registry, and single-tenant provisioning
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
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

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-001-control-database-tenant-registry-and-single-tenant-provisioning
```

All work on this branch, in `.` (root-path child, `layout = "in-tree"`). Local WIP commits
encouraged; do not push or open a merge request without explicit user approval (project
commit policy). Before presenting for approval, tidy WIP commits into a small number of
atomic commits (root-path default — keep history, don't squash).

### Prerequisite gate (hard)

None. This is the first ticket. The only environmental precondition is a local Postgres
reachable via `docker compose up -d` (already in the repo), which is a standing dev
requirement, not a ticket dependency.

### Confirmed design decisions (do not deviate without asking)

1. **Two databases in one Postgres cluster for local dev: `control` and dynamically-created
   `tenant_<slug>`.** §2.1 requires one database per tenant, not one server per tenant — a
   single compose Postgres instance can host both without contradicting the design.
2. **`CONTROL_DATABASE_URL` doubles as the admin connection for `CREATE DATABASE`.** Postgres
   allows issuing `CREATE DATABASE` from a session connected to any existing database, and
   the compose bootstrap role (`messgr`) already has `CREATEDB` via `initdb`. No second admin
   URL is introduced.
3. **Crate layout: `src/lib.rs` (shared modules) + `src/bin/<name>.rs` per binary**, per §13's
   six-binary target. This ticket adds only `messgr-control`; `messgr-ingest` and the rest
   arrive with their own tickets. `axum` is removed from `Cargo.toml` until a binary that
   serves HTTP needs it again — the old ingest+web server is being deleted whole, not kept
   around unused.
4. **`clap` (derive) is the CLI framework for `messgr-control`.** New dependency, justified
   because every later binary needing subcommands (control keeps growing; dispatcher, query,
   etc. may too) reuses it rather than each rolling its own arg parsing.
5. **A `Profile` enum (`Dev` / `Staging` / `Production`) is introduced now**, read from
   `MESSGR_PROFILE` (default `dev`). Two other guards documented elsewhere reuse exactly this
   shape later — `MockProvider` refusing to start outside `dev` (§11.1) and dev-mode Vault
   refusing non-dev callers (§7.6) — so it is established once here rather than three times.
6. **The `current_database()` assertion (§2.1) runs in two places**: unconditionally in
   `after_connect` (once per physical connection) and additionally in `before_acquire` when
   `Profile` is `Dev` or `Staging` (skipped in `Production` to avoid per-checkout overhead in
   the hot path). A mismatch **panics** rather than returning a swallowable `Result` — a
   mis-wired tenant pool is exactly the bug this check exists to make impossible to ignore.
7. **`tenant.vault_mount` and `tenant.webhook_token` are populated at provisioning time even
   though Vault and the webhook receiver don't exist yet.** `vault_mount` gets the
   deterministic path `transit/<slug>/messgr-dek` (§7.6's own naming convention);
   `webhook_token` gets a random opaque value (§10). Both columns are `NOT NULL` in §4.11's
   schema — recording the intended values now is cheaper and more honest than relaxing the
   constraint for a gap later tickets close.
8. **Provisioning is idempotent by slug**, not by a separate flag: re-running `provision` for
   an existing slug skips `CREATE DATABASE`, re-runs the (currently empty) tenant migrations,
   and refreshes `tenant_schema_version` — it never errors on a second run.
9. **The pre-design SMS prototype is deleted outright, not migrated or feature-flagged off.**
   `src/sms/`, `src/web/`, `src/main.rs`, `src/state.rs`,
   `migrations/0001_create_sms_messages.sql`, `tests/api/`, `tests/performance/sms.js`, and
   `.github/workflows/api-tests.yml` all implement or test a table that conflates the ledger
   and the queue (§1) and stores unencrypted content (§7) — nothing in it is compatible with
   the design, per the gap analysis already on record in this conversation.
10. **CI gains a `test` job** (Postgres service container, mirroring the pattern the deleted
    `api-tests.yml` used) running `cargo test`, so the two-tenant isolation suite §14 ties to
    this build step actually runs on every push, not only locally.

### Tasks

#### Task 1 — Control database schema
`migrations/control/0001_control_schema.sql`: the five tables from §4.11 verbatim —
`tenant`, `producer_cert`, `tenant_schema_version`, `platform_kill_switch`,
`platform_audit` — with their stated columns, constraints, and comments.

#### Task 2 — Tenant migrations directory
Create `migrations/tenant/` (tracked via `.gitkeep`, currently empty — the ledger/outbox
schema lands in a later ticket). Verify `sqlx::migrate!("./migrations/tenant")` compiles
and runs against zero migration files; if it does not, add a single no-op comment-only
`.sql` file instead and note that in the Finish summary.

#### Task 3 — Shared crate scaffolding
Add `src/lib.rs` exposing `pub mod config; pub mod profile; pub mod db; pub mod tenant;`.
In `Cargo.toml`: remove the `axum` dependency, add `clap = { version = "4", features =
["derive"] }`, and add:
```toml
[[bin]]
name = "messgr-control"
path = "src/bin/control.rs"
```

#### Task 4 — Profile and config
`src/profile.rs`: `pub enum Profile { Dev, Staging, Production }`, `Profile::from_env()`
(`MESSGR_PROFILE`, default `dev`), `fn checks_pool_identity(&self) -> bool` (true for `Dev`
and `Staging`). `src/config.rs`: `pub struct Config { control_database_url: String,
database_max_connections: u32, profile: Profile }` + `Config::from_env()` (dotenvy +
`env::var`, mirroring the removed `main.rs`'s existing pattern).

#### Task 5 — Tenant-aware pool connector
Rewrite `src/db.rs`: keep `pub async fn connect(url: &str, max_connections: u32) ->
Result<PgPool, sqlx::Error>` for the control pool, and add `pub async fn
connect_with_expected_database(url: &str, max_connections: u32, expected_db: &str,
profile: Profile) -> Result<PgPool, sqlx::Error>`, wiring `PgPoolOptions::after_connect`
(always) and `.before_acquire` (only when `profile.checks_pool_identity()`) to run `SELECT
current_database()` and panic on a mismatch against `expected_db`.

#### Task 6 — Tenant model, repo, and database-name→URL helper
`src/tenant/model.rs`: `Tenant` struct mirroring §4.11's `tenant` table columns.
`src/tenant/repo.rs`: `upsert_provisioning(pool, tenant)`, `mark_active(pool, tenant_id)`,
`record_schema_version(pool, tenant_id, version)`, `find_by_slug(pool, slug)` against the
control pool. `src/tenant/pool.rs`: `fn tenant_database_url(base_url: &str, database_name:
&str) -> String` (swap the path component of a Postgres connection URL) + `async fn
connect_tenant_pool(base_url, database_name, max_connections, profile) -> PgPool` wrapping
Task 5's connector with `expected_db = database_name`.

#### Task 7 — Provisioning orchestration
`src/tenant/provision.rs`: `pub async fn provision_tenant(control_pool: &PgPool,
base_db_url: &str, slug: &str, region: &str, database_name: &str, profile: Profile) ->
Result<Uuid, ProvisionError>`:
1. Look up an existing tenant by slug; reuse its id if present, else generate one and
   insert with `status = 'provisioning'`, a freshly generated `webhook_token`, and
   `vault_mount = format!("transit/{slug}/messgr-dek")`.
2. On the control connection, check `pg_database` for `database_name`; issue `CREATE
   DATABASE "<database_name>"` if absent (never inside a transaction block).
3. `connect_tenant_pool(...)`, then `sqlx::migrate!("./migrations/tenant").run(&tenant_pool)`.
4. Query `SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations` on the tenant pool;
   `record_schema_version`.
5. `mark_active`.
Must be safe to call twice for the same slug (decision 8).

#### Task 8 — `messgr-control` binary
`src/bin/control.rs`: clap subcommands `migrate` (run `sqlx::migrate!("./migrations/control")`
against the control pool and exit) and `provision --slug <slug> --region <region>
--database-name <name>` (call Task 7, print the resulting tenant id). `main()` loads
`Config::from_env()` and connects the control pool — **it does not migrate**. Only the
`migrate` subcommand runs `sqlx::migrate!`; `provision` assumes the control schema is
already applied and fails with a clear error (not a silent auto-migration) if the `tenant`
table doesn't exist. Per §13, migrations are never automatic on process start — the
deleted prototype's self-migrating `main()` is exactly the pattern this must not repeat.

#### Task 9 — Remove the pre-design prototype
Delete `src/main.rs`, `src/state.rs`, `src/sms/`, `src/web/`,
`migrations/0001_create_sms_messages.sql`, `tests/api/`, `tests/performance/sms.js`,
`.github/workflows/api-tests.yml`. Update `justfile`: drop `seed`, `list`, `test-api`,
`test-perf`, `run`, `watch` (all tied to the removed HTTP server); repoint `migrate` /
`migrate-revert` at the control database with an explicit source
(`sqlx migrate run --source migrations/control`, since sqlx-cli defaults to `./migrations`
which is now just a parent directory with no loose files) and `db-shell` at `.../control`;
add `provision` and `control-migrate` recipes wrapping the new binary. Update
`compose.yml`: `POSTGRES_DB: control` **and** the healthcheck's `pg_isready -U messgr -d
control` (both must change together, or the container reports unhealthy against a database
that no longer exists). Update `.env` / `.env.example`: replace `DATABASE_URL` /
`SERVER_ADDR` / `SMS_QUEUE_CAPACITY` with
`CONTROL_DATABASE_URL=postgres://messgr:messgr@localhost:5432/control`,
`DATABASE_MAX_CONNECTIONS=20`, `MESSGR_PROFILE=dev`.

#### Task 10 — Two-tenant isolation test
`tests/tenancy.rs`: integration test reading `CONTROL_DATABASE_URL` from env, provisioning
two tenants with randomly generated slugs/database names by calling Task 7's function
directly (not through the CLI), asserting:
- each tenant's pool reports `current_database()` equal to its own `database_name`
  (proves the pools are not accidentally sharing one database);
- calling `connect_with_expected_database` with a deliberately wrong `expected_db` panics.
Best-effort teardown (drop both tenant databases and their `tenant` /
`tenant_schema_version` rows) after assertions run, not gating pass/fail on cleanup
succeeding.

#### Task 11 — CI
Add a `test` job to `.github/workflows/ci.yml`: a Postgres service container
(`POSTGRES_DB: control`, matching compose), `cargo build`, then `cargo test` with
`CONTROL_DATABASE_URL` pointed at the service.

#### Task 12 — README
Add a root `README.md`: one line on what messgr is (linking to `DESIGN.md`), local dev
setup (`docker compose up -d`, copy `.env.example` to `.env`), and how to run
`just control-migrate` / `just provision slug=... region=... db=...`.

### Acceptance test

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build

docker compose up -d
cp .env.example .env   # if not already present

cargo run --bin messgr-control -- migrate
cargo run --bin messgr-control -- provision --slug acme --region eu --database-name tenant_acme
# expect: prints a tenant id, exit 0

cargo run --bin messgr-control -- provision --slug acme --region eu --database-name tenant_acme
# expect: same tenant id, exit 0, no error — idempotent re-run (decision 8)

psql "$CONTROL_DATABASE_URL" -c "select slug, status, database_name from tenant;"
# expect exactly one row: acme | active | tenant_acme

psql "$CONTROL_DATABASE_URL" -c "select count(*) from tenant_schema_version;"
# expect 1

cargo test
# expect tests/tenancy.rs green: two tenants isolated by database, and the mis-wired-pool
# assertion panics as expected
```

### Docs update (mandatory when user-facing)

Add root `README.md` (Task 12) — this ticket ships the project's first operator-facing
capability (a provisioning CLI), and there is currently no dev-facing doc other than the
justfile itself.

### Finish (mandatory)

1. Acceptance test green; `cargo fmt`/`cargo clippy`/`cargo build`/`cargo test` clean.
2. `README.md` added and accurate against the final CLI/justfile shape.
3. Write a summary: files touched (new control-plane modules, deleted prototype, CI/compose/
   justfile/env changes), any decision from the list above that had to be adjusted during
   implementation and why, anything deferred.
4. Suggested commit message (broad change, no single scope — omit the parens):
   ```
   feat: add control database, tenant registry, and provisioning CLI (T-001)
   ```
5. Tidy WIP commits into a small number of atomic commits before presenting (root-path
   child default: keep history, don't squash).
6. Commit locally on the ticket branch. Do not push or open a merge request without
   explicit user approval. Under `layout = "in-tree"`, before pushing verify the remote
   base is not behind: `git fetch origin main && git diff --name-only
   origin/main...HEAD | grep '^tickets/'` must print nothing. Present the commit message;
   only after approval, push and open the merge request. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-20 — created (TO DO). source: chat: build order step 0b (DESIGN.md §14) — first ticket of the from-scratch rebuild against the current design, replacing the pre-design SMS prototype.
- 2026-08-20 — TO DO → READY: implementation plan complete
- 2026-08-20 — plan amended inline: applicability-gate audit (fresh sub-agent) found Task 8 had `messgr-control` self-migrate the control DB on every invocation, contradicting §13's "never automatic on process start" — changed so only the `migrate` subcommand runs `sqlx::migrate!`, `provision` fails loudly if the schema isn't applied yet. Also fixed two smaller plan-text bugs in Task 9: `compose.yml`'s healthcheck wasn't updated alongside `POSTGRES_DB: control`, and the `justfile` `migrate`/`migrate-revert` recipes were missing `--source migrations/control`. Remaining audit notes (sqlx empty-migrations-dir behavior, `after_connect`/`before_acquire` signatures, `CREATE DATABASE` from a non-`postgres` connection, delete-list completeness) confirmed true as written — noted, no plan change needed.
- 2026-08-21 — READY → IN DEVELOPMENT: picked up
- 2026-08-21 — IN DEVELOPMENT → IN REVIEW: acceptance test green
