---
id: T-007
title: tenant_config table + typed config loading
project: messgr
depends-on: [T-001]
spawned-by: []
impact: medium
complexity: low
cost: S
---

# T-007 — tenant_config table + typed config loading

## Outcome

After this ships, every tenant has a typed configuration record — retention window,
timezone/locale defaults, schedule horizon, verification mode, staleness bound, quota day
boundary — that later gates, the ingest path, and the dispatcher read instead of hardcoded
values.

## Description

Add the `tenant_config` table (one row per tenant, in the tenant database) plus typed config
loading on top of it: retention, timezone/locale defaults, schedule horizon, verification mode
(`enforce`/`observe`), staleness bound, and quota day boundary (design §4.10). This is the first
ticket of build step 2 (ledger, outbox, ingest, one channel, encryption from the first write —
`PLAN.md`) and the umbrella of that step's ticket family (`family: T-007`): T-008 through T-014
all belong to it. Several downstream tickets read fields this table defines — T-009's schema
work, T-012's provider selection, T-026's quiet-hours resolution — so the column set should
anticipate those without inventing fields no ticket yet needs.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-007-tenant-config
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented (rules
§0). Do not push and do not open a merge request without explicit user approval. Ticket and board
bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-001` is in `6-done/` and merged to `main` — confirmed at refinement (control database,
  tenant registry, `provision_tenant`, `connect_tenant_pool` all exist and are exercised by
  `tests/tenancy.rs`).
- `T-005` is in `6-done/` and merged — this ticket follows its established two-file-per-concern
  shape (`model.rs`/`repo.rs`) and its decision 6 (no `tenant_id` column inside a tenant-database
  table).
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init` — the
  integration tests provision real tenants.

### Confirmed design decisions (do not deviate without asking)

1. **Six columns only — `retention_years`, `default_timezone`, `default_locale`,
   `schedule_horizon_days`, `quota_day_boundary_tz`, `verification_mode`, `staleness_max_age`.**
   These are the fields the ticket and PLAN.md name. DESIGN.md §4.10's full `tenant_config`
   table also shows `display_name` and `oidc_issuer`/`oidc_client_id`/`oidc_group_claim`; none
   has a reader yet (`oidc_*` arrives with the OIDC ticket, T-035, per PLAN.md's own
   `depends-on: [T-007]` row for it). Confirmed with the user during refinement: do not invent
   columns no ticket needs yet.
2. **No `tenant_id` column**, matching T-005 decision 6. `tenant_config` lives inside the
   tenant's own database (§2.1) — the tenant already is the database.
3. **Singleton enforced at the schema level, not just by convention.**
   `singleton boolean NOT NULL DEFAULT true` with `PRIMARY KEY (singleton)` and
   `CHECK (singleton)` — a second row is a primary-key violation, not a bug that surfaces later
   as "which row do I read". `repo::load`'s query still says `WHERE singleton` for readability
   even though it is the only legal value.
4. **No auto-seeding at provision time.** `provision_tenant` (`src/tenant/provision.rs`) is
   untouched. Several of these columns have no platform-wide default DESIGN.md has actually
   settled — "Still open" #2 (`staleness_max_age`) and #8 (`quota_day_boundary_tz`) are
   explicitly undecided, and `retention_years`/timezone/locale defaults are per-tenant by
   design (§4.10: "Everything an institution can differ on"). Confirmed with the user: a fresh
   tenant has **no** `tenant_config` row until an operator sets one explicitly.
   `tenant_config::repo::load` therefore returns `Option<TenantConfig>` — `None` means "not
   configured yet". Deciding what a gate/loader does with `None` belongs to the ticket that
   reads this table for real (T-009, T-012, the gate chain), not this one.
5. **`verification_mode` is a `text` column validated at the CLI boundary, not a new Rust
   enum.** `tenant::model::status` already established the codebase's convention for a
   closed-set DB-backed string: plain `String` field + a `pub mod` of `&'static str` constants.
   `tenant_config::verification_mode::{ENFORCE, OBSERVE}` follows that exactly. The CLI rejects
   any other value via `clap`'s `PossibleValuesParser` before it ever reaches the database.
6. **`staleness_max_age` is Postgres `interval`, read as `sqlx::postgres::types::PgInterval`
   (built into sqlx — no new dependency).** The CLI accepts only a whole-seconds count
   (`--staleness-max-age-seconds`), which `configure::set_tenant_config` turns into a
   `PgInterval` with `months: 0, days: 0`. A freshness bound (§4.8) has no calendar-length
   component to represent, so this ticket never populates `months`/`days` — only `microseconds`.
   `TenantConfig::staleness_max_age_duration() -> chrono::Duration` converts on that assumption
   and documents it inline.
7. **`schedule_horizon_days`/`verification_mode` CLI flags are optional**, defaulting to
   DESIGN.md's own SQL defaults (`90`, `"observe"`) when omitted — the SQL `DEFAULT` clauses
   exist for direct-SQL use (tests, `psql`); the CLI mirrors them rather than requiring an
   operator to restate them every time.
8. **`set_tenant_config` is a plain upsert** (`INSERT ... ON CONFLICT (singleton) DO UPDATE`,
   the same shape as `tenant::repo::record_schema_version`), auditing exactly one
   `tenant_config.set` `platform_audit` row per call with outcome `created` (no prior row),
   `updated` (prior row, different values), or `idempotent` (prior row, identical values) —
   determined by an `repo::load` immediately before the upsert. **Learning from the T-005/F1
   review finding**, an unknown `--tenant-slug` also writes a `rejected` audit row
   (`tenant_id: None`) before returning the error, rather than skipping the audit on that path
   as T-005's first cut did.

### Tasks

#### Task 1 — Tenant migration

Create `migrations/tenant/0002_tenant_config.sql`:

```sql
-- Tenant-scoped typed configuration (DESIGN.md §4.10, T-007): one row per
-- tenant, singleton-enforced. No tenant_id column, matching
-- 0001_producer.sql (§2.1) — the tenant already is the database.
-- display_name and the oidc_* columns from DESIGN.md's full §4.10 table are
-- deliberately not created yet: no ticket reads them (T-035 adds the oidc_*
-- columns when real OIDC lands).
CREATE TABLE tenant_config (
    singleton             boolean  NOT NULL DEFAULT true,
    retention_years       int      NOT NULL,
    default_timezone      text     NOT NULL,
    default_locale        text     NOT NULL,
    schedule_horizon_days int      NOT NULL DEFAULT 90,
    quota_day_boundary_tz text     NOT NULL,
    verification_mode     text     NOT NULL DEFAULT 'observe',  -- enforce | observe (§5)
    staleness_max_age     interval NOT NULL,                    -- projection freshness bound (§4.8)
    CONSTRAINT tenant_config_singleton CHECK (singleton),
    PRIMARY KEY (singleton)
);
```

No new runner needed — `provision_tenant` already runs `sqlx::migrate!("./migrations/tenant")`
against every tenant; an existing dev tenant picks this up on its next (idempotent) re-provision.

#### Task 2 — Model

Add `src/tenant_config/model.rs`:

- `TenantConfig` — `#[derive(Debug, Clone, sqlx::FromRow)]`, one field per column except
  `singleton`: `retention_years: i32`, `default_timezone: String`, `default_locale: String`,
  `schedule_horizon_days: i32`, `quota_day_boundary_tz: String`, `verification_mode: String`,
  `staleness_max_age: sqlx::postgres::types::PgInterval`. Add
  `pub fn staleness_max_age_duration(&self) -> chrono::Duration` per decision 6, with a doc
  comment stating the months/days-are-always-zero assumption.
- `TenantConfigInput` — the same seven fields, `#[derive(Debug, Clone, PartialEq)]`, used by
  both `repo::upsert` and `configure::set_tenant_config`'s created/updated/idempotent comparison
  (compare each field of a loaded `TenantConfig` against the new `TenantConfigInput`
  field-by-field; `PgInterval` derives `PartialEq` in sqlx, so `staleness_max_age` compares
  directly).
- `pub mod verification_mode { pub const ENFORCE: &str = "enforce"; pub const OBSERVE: &str = "observe"; }`
  per decision 5, following `src/tenant/model.rs`'s `pub mod status` shape.

#### Task 3 — Repository

Add `src/tenant_config/repo.rs`, following `src/producer/repo.rs`'s shape:

- `pub async fn load(pool: &PgPool) -> Result<Option<TenantConfig>, sqlx::Error>` — `SELECT`
  the seven columns `WHERE singleton`, `fetch_optional`.
- `pub async fn upsert(pool: &PgPool, input: &TenantConfigInput) -> Result<(), sqlx::Error>` —
  `INSERT INTO tenant_config (singleton, ...) VALUES (true, ...) ON CONFLICT (singleton) DO
  UPDATE SET ...` for all seven columns, mirroring
  `tenant::repo::record_schema_version`'s `ON CONFLICT` shape.

#### Task 4 — Configure operation

Add `src/tenant_config/configure.rs`, following `src/producer/register.rs`'s shape:

- `ConfigureError` enum + `Display`/`Error`/`From<sqlx::Error>` impls, in the shape of
  `ProducerError`.
- `pub struct ConfigureOutcome { pub outcome: &'static str }` — `"created"` | `"updated"` |
  `"idempotent"` (never `"rejected"`, returned as `Err` instead, per decision 8).
- `pub async fn set_tenant_config(control_pool: &PgPool, base_db_url: &str, tenant_slug: &str, input: TenantConfigInput, profile: Profile, actor: &str) -> Result<ConfigureOutcome, ConfigureError>`:
  resolve `tenant_slug` via `tenant::repo::find_by_slug`; on `None`, audit a `rejected` row
  (`tenant_id: None`) and return an error (decision 8's proactive fix); on `Some`, open the
  tenant pool via `connect_tenant_pool`, call `repo::load`, compare against `input`, `upsert`
  when not already identical, audit `created`/`updated`/`idempotent` accordingly (detail JSON:
  the seven new values plus the outcome), close the pool.
- `pub async fn show_tenant_config(control_pool: &PgPool, base_db_url: &str, tenant_slug: &str, profile: Profile) -> Result<Option<TenantConfig>, ConfigureError>` — resolve, connect, `repo::load`, close; mirrors `list_producers`'s shape.

#### Task 5 — Wiring

Add `src/tenant_config/mod.rs` (`pub mod configure; pub mod model; pub mod repo;`) and register
`pub mod tenant_config;` in `src/lib.rs`.

#### Task 6 — `messgr-control tenant-config` subcommands

Extend `src/bin/control.rs` with a `TenantConfig { command: TenantConfigCommand }` variant:

- `Set { tenant_slug, retention_years: i32, default_timezone: String, default_locale: String, schedule_horizon_days: Option<i32>, quota_day_boundary_tz: String, verification_mode: Option<String>, staleness_max_age_seconds: i64, actor: String }`.
  `--verification-mode` uses `#[arg(value_parser = clap::builder::PossibleValuesParser::new([tenant_config::model::verification_mode::ENFORCE, tenant_config::model::verification_mode::OBSERVE]))]`
  per decision 5; default `"observe"` and `90` when the optional flags are omitted, per decision
  7. Builds a `TenantConfigInput`, calls `set_tenant_config`, prints `outcome=<outcome>`.
- `Show { tenant_slug }` — calls `show_tenant_config`; prints each typed field on success, or
  `not configured` when `None`.

No Vault client is connected for either arm (same reason `Migrate`/`Producer` arms do not touch
Vault).

#### Task 7 — Integration tests

Add `tests/tenant_config.rs`, following `tests/producer.rs`'s conventions exactly
(`unique_name`, real provisioning via `provision_tenant`, `drop_test_tenant`-style best-effort
cleanup). Cover:

1. `load` returns `None` immediately after provisioning, before any `set_tenant_config` call.
2. `set_tenant_config` then `repo::load` round-trips every typed field, including
   `staleness_max_age_duration()` matching the seconds count supplied.
3. Re-`set_tenant_config` with identical inputs is idempotent: `outcome == "idempotent"` on the
   second call, and `SELECT count(*) FROM tenant_config` stays `1`.
4. Re-`set_tenant_config` with a different `retention_years` returns `outcome == "updated"`,
   `repo::load` reflects the new value, and the row count stays `1`.
5. `set_tenant_config` against an unknown `--tenant-slug` is rejected **and** writes a
   `tenant_config.set`/`rejected` `platform_audit` row with `tenant_id IS NULL` — the T-005/F1
   regression pattern, asserted from the start this time.

Add a small unit test in `src/bin/control.rs` (`#[cfg(test)] mod tests`, using
`Cli::try_parse_from`) asserting `tenant-config set` without `--schedule-horizon-days`/
`--verification-mode` parses to `90`/`"observe"` (decision 7), and that an invalid
`--verification-mode` value fails to parse (decision 5).

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/tenant_config.rs and the new control.rs unit tests
```

Then exercise the CLI end to end against a real tenant:

```
just provision acme eu tenant_acme operator@example.com
cargo run --bin messgr-control -- tenant-config set --tenant-slug acme \
    --retention-years 7 --default-timezone Europe/London --default-locale en-GB \
    --quota-day-boundary-tz Europe/London --staleness-max-age-seconds 7200 \
    --actor operator@example.com
cargo run --bin messgr-control -- tenant-config show --tenant-slug acme
```

Expected: `set` prints `outcome=created`; `show` prints all seven typed fields, with
`schedule_horizon_days=90` and `verification_mode=observe` (defaults applied). Re-running the
identical `set` command prints `outcome=idempotent`. Running it again with a different
`--retention-years` prints `outcome=updated`.

Verify the row and the audit trail:

```
psql postgres://messgr:messgr@localhost:5432/tenant_acme -c "SELECT * FROM tenant_config"
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT action, detail->>'outcome' FROM platform_audit WHERE action = 'tenant_config.set' ORDER BY at"
```

Expected: exactly one `tenant_config` row; three `platform_audit` rows (`created`, `idempotent`,
`updated`) in order.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control tenant-config` subcommands.

- `README.md` — add a "### Tenant configuration" section after "### Producers", documenting
  `tenant-config set|show`, the singleton-per-tenant shape, that a fresh tenant has no row until
  configured, and the created/updated/idempotent outcomes.
- `justfile` — add `tenant-config-set`/`tenant-config-show` recipes in the `control-plane`
  group, mirroring `producer-register`/`producer-list`.
- `DESIGN.md` §4.10 — correct the `tenant_config` `CREATE TABLE` snippet to match the shipped
  schema: replace `tenant_id uuid PRIMARY KEY,` with the `singleton`/`CHECK`/`PRIMARY KEY` shape
  from Task 1, mirroring how §4.9's `producer` table was already corrected for the same reason
  when T-005 shipped. Leave `display_name`/`oidc_*` in the snippet (still the eventual design,
  per decision 1) but add an inline comment on each naming that it is not yet created (`oidc_*`:
  "added by T-035").

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README, justfile, and DESIGN.md §4.10 updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(tenant-config): add tenant_config table and typed config loading (T-007)

   Adds the second tenant migration (tenant_config, DESIGN.md §4.10) scoped to
   the six fields PLAN.md names, a typed model/repository, and a
   messgr-control tenant-config set|show subcommand. No auto-seeding at
   provision time — several fields have no platform-wide default DESIGN.md
   has settled, so a fresh tenant has no row until an operator configures
   one. Every set call writes a platform_audit row (created/updated/
   idempotent), including a rejected one for an unknown tenant slug.
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration / model+repo / configure+CLI / tests / docs is a natural split)
   before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; umbrella of the step-2 ticket family (T-007–T-014)
- 2026-08-31 — TO DO → READY: plan complete
- 2026-08-31 — READY → IN DEVELOPMENT: picked up
