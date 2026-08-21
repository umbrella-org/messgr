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

2026-08-21 — first review pass (full audit, not scoped).

- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a; the project ships
      no docs site, so "docs" is `README.md` + `DESIGN.md` + the justfile's self-description — no
      docs build command exists to run)
- [x] Docs-readability pass (step 4b) — **skipped: no docs-readability reviewer reachable from this
      session** (no `docs_readability` tool, no `opencode` subagent configured). Sanctioned conscious
      skip; never blocking.
- [x] Findings recorded with severity, class and disposition (step 5)
- [x] Ticket moved (step 6)
- [x] Other references updated; board regenerated by the move (step 7)
- [x] Remaining-tickets impact sweep done (step 8) — no other tickets exist on the board
- [x] Summary + commit message presented for approval (step 9)

### Verification performed

Acceptance test re-run verbatim against a throwaway Postgres 18 container (host port 5432 was
occupied by an unrelated stack, so the cluster was bound to 55432 and `CONTROL_DATABASE_URL`
overridden; `dotenvy` does not override a set variable, so this is faithful):

| step | result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo build` | clean |
| `messgr-control migrate` | exit 0 (silent — see F9) |
| `messgr-control provision --slug acme --region eu --database-name tenant_acme` | printed `50b1d6b1-…`, exit 0 |
| same command again (idempotence, decision 8) | same id, exit 0 |
| `select slug, status, database_name from tenant` | exactly one row: `acme \| active \| tenant_acme` |
| `select count(*) from tenant_schema_version` | 1 (version `0`, the empty tenant migration set) |
| `cargo test` | 5 passed, 0 failed (3 unit, 2 integration in `tests/tenancy.rs`) |

Tasks 1–12 are all present in the files the plan names. Task 1's schema is byte-faithful to
DESIGN.md §4.11. Task 2 needed no fallback file — `sqlx::migrate!` compiles and runs against a
directory holding only `.gitkeep`, recording version `0`. Confirmed decisions 1–7 and 9–10 were
honoured; **decision 8 was only partially honoured (F1)**.

### Findings

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | `provision` re-run for an existing slug with a *different* `--database-name` (or `--region`) silently creates and migrates an orphan database, records **its** schema version against the tenant, and leaves `tenant.database_name`/`region` pointing at the original — exit 0, no warning. The tenant registry then disagrees with `pg_database`, and it is the registry everything later keys off. | Reproduced: after provisioning `acme`/`tenant_acme`, `provision --slug acme --region us --database-name tenant_acme_typo` exited 0; `pg_database` then held both `tenant_acme` and `tenant_acme_typo` while `tenant` still read `acme \| eu \| tenant_acme`. `src/tenant/provision.rs:29-51` | Decision 8's idempotence means *same inputs*. On an existing slug, compare `database_name` and `region` against the stored row and return a clear error on mismatch instead of proceeding. |
| F2 | blocking | docs-gap | — | `README.md` documents the ticket's headline command as `just provision slug=acme region=eu db=tenant_acme`. `just` passes recipe arguments positionally, so those tokens arrive verbatim — and the command **succeeds**, registering a tenant with slug `slug=acme`. A wrong command that fails is a nuisance; one that silently creates garbage state in the tenant registry is the same defect class as F1. | `just -n provision slug=acme region=eu db=tenant_acme` → `cargo run --bin messgr-control -- provision --slug slug=acme --region region=eu --database-name db=tenant_acme`. `README.md:13` | Use the positional form: `just provision acme eu tenant_acme`. |
| F3 | blocking | test-gap | — | The two-tenant suite implements one of the two assertions DESIGN.md §14 requires from step 0b, and one of the two this ticket's Description requires. Missing: (a) that work against tenant A's pool cannot read or write a row in tenant B's database — the suite only compares `current_database()` strings and never touches a row; (b) the **checkout** arm of the assertion — §14 requires it fire "at pool creation *and* at checkout", but the mis-wired test only exercises `after_connect`, so `before_acquire` and `Profile::checks_pool_identity()` (decision 6) are never executed under a mismatch. §14 calls this "the whole isolation mechanism", so a half-tested one is the gap that matters most. | `tests/tenancy.rs:103-129` (string comparison only), `tests/tenancy.rs:159-168` (panic observed at connect time only). Description bullet 4 requires both. | No tenant tables exist yet, so the write test creates its own scratch table: `CREATE TABLE isolation_probe` + insert on A's pool, then assert `to_regclass('isolation_probe') IS NULL` on B's. For the checkout arm, connect a pool honestly, then acquire against a mismatched expectation. |
| F4 | blocking | correctness | — | `just migrate` / `just migrate-revert` are broken by this branch and duplicate `just control-migrate`. `sqlx-cli` reads `DATABASE_URL`; Task 9 removed that variable from `.env` and `.env.example` in favour of `CONTROL_DATABASE_URL`, and added `--source` but not `--database-url`. Two recipes now claim to migrate the control database, one of which cannot. | `just -n migrate` → `sqlx migrate run --source migrations/control`; `grep -r DATABASE_URL .env.example` returns only `CONTROL_DATABASE_URL`. `justfile:37-41` | Cut both recipes — `control-migrate` already owns this and needs no external tool (AGENTS.md: "if you find yourself adding a mechanism that duplicates an existing one, cut instead"). If sqlx-cli access is wanted for `revert`, pass `--database-url "$CONTROL_DATABASE_URL"` explicitly. |

### Rework (commit `5f88138`)

| id | fix |
|---|---|
| F1 | `provision_tenant` (`src/tenant/provision.rs`) now matches on `Some(tenant) if tenant.region == region && tenant.database_name == database_name` to take the idempotent path, and returns `sqlx::Error::Configuration` naming both the stored and requested values on any other match. Verified: re-running `provision --slug acme` with a different `--database-name`/`--region` now exits 101 with a clear message; `pg_database` gains no orphan database; `tenant` is unchanged. Re-running with identical inputs still succeeds unchanged (`provision 2` in the re-run log below). |
| F2 | `README.md` now reads `just provision acme eu tenant_acme` (positional, matching the recipe's `provision slug region db:` signature). Verified via `just -n provision acme eu tenant_acme`. |
| F3 | Added `work_on_tenant_as_pool_never_reads_or_writes_tenant_bs_database` to `tests/tenancy.rs`: creates and populates a scratch table via tenant A's pool, asserts `to_regclass(...)` is `NULL` via tenant B's pool. Added `assert_current_database_panics_on_the_mismatch_before_acquire_would_catch` as a unit test in `src/db.rs` (needs the private `assert_current_database` helper): acquires a connection from an honestly-connected pool and asserts the same check `before_acquire` runs panics on a deliberately wrong expectation — the module doc comment on `tests/tenancy.rs` now explains why the checkout arm is a unit test rather than an integration one (a live Postgres connection cannot change which database it is bound to mid-life, so `before_acquire` and `after_connect` cannot be made to disagree through a real pool; the unit test exercises the identical code path directly instead). `cargo test`: 7 passed (was 5), 0 failed. |
| F4 | Removed `migrate` and `migrate-revert` from `justfile`. `control-migrate` is now the only migration recipe; `just -n migrate` correctly errors with "justfile does not contain recipe `migrate`". |

Acceptance test re-run in full after the fix (fresh Postgres 18 container, port 55432): `cargo fmt --all -- --check` clean, `cargo clippy --all-targets --all-features -- -D warnings` clean, `cargo build` clean, `migrate` → exit 0, `provision` (first run) → new id exit 0, `provision` (same inputs) → same id exit 0, `provision` (F1 regression probe: same slug, different region/database-name) → exit 101 with the new error message, tenant registry and `pg_database` both left consistent, `cargo test` → 7 passed / 0 failed.

No other findings were touched — F5–F12 are unchanged from the first pass (F5/F6 live in T-002; F7–F10, F12 stand as noted; F11 was already fixed inline in the first review pass, commit `d56de3a`).

### Scoped re-review, 2026-08-21 (verifying F1–F4 only)

**Verdict: F1, F2, F4 verified fixed. F3 is only half fixed — its second half shipped a test that
cannot fail.** One new blocking finding; back to `5-rework/`.

Each fix was verified against behaviour, not against the rework note claiming it:

| finding | method | result |
|---|---|---|
| F1 | Re-ran the original repro plus a second variant the first pass did not try (same slug + same `database_name`, *different region only*). | **Fixed.** Both mismatch variants exit 101 naming stored vs. requested values; identical-input re-run still returns the same id and exits 0; `pg_database` gains no orphan; the `tenant` row is unchanged (`acme\|eu\|active\|tenant_acme`). |
| F2 | `grep` + `just -n provision acme eu tenant_acme`. | **Fixed.** Renders `--slug acme --region eu --database-name tenant_acme`. |
| F3(a) cross-database write | **Mutation test**: repointed tenant B's pool at tenant A's database and re-ran. | **Fixed and sound.** The mutant fails with `left: Some("isolation_probe"), right: None` — the test genuinely detects broken isolation rather than passing vacuously. |
| F3(b) checkout arm | Ran the new unit test with `CONTROL_DATABASE_URL` pointed at a dead port (`localhost:59999`). | **Not fixed — see F13.** It passes with no database at all. |
| F4 | `just -n migrate`, `just -n migrate-revert`, `grep -c "sqlx migrate" justfile`. | **Fixed.** Both recipes gone, zero `sqlx-cli` references remain. |

Full suite re-run against a live Postgres 18: `fmt` clean, `clippy -D warnings` clean, 7 passed /
0 failed. That green result is exactly what F13 shows to be partly misleading.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F13 | blocking | test-gap | — | The unit test added to close F3's checkout arm, `assert_current_database_panics_on_the_mismatch_before_acquire_would_catch`, **cannot fail**. It puts `super::connect(&url, 2).await.expect(...)` and `pool.acquire().await.expect(...)` *inside* the `tokio::spawn`, then asserts only `result.is_err()` on the `JoinHandle`. Any panic in that task satisfies the assertion — including the connection itself failing — so the test is green whether or not `assert_current_database` is ever reached, and whether or not it panics for the right reason. It currently certifies "the whole isolation mechanism" (§14) while proving nothing about it. Note the contrast: all three tests in `tests/tenancy.rs` connect *outside* the spawn and so fail correctly under the same conditions — the defect was introduced by the rework, not inherited. | With `CONTROL_DATABASE_URL=postgres://…@localhost:59999/nonexistent`, `cargo test --lib assert_current_database_panics` reports `ok. 1 passed` after a 30s connect timeout. Under the same URL `cargo test --test tenancy` correctly reports `FAILED. 0 passed; 3 failed` (`PoolTimedOut`). `src/db.rs:118-140` | Move the pool connect and `acquire` *outside* `tokio::spawn` so an infrastructure failure fails the test instead of satisfying it, spawn only the `assert_current_database` call, and assert on the panic payload rather than its mere existence — downcast the `JoinError` to `&str`/`String` and require it to contain `tenant pool mis-routed`. The same tightening is worth applying to `a_mis_wired_pool_trips_the_current_database_assertion`, whose `result.is_err()` is sound today only because its connect happens outside the spawn. |
| F14 | non-blocking | design | noted | The F3 fix makes `cargo test --lib` require a live Postgres for the first time — the three pre-existing unit tests are hermetic, and the database requirement used to be confined to `tests/tenancy.rs`. Consequence beyond tidiness: with no database reachable the new test spends 30s in a connect timeout before passing (see F13), so the hermetic-unit/integration split is what would have made the tautology obvious immediately. Keeping it in `src/db.rs` is defensible — it needs the private `assert_current_database` — but the split is now blurred. | `cargo test --lib` against a dead port: 30.01s, 1 passed. `src/db.rs:118-140` vs. `src/profile.rs:53-75`. | Either accept it and say so in the module doc, or expose the helper as `pub(crate)` plus a thin `#[doc(hidden)]` test seam so the check can live beside the other isolation tests in `tests/tenancy.rs`. Not worth scheduling on its own; revisit if more DB-backed unit tests accumulate. |

**Disposition summary (scoped re-review):** 2 new findings — 1 blocking (F13, the rework scope), 1
non-blocking → **noted** (F14). F1, F2, F4 and F3(a) confirmed fixed and closed. F5–F12 untouched
and unchanged.

```
cost: estimated L, actual L
```

### Rework #2 (commit `0210c18`)

| id | fix |
|---|---|
| F13 | `assert_current_database_panics_on_the_mismatch_before_acquire_would_catch` (`src/db.rs`) now connects and acquires the connection *outside* `tokio::spawn`, so an infrastructure failure fails the test via `expect()` instead of satisfying the assertion under test — spawn wraps only the `assert_current_database` call itself. The assertion was tightened from `result.is_err()` to extracting the panic payload via `JoinError::into_panic()` and requiring the message contain `tenant pool mis-routed`, so a panic for an unrelated reason no longer passes either. |

Verified in both directions rather than trusting the description:

- **Dead port** (`CONTROL_DATABASE_URL` pointed at `localhost:59999`, nothing listening): `cargo test --lib assert_current_database_panics` now reports `FAILED. 0 passed; 1 failed`, panicking at the `expect` on the pool connect — this is the exact case F13 showed passing incorrectly before the fix.
- **Mutation** (expectation string temporarily changed from `"not_the_real_database"` to the pool's real database name, `"control"`, then reverted): no panic occurs inside the spawn, so the test correctly fails with `assert_current_database must panic on a mismatched expectation ...: Ok(())` — proves the test also fails when the code path it exists to catch produces no defect.
- **Live database, unmutated**: `cargo fmt --all -- --check` clean, `cargo clippy --all-targets --all-features -- -D warnings` clean, full suite 7 passed / 0 failed.

F14 and F5–F12 untouched, as scoped — F13 was the entire rework.

### Scoped re-review #2, 2026-08-21 (verifying F13 only)

**Verdict: F13 fixed. Ticket proceeds to `6-done/`.** One new non-blocking finding (F15), noted.

The fix was verified by trying to kill the test five ways rather than by reading it. A test whose
whole defect was "it passes when it shouldn't" earns nothing less:

| mutation | expected | observed |
|---|---|---|
| M1 — no database reachable (`localhost:59999`) | fail | **FAILED**, panics on the pool-connect `expect` (this is the exact case that passed before the fix) |
| M2 — expectation changed to the pool's real database (`"control"`), so no mismatch occurs | fail | **FAILED**: `must panic on a mismatched expectation …: Ok(())` |
| M3 — production panic message changed to `"wrong db"` | fail | **FAILED** — confirms the substring check is load-bearing, not decorative |
| M4 — production `assert_eq!` in `assert_current_database` replaced with a no-op | fail | **FAILED** — confirms the test detects the isolation check being disabled |
| honest, unmutated, live database | pass | **ok. 1 passed** |

Also re-ran the full acceptance test: `fmt` clean, `clippy -D warnings` clean, `migrate` exit 0,
`provision` fresh + idempotent both exit 0 returning the same id, one `active` tenant row, one
`tenant_schema_version` row, `cargo test` 7 passed / 0 failed. The diff is confined to `src/db.rs`,
so F1/F2/F4/F3(a) are untouched and remain closed.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F15 | non-blocking | test-gap | noted | The checkout arm's **wiring** is still unverified, as distinct from its **logic**, which F13 closed. Deleting the entire `.before_acquire(…)` registration from `connect_with_expected_database` leaves all seven tests green — only a compiler warning about an unused `profile` betrays it. So nothing would catch an accidental removal of the per-checkout half of §2.1's assertion, or a mis-wiring of its `profile.checks_pool_identity()` gate. Deliberately **not** blocking: the shipped wiring was read and is correct, `Profile::checks_pool_identity()` is unit-tested, and the helper it calls is now properly tested — this is missing coverage of correct code, not wrong behaviour. It is also genuinely awkward to test, because `after_connect` catches any mismatch before a checkout can observe one (the structural limitation the test's own doc comment already explains), so the honest fix is a refactor rather than another assertion. Recorded permanently so a later reviewer can promote it by citing this row. | M5: removing the `.before_acquire(…)` block and its two closure variables from `src/db.rs` → `cargo test` still reports 4 passed + 3 passed, 0 failed. | Extract hook construction into a small named factory returning the closure (or `None` when `!profile.checks_pool_identity()`) and unit-test the factory's shape per profile. Worth doing when this code is next opened, not on its own. |

**Disposition summary (scoped re-review #2):** 1 new finding — 0 blocking, 1 non-blocking →
**noted** (F15). F13 verified fixed and closed. All prior findings unchanged: F1–F4 and F3(a)
closed, F5/F6 carried by T-002, F11 fixed inline, F7–F10/F12/F14 standing as noted.

```
cost: estimated L, actual L
```

**Final tally across three passes:** 15 findings — 5 blocking (F1, F2, F3, F4, F13), all fixed and
verified; 10 non-blocking (2 → T-002, 1 fixed inline, 7 noted). Two of the five blocking findings
(F3, F13) were defects in test *credibility* rather than in shipped behaviour, and F13 was a defect
introduced by the rework of F3 — which is the argument for verifying a fix by breaking it rather
than by re-reading it.
| F5 | non-blocking | correctness | new ticket (T-002) | `db::with_database_name` derives the tenant URL by string-splitting on the last `/`, which silently discards any query string. `postgres://…/control?sslmode=require` becomes `postgres://…/tenant_acme` — every tenant pool in any non-local deployment would quietly drop its TLS and connect options. Harmless today (local dev only), and exactly the kind of defect that survives to production unnoticed. | `src/db.rs:87-92`; its own doc comment concedes "local-dev URLs only", yet `provision_tenant` is the production provisioning path (§11.4). | Parse instead of split: `base_url.parse::<PgConnectOptions>()?.database(database_name)`, which is already the type `connect_with_expected_database` uses two functions away. |
| F6 | non-blocking | design | new ticket (T-002) | Nothing writes `platform_audit`, though §4.11 names provisioning as its first purpose ("provisioning, suspension, break-glass") and §11.4 lists the audit trail as a platform-console surface. Provisioning is currently the only auditable platform action that exists, and it goes unrecorded. | `migrations/control/0001_control_schema.sql:43-50` creates the table; `grep -r platform_audit src/` returns nothing. | Write one `platform_audit` row per provisioning run (actor from the invoking operator, action `tenant.provision`, `detail` carrying slug/region/database_name and whether the database was created or already existed). |
| F7 | non-blocking | design | noted | `Config::from_env()` runs before `Cli::parse()`, so `messgr-control --help` and a bare invocation panic on a missing `CONTROL_DATABASE_URL` instead of printing usage. Masked in the repo because `dotenvy` finds `.env` in the cwd; it reproduces from anywhere else. | From `/tmp`: `env -u CONTROL_DATABASE_URL …/messgr-control --help` → `panicked at src/config.rs:21: CONTROL_DATABASE_URL must be set`. `src/bin/control.rs:37-38` | Swap the two lines — parse argv first, load config after. Left as a finding rather than a rework item because it changes behaviour and so fails the `fixed inline` bar. |
| F8 | non-blocking | stale-xref | noted | The migration carries §4.11's comment `region … must match this control DB's region; asserted on boot`, but no such assertion exists and there is no per-control-DB region setting to assert against. Copying §4.11 verbatim was Task 1's instruction, so this is not a deviation — recording it so the comment is not later read as shipped behaviour. | `migrations/control/0001_control_schema.sql:9` vs. `grep -rn region src/` (no assertion) | Lands naturally with the control-plane boot config in a later step; leave the comment as the design's statement of intent. |
| F9 | non-blocking | design | noted | `Command::Migrate` reports success only through `tracing::info!`, and `tracing_subscriber::fmt::init()` filters on `RUST_LOG`, which is unset in `.env.example` and CI — so `just control-migrate` prints nothing whatsoever on success. `provision` correctly uses `println!`. An operator command that is silent on success is indistinguishable from one that did nothing. | `src/bin/control.rs:35,53`; observed during the acceptance re-run. | Either `println!` the outcome (consistent with `provision`) or set a default filter, e.g. `EnvFilter::try_from_default_env().unwrap_or_else(\|_\| "info".into())`. |
| F10 | non-blocking | design | noted | `serde` and `serde_json` are now unused, as are the `serde` features on `uuid` and `chrono` (`grep` finds no `Serialize`/`Deserialize`/`json!` in `src/` or `tests/`). Decision 3 removed `axum` on precisely this reasoning and stopped one dependency short. | `Cargo.toml:17-18,26-27` vs. `grep -rn "serde" src/ tests/` (no hits) | Drop them; the first ticket that needs a JSON body re-adds them in the same commit that uses them. |
| F11 | non-blocking | stale-xref | fixed inline | `.gitignore` still carried the Playwright block (`node_modules/`, `/test-results/`, `/playwright-report/`, `/blob-report/`, `/playwright/.cache/`, `/playwright/.auth/`) after Task 9 deleted `tests/api/`. Dead ignores this branch made false; no behaviour change. | `.gitignore` before commit `d56de3a` | Fixed during review: commit `d56de3a` on the ticket branch removes the block. |
| F12 | non-blocking | spec-unclear | noted | Two contradictory policies for malformed configuration: `Profile::from_env()` panics on an unrecognized `MESSGR_PROFILE` (deliberately, per its own doc comment), while `Config::from_env()` silently substitutes `20` for an unparseable `DATABASE_MAX_CONNECTIONS` — under a doc comment that claims it "panics with a clear message" and that "failing loudly at startup beats failing confusingly on first use". The comment describes the behaviour of one field and not the other. | `src/profile.rs:23-29` vs. `src/config.rs:13-25` (`.and_then(\|v\| v.parse().ok()).unwrap_or(20)`) | Pick one policy and make the comment match. Loud failure is the more consistent choice given `Profile`. |

**Disposition summary:** 12 findings — 4 blocking (F1, F2, F3, F4; not dispositioned, they are the
rework scope), 8 non-blocking: 2 → **new ticket** (F5, F6, batched as T-002), 1 → **fixed inline**
(F11, commit `d56de3a`), 5 → **noted** (F7, F8, F9, F10, F12).

```
cost: estimated L, actual L
```

### Notes not rising to findings

- The `i64` schema version bound into `tenant_schema_version.version` (`int`) was suspected to be a
  type mismatch; verified working against Postgres 18 — no finding.
- `sqlx::migrate!` against a `.gitkeep`-only directory compiles and runs, recording version `0`.
  Task 2's fallback ("add a comment-only `.sql` file instead") was correctly not needed.
- Task 1's schema is byte-faithful to DESIGN.md §4.11, comments included.
- The branch introduces no `tickets/` path (`git log main..HEAD --name-only | grep '^tickets/'` is
  empty), so the `layout = "in-tree"` pre-push check is satisfied. Note that `pickle hooks install`
  has **not** been run in this clone — the check is currently manual only.

## History

- 2026-08-20 — created (TO DO). source: chat: build order step 0b (DESIGN.md §14) — first ticket of the from-scratch rebuild against the current design, replacing the pre-design SMS prototype.
- 2026-08-20 — TO DO → READY: implementation plan complete
- 2026-08-20 — plan amended inline: applicability-gate audit (fresh sub-agent) found Task 8 had `messgr-control` self-migrate the control DB on every invocation, contradicting §13's "never automatic on process start" — changed so only the `migrate` subcommand runs `sqlx::migrate!`, `provision` fails loudly if the schema isn't applied yet. Also fixed two smaller plan-text bugs in Task 9: `compose.yml`'s healthcheck wasn't updated alongside `POSTGRES_DB: control`, and the `justfile` `migrate`/`migrate-revert` recipes were missing `--source migrations/control`. Remaining audit notes (sqlx empty-migrations-dir behavior, `after_connect`/`before_acquire` signatures, `CREATE DATABASE` from a non-`postgres` connection, delete-list completeness) confirmed true as written — noted, no plan change needed.
- 2026-08-21 — READY → IN DEVELOPMENT: picked up
- 2026-08-21 — IN DEVELOPMENT → IN REVIEW: acceptance test green
- 2026-08-21 — IN REVIEW → REWORK: review: 4 blocking findings (F1 registry divergence on mismatched re-provision, F2 README just-provision invocation wrong, F3 two-tenant suite missing the §14 cross-database and checkout assertions, F4 broken duplicate justfile migrate recipes); 8 non-blocking dispositioned — 2 -> T-002, 1 fixed inline, 5 noted
- 2026-08-21 — REWORK → IN REVIEW: findings F1-F4 fixed; 7/7 tests green
- 2026-08-21 — IN REVIEW → REWORK: scoped re-review: F1, F2, F4 and F3(a) verified fixed; F13 blocking — the F3 checkout-arm unit test is tautological (passes with no database); F14 noted
- 2026-08-21 — REWORK → IN REVIEW: F13 fixed: checkout-arm test now fails without a database and fails under mutation; verified both directions
- 2026-08-21 — IN REVIEW → DONE: scoped re-review #2: F13 verified fixed by 5 mutations; F15 noted; all 5 blocking findings across 3 passes closed
