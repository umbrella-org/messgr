---
id: T-014
title: Partition lifecycle: create-ahead, move to slow tablespace, detach + drop
project: messgr
depends-on: [T-009]
spawned-by: []
family: T-007
impact: medium
complexity: medium
cost: M
---

# T-014 — Partition lifecycle: create-ahead, move to slow tablespace, detach + drop

## Outcome

After this ships, `comms_request` and `comms_event` partitions manage themselves: an operator
(or a cron/systemd timer, invoked regularly) runs one `messgr-control partition-lifecycle`
command per tenant that keeps the current and next month's partitions ready, moves partitions
older than 18 months to a slower (still writable) tablespace, and detaches and drops partitions
past the tenant's retention boundary — nobody manually manages ledger partitions.

## Description

Build the partition lifecycle per design §4.1, §7.2, §7.5: a create-ahead step, an 18-month move
to a slower (still writable) tablespace, and detach + drop at the tenant's retention boundary
(read from `tenant_config.retention_years`, T-007). Applies to **both** monthly-partitioned
tables T-009 created — `comms_request` and `comms_event` — not just the one the title names;
`outbox`/`idempotency` are plain tables (T-009 decision 4) and are out of scope. Part of the
step-2 ticket family (`family: T-007`; see T-007) — its last remaining ticket. Depends on T-009
for the partitioned tables and the `<table>_<YYYY>_<MM>` naming convention (T-009 decision 3)
this ticket extends going forward.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-014-partition-lifecycle
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented
(rules §0). Do not push and do not open a merge request without explicit user approval. Ticket
and board bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-009` is in `6-done/` and merged to `main` — provides the partitioned `comms_request`/
  `comms_event` tables and the `<table>_<YYYY>_<MM>` naming convention this ticket extends.
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init`, then
  `just tablespace-init` (new recipe, Task 1 below) — the integration tests provision real
  tenants and need the `messgr_cold` tablespace to exist.

### Confirmed design decisions (do not deviate without asking)

1. **Implemented as a `messgr-control partition-lifecycle run --tenant-slug <slug>` subcommand,
   not a new binary.** DESIGN.md §13 enumerates exactly six binaries; this doesn't add a
   seventh, matching the existing `Migrate`/`Provision`/`TenantConfig` precedent of
   control-plane maintenance operations living as subcommands. Triggering it on a schedule
   (cron/systemd timer, e.g. daily) is a deployment concern left to ops — this repo has no
   deploy/systemd artifacts for any binary yet, not even the dispatcher (T-013), so none is
   authored here either.
2. **One subcommand runs all three lifecycle steps — create-ahead, move, detach+drop — in
   sequence, every invocation.** Each step is cheap to check-and-skip when nothing is due, and
   this matches the Outcome's "nobody manually manages ledger partitions" framing: an operator
   or cron just runs it regularly and it self-limits to whatever's actually due.
3. **Partition age is determined by parsing the `<table>_<YYYY>_<MM>` name** (T-009 decision 3),
   not by querying the partition's constraint bounds via `pg_get_expr`. The convention exists
   for exactly this, and this ticket owns it going forward. A child partition whose name doesn't
   match `<table>_\d{4}_\d{2}` is skipped with a `tracing::warn!`, never moved or dropped —
   protects any partition created outside this convention from being silently touched.
4. **`as_of: DateTime<Utc>` is an explicit parameter threaded through every lifecycle function**,
   never read internally via `Utc::now()`. The CLI passes `Utc::now()`; tests pass fixed values
   so partitions dated "20 months old" or "8 years old" can be synthesized and asserted against
   deterministically, without waiting real time or mocking the clock globally.
5. **Create-ahead maintains exactly the current and next calendar month**, mirroring T-009's own
   bootstrap `DO` block (same two-iteration loop, same `to_char(..., 'YYYY_MM')` suffix) —
   re-run periodically rather than only once at provision time. Not tied to
   `tenant_config.schedule_horizon_days`: `comms_request.created_at`/`comms_event.occurred_at`
   are always "now" at insert time (T-011's ingest path — confirmed by reading
   `src/ingest/repo.rs`, `created_at` is `Utc::now()`), and `scheduled_for` only delays
   `outbox.next_attempt_at` — it never changes which partition a row lands in. So create-ahead
   only has to survive the calendar rolling over between runs; two months of headroom is enough
   for any cron cadence saner than "less than monthly."
6. **The 18-month move threshold is a fixed global constant, not read from `tenant_config`.**
   §7.5 states it as a platform-wide rule, unlike the drop boundary, which is explicitly
   per-tenant (`retention_years`).
7. **The drop threshold is `retention_years * 12` months, read from `tenant_config`. If a
   tenant has no `tenant_config` row (T-007 decision 4: fresh tenants start unconfigured), the
   drop step is skipped entirely for that tenant** — never inferred, never defaulted. Dropping a
   bank's ledger partitions on an assumed retention window is exactly the silent compliance hole
   AGENTS.md's hard invariants #3 and #6 exist to prevent. The move-to-slow-tablespace step still
   runs regardless (decision 6 — not gated on `tenant_config`).
8. **Detach uses plain `ALTER TABLE ... DETACH PARTITION ...`, not `DETACH PARTITION
   CONCURRENTLY`.** Concurrent detach exists to avoid locking out writers on a live partition; a
   partition old enough to cross the retention boundary is, per §4.1, "never rewritten and stays
   effectively frozen" — there is nothing concurrent to protect against, and plain `DETACH`
   avoids the two-step `FINALIZE` protocol concurrent detach requires if interrupted.
9. **Moving a partition's tablespace also moves its indexes.** `ALTER TABLE ... SET TABLESPACE`
   relocates only the table's heap, not its indexes — leaving indexes behind only half executes
   §7.2's "cold partitions on slower storage." Each of the partition's indexes (queried from
   `pg_indexes`) is relocated with its own `ALTER INDEX ... SET TABLESPACE ...` immediately
   after the table.
10. **The `messgr_cold` tablespace is a cluster-level object, not created by a tenant
    migration.** Postgres tablespaces are shared across every database in the instance, unlike
    everything else this codebase provisions per-tenant-database — a per-tenant migration would
    fail on the second tenant ("already exists"), and creating it from application code assumes
    filesystem privileges the tenant DB role shouldn't need. For local dev/CI, a new idempotent
    `just tablespace-init` recipe (Task 1) creates the directory and the tablespace once against
    the running `messgr-postgres` container, mirroring `vault-dev-init`'s shape (direct `docker
    exec`, `|| true` for repeatability). A real deployment provisions the equivalent tablespace
    once per Postgres cluster as part of its own ops runbook, the same way it provisions the
    cluster itself — out of scope for this ticket's code.

### Tasks

#### Task 1 — `just tablespace-init` recipe

Add to the `db` group in `justfile`, after `vault-dev-init`:

```
# Create the messgr_cold tablespace used by partition-lifecycle moves (T-014).
# Cluster-level, not per-tenant -- run once per Postgres instance, and again
# after `just db-reset`.
[group('db')]
tablespace-init:
    docker exec messgr-postgres mkdir -p /var/lib/postgresql/tablespaces/messgr_cold
    docker exec messgr-postgres chown postgres:postgres /var/lib/postgresql/tablespaces/messgr_cold
    docker exec messgr-postgres psql -U messgr -d control -c \
        "CREATE TABLESPACE messgr_cold LOCATION '/var/lib/postgresql/tablespaces/messgr_cold'" || true
```

#### Task 2 — Partition lifecycle module

Add `src/partition_lifecycle/{mod.rs, model.rs, repo.rs, lifecycle.rs}`, following
`src/customer_dek/`'s shape (error enum + `Display`/`Error`/`From<sqlx::Error>` in
`lifecycle.rs`, raw-query functions in `repo.rs`, no ORM/model heaviness — this operates on
`pg_catalog`, not a typed application table).

`model.rs`:
- `pub struct PartitionInfo { pub name: String, pub month_start: chrono::NaiveDate, pub tablespace: Option<String> }`
- `pub struct LifecycleReport { pub created: Vec<String>, pub moved: Vec<String>, pub dropped: Vec<String>, pub retention_skipped: bool }`

`repo.rs`:
- `pub async fn list_partitions(pool: &PgPool, parent_table: &str) -> Result<Vec<PartitionInfo>, sqlx::Error>` —
  joins `pg_inherits`/`pg_class`/`pg_tablespace` for `parent_table`'s children; parses each
  `relname`'s trailing `_YYYY_MM` in Rust into `month_start`, skipping (with `tracing::warn!`)
  any name that doesn't match `<parent_table>_\d{4}_\d{2}` (decision 3).
- `pub async fn partition_exists(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error>` —
  `SELECT to_regclass($1) IS NOT NULL`.
- `pub async fn create_partition(pool: &PgPool, parent_table: &str, month_start: chrono::NaiveDate) -> Result<(), sqlx::Error>` —
  computes the `<parent_table>_<YYYY>_<MM>` name and `[month_start, month_start + 1 month)`
  range, `CREATE TABLE <name> PARTITION OF <parent_table> FOR VALUES FROM (...) TO (...)`.
- `pub async fn move_to_tablespace(pool: &PgPool, partition_name: &str, tablespace: &str) -> Result<(), sqlx::Error>` —
  `ALTER TABLE <partition_name> SET TABLESPACE <tablespace>`, then queries `pg_indexes WHERE
  tablename = <partition_name>` and issues `ALTER INDEX <indexname> SET TABLESPACE <tablespace>`
  for each (decision 9).
- `pub async fn detach_and_drop(pool: &PgPool, parent_table: &str, partition_name: &str) -> Result<(), sqlx::Error>` —
  `ALTER TABLE <parent_table> DETACH PARTITION <partition_name>` then `DROP TABLE
  <partition_name>` (decision 8).

`lifecycle.rs`:
- `pub const MOVE_AFTER_MONTHS: i32 = 18;` (decision 6), `pub const COLD_TABLESPACE: &str =
  "messgr_cold";`, `const PARTITIONED_TABLES: [&str; 2] = ["comms_request", "comms_event"];`
  (Description — both tables).
- `PartitionLifecycleError` enum wrapping `sqlx::Error`, in the shape of `CustomerDekError`.
- `pub async fn run(pool: &PgPool, as_of: chrono::DateTime<chrono::Utc>, tenant_config: Option<&messgr::tenant_config::model::TenantConfig>) -> Result<LifecycleReport, PartitionLifecycleError>`:
  for each of `PARTITIONED_TABLES` — ensure the `as_of` and `as_of + 1 month` partitions exist
  (decision 5, via `create_partition` guarded by `partition_exists`); list partitions and move
  any whose `month_start + 1 month <= as_of - 18 months` and not already on `COLD_TABLESPACE`
  (decision 6); if `tenant_config` is `Some`, additionally detach+drop any whose `month_start + 1
  month <= as_of - (retention_years * 12) months` (decision 7 — independent of current
  tablespace, a partition may be dropped straight from either). If `tenant_config` is `None`,
  set `report.retention_skipped = true`, skip the whole drop phase, and
  `tracing::warn!("tenant_config not set -- skipping partition drop; partitions past retention \
  are being kept, not lost")`.

#### Task 3 — Wiring

Add `src/partition_lifecycle/mod.rs` (`pub mod lifecycle; pub mod model; pub mod repo;`) and
register `pub mod partition_lifecycle;` in `src/lib.rs`.

#### Task 4 — `messgr-control partition-lifecycle` subcommand

Extend `src/bin/control.rs` with:

```rust
/// Keep comms_request/comms_event partitions self-managing (DESIGN.md
/// §4.1, §7.2, §7.5, T-014): create-ahead, move to slow tablespace, detach
/// + drop at the tenant's retention boundary. Meant to run on a schedule
/// (cron/systemd timer) -- this binary does not daemonize or loop.
PartitionLifecycle {
    #[command(subcommand)]
    command: PartitionLifecycleCommand,
},
```
with a nested `PartitionLifecycleCommand::Run { tenant_slug: String }` — nested, not flat, to
match every other multi-word command in this file (`Producer`/`TenantConfig`/`CustomerDek`
all nest even a single-variant subcommand) and the `partition-lifecycle run --tenant-slug`
form this plan's decision 1 and the Acceptance test below already use.

Handler: resolution/connection/close is wrapped in a `partition_lifecycle::lifecycle::
run_for_tenant(control_pool, base_db_url, tenant_slug, as_of, profile)` function (mirroring
`tenant_config::configure::set_tenant_config`'s own resolve/connect/close shape) rather than
inlined in `control.rs` — every other command in this file delegates that wiring to its domain
module, and this one is no exception. It resolves `tenant_slug` via `tenant_repo::find_by_slug`
(error on unknown slug via `sqlx::Error::Configuration`, matching `ConfigureError`'s
`rejected()` helper — no `platform_audit` write, since this is a read/maintenance operation,
not a state change to a control-plane record, so it follows `Migrate`'s no-audit precedent, not
`Producer`/`TenantConfig`'s audited-write one). No Vault client connected in `control.rs`, for
the same reason `Migrate` doesn't connect one. The handler calls `run_for_tenant(&control_pool,
&config.control_database_url, &tenant_slug, chrono::Utc::now(), config.profile)` and prints
`created=<n> moved=<n> dropped=<n>` and, when `retention_skipped`, an additional line
`retention: skipped (tenant_config not set)`.

#### Task 5 — Integration tests

Add `tests/partition_lifecycle.rs`, following `tests/tenant_config.rs`'s conventions exactly
(`unique_name`, real `provision_tenant`, `drop_test_tenant`-style best-effort cleanup). Requires
`just tablespace-init` to have been run first — note this in the file's header comment and in
the Acceptance test below. Cover:

1. **Create-ahead is idempotent.** Calling `lifecycle::run` twice in a row immediately after
   provisioning does not error (no duplicate-partition creation) and both calls report the same
   two bootstrap partitions already existing.
2. **A partition inside the retention window moves but is not dropped.** Create a synthetic
   partition (via `repo::create_partition`) dated 20 months before `as_of`, for both
   `comms_request` and `comms_event`, and insert one row into each. Load a `tenant_config` with
   `retention_years: 7` (84 months — 20 < 84). After `run`, assert via `pg_class`/`pg_tablespace`
   that the partition's table *and* its indexes report `spcname = 'messgr_cold'`; assert via
   `pg_inherits` it is still attached; assert the inserted row still round-trips by direct
   `SELECT`.
3. **A partition past the retention boundary is detached and dropped.** Same setup, but the
   synthetic partition is dated 96 months (8 years) before `as_of`, with the same
   `retention_years: 7`. After `run`, assert `to_regclass('<name>')` is `NULL` and `pg_inherits`
   no longer lists it, for both tables.
4. **No `tenant_config` row means no drop, ever.** Repeat the 96-month-old partition setup with
   no `tenant_config` row (pass `None`). After `run`, assert the partition is untouched — still
   attached, still queryable — and `LifecycleReport::retention_skipped` is `true`.
5. **A non-conforming partition name is never touched.** Manually `CREATE TABLE ... PARTITION
   OF comms_request ...` with a name that doesn't match `comms_request_YYYY_MM` (e.g.
   `comms_request_legacy`), dated well past both thresholds. After `run`, assert it is still
   attached and on the default tablespace — proves decision 3's naming guard.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just tablespace-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/partition_lifecycle.rs
```

Then verify against a real tenant:

```
just provision acme eu tenant_acme operator@example.com
cargo run --bin messgr-control -- partition-lifecycle run --tenant-slug acme
```

Expected: prints `created=0 moved=0 dropped=0` (a freshly provisioned tenant has nothing due
yet — T-009 already bootstrapped the current/next month) plus `retention: skipped (tenant_config
not set)`. Re-running is a no-op with identical output.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control partition-lifecycle` subcommand and the new
`just tablespace-init` recipe.

- `README.md` — add a "### Partition lifecycle" section after "### Tenant configuration",
  documenting the subcommand, that it is meant to run on a schedule (cron/systemd timer — not
  built here), the 18-month move / retention-year drop rules, and the `retention_skipped`
  behavior when `tenant_config` is unset.
- `justfile` — the `tablespace-init` recipe (Task 1) plus a `partition-lifecycle-run` recipe in
  the `control-plane` group, mirroring `tenant-config-set`/`tenant-config-show`.
- `DESIGN.md` — no change expected: this implements §4.1/§7.2/§7.5 as written. If implementation
  forces a deviation, stop and raise it rather than editing the design to match the code.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README and justfile updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(partition-lifecycle): add create-ahead/move/drop job (T-014)

   Adds a messgr-control partition-lifecycle subcommand that keeps
   comms_request/comms_event partitions self-managing: ensures the
   current and next month exist, moves partitions past 18 months to
   the messgr_cold tablespace (indexes included), and detaches and
   drops partitions past the tenant's tenant_config.retention_years
   boundary. Skips the drop step entirely when a tenant has no
   tenant_config row, rather than assuming a default retention.
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (justfile / module / CLI wiring / tests / docs is a natural split) before
   presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-09-01 — TO DO → READY: plan complete
- 2026-09-01 — READY → IN DEVELOPMENT: picked up
- 2026-09-01 — plan amended inline: Task 4's code sketch showed a flat `PartitionLifecycle { tenant_slug }` variant, contradicting decision 1 and the Acceptance test's own `partition-lifecycle run --tenant-slug` usage; implemented as nested `PartitionLifecycle { command: PartitionLifecycleCommand }` with `Run { tenant_slug }`, and wrapped tenant resolution/connection in a `lifecycle::run_for_tenant` function mirroring `set_tenant_config`'s shape, keeping `control.rs` thin like every other command
