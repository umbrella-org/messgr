---
id: T-002
title: Provisioning path: preserve connection options and write the platform_audit trail
project: messgr
depends-on: []
spawned-by: [T-001]
impact: medium
complexity: low
cost: S
---

# T-002 — Provisioning path: preserve connection options and write the platform_audit trail

## Outcome

After this ships, a tenant pool connects with the same TLS and connection options as the
control pool it was derived from rather than silently dropping them, and every provisioning
run leaves a row in `platform_audit` — so the platform console (§11.4) has a trail from the
first tenant onward instead of one backfilled later.

## Description

Two gaps in the provisioning path shipped by T-001, both recorded as non-blocking findings in
that ticket's `## Review` (F5 and F6). Batched because they live in the same two files
(`src/db.rs`, `src/tenant/provision.rs`) and are one sitting of work together.

**1. Connection options are silently discarded (F5, `correctness`).** `db::with_database_name`
derives a tenant URL by string-splitting the base URL on its last `/`, so any query string is
lost: `postgres://…/control?sslmode=require` becomes `postgres://…/tenant_acme`. Today every
URL is a local-dev one with no query string, so nothing is broken — but the first staging or
production deployment would connect every tenant pool without TLS and without any other
connect option, and it would do so silently. The fix is to stop treating the URL as a string:
`base_url.parse::<PgConnectOptions>()?.database(database_name)` produces the same value with
every other option preserved, and `PgConnectOptions` is already the type
`db::connect_with_expected_database` accepts two functions away. `db::connect` (the control
pool) should be checked for the same treatment.

**2. `platform_audit` is never written (F6, `design`).** §4.11 introduces the table with the
comment "provisioning, suspension, break-glass" and §11.4 lists the audit trail as a platform
console surface. T-001 created the table and the provisioning command, and wired neither to
the other. Provisioning is currently the only auditable platform action that exists. One row
per run: actor (the invoking operator — how that identity is obtained on a CLI with no auth
realm yet is the open question this ticket must settle, and the honest interim answer may be
an explicit `--actor` flag rather than an inferred one), action `tenant.provision`, `tenant_id`,
and a `detail` payload carrying slug, region, database_name, and whether the database was
created or already existed. The row belongs on the idempotent re-run path too — a second
provision of the same tenant is an operator action worth seeing.

**Third path to audit, added after T-001's rework.** T-001 shipped with a guard (its review
finding F1) that *rejects* a re-provision of an existing slug carrying a different `region`
or `database_name`, returning an error instead of silently creating an orphan database. So
`provision_tenant` now has three outcomes, not two: created, idempotent no-op, and rejected.
A rejected provisioning attempt is an operator error against the tenant registry and is
arguably the most audit-worthy of the three — decide explicitly whether it writes a
`platform_audit` row, and note that it currently returns early *before* any audit write would
naturally sit. Do not simply wrap the happy path and leave the rejection silent.

Soft coupling, no hard dependency: this edits code T-001 introduced, so it wants T-001 merged
first to avoid a conflict, but it encodes no assumption T-001 could invalidate. Note that
T-001's rework pass (blocking findings F1–F4) also touches `src/tenant/provision.rs`.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-002-provisioning-path-preserve-connection-options-and-write-the-platform-audit-trail
```

All work on this branch, in this repo (`project: messgr`, `path = "."`, `layout = "in-tree"`).
Commit locally as you go. Do not push or open a merge request without explicit user approval
(this project's commit policy). Before pushing, verify the remote base is not behind:
`git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` must print
nothing.

### Prerequisite gate (hard)

T-001 (`depends-on` is empty, but this ticket edits code T-001 introduced) is in `6-done/` with
a `MERGED` History line (`main`, commit `d740537` et al.) — confirmed. No hard dependency, no
blocking prerequisite. Working tree must be clean before starting.

### Confirmed design decisions (do not deviate without asking)

1. **Actor identity is an explicit, required CLI flag (`--actor`), never inferred.** No auth
   realm exists yet for `messgr-control`; a free-text operator identifier supplied on
   invocation is the honest interim, per the ticket's own Description.
2. **Every one of `provision_tenant`'s three outcomes writes exactly one `platform_audit`
   row.** Created and idempotent no-op share action `tenant.provision`, distinguished by
   `detail.outcome` (`"created"` | `"idempotent"`); the F1 rejection path writes a distinct
   action `tenant.provision_rejected` and does so *before* returning its `Err`, since that
   branch returns early.
3. **Audit writing lives in a new top-level `src/platform_audit.rs` module, not under
   `src/tenant/`.** The table is platform-scoped (`tenant_id` is nullable; its schema comment
   reads "provisioning, suspension, break-glass") — future suspension and break-glass work
   reuses the same `record` function; it is not tenant-specific.
4. **The F5 fix restructures `with_database_name` to return
   `Result<PgConnectOptions, sqlx::Error>` instead of a `String`.**
   `db::connect_with_expected_database` takes the already-parsed `PgConnectOptions` directly
   rather than reparsing a string, closing the loss at the type level rather than patching the
   one call site. `db::connect` (the control pool) needs no equivalent change: it already hands
   its full URL straight to `PgPoolOptions::connect`, with no string-splitting step to lose
   anything — checked, not just assumed.
5. **No transaction wraps a provisioning run's audit write.** Every other step in
   `provision_tenant` is already its own auto-committed statement — there is no existing
   transaction to join. The audit write follows that same non-transactional style rather than
   introducing a new mechanism for this ticket alone.
6. **If the audit write itself fails, its error propagates instead of the operation's own
   result.** On the rejection path this means an audit-write failure surfaces in place of the
   rejection error; on the success path it means an otherwise-correct provisioning run can
   still return `Err` if the trailing audit insert fails. Accepted deliberately: a silent audit
   gap is the exact defect this ticket exists to close, so failing loudly beats swallowing it
   to protect the caller's happy path.

### Tasks

#### Task 1 — `platform_audit` module

New file `src/platform_audit.rs`:

```rust
use chrono::Utc;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Writes one row to `platform_audit` (DESIGN.md §4.11) for any platform-level
/// action. Provisioning is the only caller today; suspension and break-glass
/// content access (§7.6, §11.4) call this same function once those
/// subsystems exist. Not tenant-scoped — `tenant_id` is nullable because some
/// platform actions (e.g. a platform-wide kill switch) have none.
pub async fn record(
    pool: &PgPool,
    actor: &str,
    action: &str,
    tenant_id: Option<Uuid>,
    detail: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO platform_audit (id, actor, action, tenant_id, detail, at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(actor)
    .bind(action)
    .bind(tenant_id)
    .bind(detail)
    .bind(Utc::now())
    .execute(pool)
    .await
    .map(|_| ())
}
```

Register it in `src/lib.rs`: add `pub mod platform_audit;`.

#### Task 2 — Fix `with_database_name` (F5), `src/db.rs`

Replace the string-splitting implementation and its doc comment:

```rust
/// Swaps in a tenant's database name on a base connection URL, preserving
/// every other connection option (TLS mode, application name, etc.) —
/// review finding T-001/F5. Parsing into `PgConnectOptions` and back out
/// through its own builder means nothing carried in the URL, including a
/// query string, is lost the way a naive string split would lose it.
pub fn with_database_name(
    base_url: &str,
    database_name: &str,
) -> Result<PgConnectOptions, sqlx::Error> {
    let options: PgConnectOptions = base_url.parse()?;
    Ok(options.database(database_name))
}
```

Change `connect_with_expected_database`'s first parameter from `database_url: &str` to
`options: PgConnectOptions`, and delete its own internal `let options: PgConnectOptions =
database_url.parse()?;` line (the caller now supplies it already parsed). Update its doc
comment's reference to "a connection string" accordingly.

Replace the existing unit test `with_database_name_swaps_the_final_segment` (its assertion no
longer compiles against the new return type) with:

```rust
#[test]
fn with_database_name_swaps_the_database_and_keeps_everything_else() {
    let options = with_database_name(
        "postgres://messgr:messgr@localhost:5432/control",
        "tenant_acme",
    )
    .expect("parsing a well-formed URL must not fail");

    assert_eq!(options.get_database(), Some("tenant_acme"));
    assert_eq!(options.get_host(), "localhost");
    assert_eq!(options.get_port(), 5432);
    assert_eq!(options.get_username(), "messgr");
}

/// Review finding T-001/F5: the previous string-split implementation
/// silently dropped every connection option carried in the query string.
/// `sslmode=require` is the one that would have gone unnoticed in
/// production — proven here by parsing it back out after the swap.
#[test]
fn with_database_name_preserves_query_string_options() {
    let options = with_database_name(
        "postgres://messgr:messgr@localhost:5432/control?sslmode=require",
        "tenant_acme",
    )
    .expect("parsing a well-formed URL must not fail");

    assert_eq!(options.get_database(), Some("tenant_acme"));
    assert_eq!(
        format!("{:?}", options.get_ssl_mode()),
        format!("{:?}", sqlx::postgres::PgSslMode::Require)
    );
}
```

#### Task 3 — Adapt `connect_tenant_pool`, `src/tenant/pool.rs`

```rust
pub async fn connect_tenant_pool(
    base_db_url: &str,
    database_name: &str,
    max_connections: u32,
    profile: Profile,
) -> Result<PgPool, sqlx::Error> {
    let options = db::with_database_name(base_db_url, database_name)?;
    db::connect_with_expected_database(options, max_connections, database_name, profile).await
}
```

#### Task 4 — Wire actor + audit into `provision_tenant`, `src/tenant/provision.rs` (F6)

Add `actor: &str` as a new trailing parameter. Restructure the outcome match to carry an
outcome tag, write the rejection audit row before its early return, and write the
created/idempotent audit row right before the final `Ok(tenant_id)`:

```rust
pub async fn provision_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    slug: &str,
    region: &str,
    database_name: &str,
    profile: Profile,
    actor: &str,
) -> Result<Uuid, sqlx::Error> {
    let (tenant_id, outcome) = match repo::find_by_slug(control_pool, slug).await? {
        Some(tenant)
            if tenant.region == region && tenant.database_name == database_name =>
        {
            (tenant.id, "idempotent")
        }
        Some(tenant) => {
            crate::platform_audit::record(
                control_pool,
                actor,
                "tenant.provision_rejected",
                Some(tenant.id),
                serde_json::json!({
                    "slug": slug,
                    "attempted_region": region,
                    "attempted_database_name": database_name,
                    "existing_region": tenant.region,
                    "existing_database_name": tenant.database_name,
                }),
            )
            .await?;

            return Err(sqlx::Error::Configuration(
                format!(
                    "tenant {slug:?} is already registered with region={:?} database_name={:?}; \
                     refusing to re-provision it with region={region:?} database_name={database_name:?} \
                     (provisioning is idempotent only when re-run with identical inputs)",
                    tenant.region, tenant.database_name,
                )
                .into(),
            ));
        }
        None => {
            // ... unchanged insert-provisioning block ...
            (id, "created")
        }
    };

    ensure_database_exists(control_pool, database_name).await?;
    // ... unchanged migrate / record_schema_version / mark_active block ...

    crate::platform_audit::record(
        control_pool,
        actor,
        "tenant.provision",
        Some(tenant_id),
        serde_json::json!({
            "slug": slug,
            "region": region,
            "database_name": database_name,
            "outcome": outcome,
        }),
    )
    .await?;

    Ok(tenant_id)
}
```

Update the function's doc comment to mention the audit trail and the `actor` parameter.

#### Task 5 — CLI `--actor` flag, `src/bin/control.rs`

Add to the `Provision` variant:

```rust
Provision {
    #[arg(long)]
    slug: String,
    #[arg(long)]
    region: String,
    #[arg(long = "database-name")]
    database_name: String,
    /// Operator identity recorded on the platform_audit row. No auth realm
    /// exists yet for messgr-control, so this is supplied explicitly rather
    /// than inferred.
    #[arg(long)]
    actor: String,
},
```

Pass `&actor` through to `provision_tenant(...)` in the `Command::Provision` match arm.

#### Task 6 — `justfile` and `README.md`

`justfile`'s `provision` recipe gains an `actor` parameter:

```
provision slug region db actor:
    cargo run --bin messgr-control -- provision --slug {{slug}} --region {{region}} --database-name {{db}} --actor {{actor}}
```

`README.md`'s Local development example updates its invocation to
`just provision acme eu tenant_acme operator@example.com` (any illustrative actor value).

#### Task 7 — Update `tests/tenancy.rs` for the new signatures, and add audit coverage

Update every existing `provision_tenant(...)` call to pass a trailing actor argument (e.g.
`"test-actor"`), and update `a_mis_wired_pool_trips_the_current_database_assertion` to the new
`with_database_name`/`connect_with_expected_database` signatures:

```rust
let options = db::with_database_name(&control_url, &db_name)
    .expect("parsing the control URL failed");
let wrong_expected = "not_the_real_database";

let result = tokio::spawn(async move {
    db::connect_with_expected_database(options, 2, wrong_expected, Profile::Dev).await
})
.await;
```

Add three new `#[tokio::test]`s (same file, reusing `unique_name`/`drop_test_tenant`) asserting
the audit trail end to end against a real `platform_audit` row:

- `provisioning_writes_a_platform_audit_row` — provision once; query
  `SELECT action, detail->>'outcome' FROM platform_audit WHERE tenant_id = $1` and assert one
  row, `action = "tenant.provision"`, `outcome = "created"`.
- `idempotent_reprovision_writes_a_second_platform_audit_row` — provision twice with identical
  `region`/`database_name`; assert two rows for that `tenant_id`, ordered by `at`, the second
  with `outcome = "idempotent"`.
- `rejected_reprovision_writes_a_platform_audit_row` — provision once, then again with a
  different `database_name`; assert the second call returns `Err`, and that exactly one
  `platform_audit` row with `action = "tenant.provision_rejected"` and
  `detail->>'attempted_database_name'` equal to the second call's value exists for that
  `tenant_id`.

Clean up each test's rows from `platform_audit` in the existing `drop_test_tenant` cleanup
helper (add a `DELETE FROM platform_audit WHERE tenant_id = $1`, best-effort like its siblings)
so the suite stays hermetic under repeated local runs.

### Acceptance test

```
cargo fmt --check
cargo clippy -- -D warnings
cargo build
cargo test
```

All green, including specifically:

- `db::tests::with_database_name_swaps_the_database_and_keeps_everything_else`
- `db::tests::with_database_name_preserves_query_string_options`
- `db::tests::assert_current_database_panics_on_the_mismatch_before_acquire_would_catch`
  (unaffected in behaviour; proves the surrounding module still compiles against the new
  `with_database_name`/`connect_with_expected_database` signatures)
- `two_tenants_are_isolated_by_database`, `work_on_tenant_as_pool_never_reads_or_writes_tenant_bs_database`,
  `a_mis_wired_pool_trips_the_current_database_assertion` (updated call sites)
- `provisioning_writes_a_platform_audit_row`
- `idempotent_reprovision_writes_a_second_platform_audit_row`
- `rejected_reprovision_writes_a_platform_audit_row`

Manual smoke check (not part of `cargo test`): `cargo run --bin messgr-control -- provision
--slug acme --region eu --database-name tenant_acme` (no `--actor`) must be rejected by clap
with a missing-required-argument error; adding `--actor smoke-test` must succeed, and
`psql $CONTROL_DATABASE_URL -c "select actor, action, detail from platform_audit order by at
desc limit 1"` must show the resulting row.

### Docs update (mandatory when user-facing)

`README.md`'s Local development section: update the `just provision` example invocation to
include the new `actor` argument (Task 6). No other user-facing surface exists yet — the
platform console (§11.4) that will eventually display this trail is not built.

### Finish (mandatory)

1. Acceptance test green; `cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo build`
   clean.
2. `README.md` updated per the Docs update step above.
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggest a Conventional Commit message, ticket id in brackets, e.g.:
   ```
   fix(provisioning): preserve connection options and write platform_audit (T-002)
   ```
5. This is a root-path child (`path = "."`) — interactive-rebase WIP commits into a small
   number of atomic, correctly typed/scoped commits before presenting them (replaces
   squash-on-merge for this repo).
6. Commit locally on `feat/T-002-provisioning-path-preserve-connection-options-and-write-the-platform-audit-trail`.
   Do not push or open a merge request without user approval. Present the commit message; only
   after approval, verify the remote base is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` must print nothing), then push and open
   the merge request. Merging is always the human's. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-21 — created (TO DO). source: review: T-001 review findings F5 (tenant URLs silently drop query-string connection options, e.g. `sslmode`) and F6 (`platform_audit` created but never written), batched by theme — both are provisioning-path completeness in `src/db.rs` / `src/tenant/provision.rs`.
- 2026-08-21 — description amended by T-001's review impact sweep: T-001's F1 fix added a *rejection* path to `provision_tenant` (mismatched re-provision of an existing slug now errors instead of proceeding), so the `platform_audit` work has three outcomes to cover rather than two. `db::with_database_name` (F5) was not touched by that rework and this ticket's plan for it stands unchanged.
- 2026-08-21 — TO DO → READY: plan complete
- 2026-08-21 — READY → IN DEVELOPMENT: picked up
- 2026-08-21 — IN DEVELOPMENT → IN REVIEW: acceptance green
