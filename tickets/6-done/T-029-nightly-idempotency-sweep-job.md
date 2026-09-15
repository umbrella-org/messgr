---
id: T-029
title: Nightly idempotency-sweep job
project: messgr
depends-on: []
spawned-by: [T-022]
impact: low
complexity: low
cost: S
---

# T-029 — Nightly idempotency-sweep job

## Outcome

After this ships, the `idempotency` table is actually bounded at the 30-day retention DESIGN.md
§4.3 already promises ("retained 30 days, swept nightly"), instead of growing forever.

## Description

`idempotency` (§4.3) has never had its sweep built. T-009 shipped the table and T-011 shipped
the write path, and both deferred the nightly sweep without either claiming it — it has sat
unowned since. Every row currently lives forever; nothing deletes an expired one. This is
narrow, bounded hygiene work, not a design question: a scheduled job (or a `messgr-control`
subcommand invoked by an external cron, matching this project's existing operational pattern —
see `partition-lifecycle run`) that runs `DELETE FROM idempotency WHERE expires_at < now()` per
tenant, on a nightly cadence. No gate-chain, consent, or encryption surface is touched — the
table holds no PII (the key and `comms_request_id` are opaque; the request payload itself lives
in `comms_request`).

T-022 rescoped `idempotency`'s primary key to `(producer_id, key)` (already merged) — this
ticket's sweep query is unaffected either way, since it deletes on `expires_at` alone.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-029-idempotency-sweep
```

### Prerequisite gate (hard)

None. No `depends-on:`; nothing else must land first.

### Confirmed design decisions (do not deviate without asking)

1. **Mirrors `partition-lifecycle`'s exact shape** (`src/partition_lifecycle/lifecycle.rs`,
   `src/bin/control.rs`'s `PartitionLifecycle` subcommand): a `messgr-control` subcommand,
   per-tenant, one-shot, meant to run on an external schedule (cron/systemd timer) — this binary
   does not daemonize or loop.
2. **`as_of: DateTime<Utc>` is an explicit parameter**, not read from `Utc::now()` inside the
   sweep function — mirrors `partition_lifecycle::lifecycle`'s own decision 4, so the acceptance
   test can synthesize expired/not-yet-expired rows deterministically instead of waiting on wall
   clock time.
3. **No Vault/keystore connection.** `idempotency` holds no PII (the ticket's own Description:
   the key and `comms_request_id` are opaque) — matches `partition-lifecycle`'s own "no Vault
   client connected, touches neither Transit nor AppRole" precedent.
4. **A bare `DELETE`, no soft-delete or archive.** Nothing reads an expired idempotency row ever
   again — DESIGN.md §4.3 only promises the 30-day retention window, not an audit trail of swept
   rows.

### Tasks

#### Task 1 — sweep function (`src/idempotency_sweep.rs`)

New file, single function (no `model.rs`/`repo.rs` split — one query doesn't warrant it):

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

#[derive(Debug)]
pub enum SweepError {
    Database(sqlx::Error),
    UnknownTenant(String),
}

// Display/Error impls matching partition_lifecycle::PartitionLifecycleError's shape
// (Database variant wraps sqlx::Error; UnknownTenant carries the slug).

/// Deletes every `idempotency` row whose `expires_at` is at or before `as_of`,
/// for the tenant named by `tenant_slug`. Returns the number of rows removed.
pub async fn run_for_tenant(
    control_pool: &PgPool,
    control_database_url: &str,
    tenant_slug: &str,
    as_of: DateTime<Utc>,
    max_connections: u32,
) -> Result<u64, SweepError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await
        .map_err(SweepError::Database)?
        .ok_or_else(|| SweepError::UnknownTenant(tenant_slug.to_string()))?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        control_database_url,
        tenant.id,
        &tenant.database_name,
        max_connections,
    )
    .await
    .map_err(SweepError::Database)?
    .pool;

    sweep(&tenant_pool, as_of).await
}

async fn sweep(tenant_pool: &PgPool, as_of: DateTime<Utc>) -> Result<u64, SweepError> {
    let result = sqlx::query("DELETE FROM idempotency WHERE expires_at <= $1")
        .bind(as_of)
        .execute(tenant_pool)
        .await
        .map_err(SweepError::Database)?;

    Ok(result.rows_affected())
}
```

Register in `src/lib.rs`: add `pub mod idempotency_sweep;` (alphabetical, between `health` and
`ingest`).

#### Task 2 — `messgr-control idempotency-sweep run` subcommand (`src/bin/control.rs`)

- Add to the `Command` enum, next to `PartitionLifecycle`:

  ```rust
  /// Deletes idempotency rows past their retention window (DESIGN.md §4.3:
  /// "retained 30 days, swept nightly"). Meant to run on a schedule
  /// (cron/systemd timer) — this binary does not daemonize or loop. T-029.
  IdempotencySweep {
      #[command(subcommand)]
      command: IdempotencySweepCommand,
  },
  ```

- New enum, next to `PartitionLifecycleCommand`:

  ```rust
  #[derive(Subcommand)]
  enum IdempotencySweepCommand {
      /// Delete every idempotency row whose expires_at is at or before now.
      Run {
          #[arg(long = "tenant-slug")]
          tenant_slug: String,
      },
  }
  ```

- Handler, next to the `PartitionLifecycle` arm (same "no Vault client" comment style):

  ```rust
  Command::IdempotencySweep { command } => match command {
      IdempotencySweepCommand::Run { tenant_slug } => {
          let deleted = messgr::idempotency_sweep::run_for_tenant(
              &control_pool,
              &config.control_database_url,
              &tenant_slug,
              chrono::Utc::now(),
              config.database_max_connections,
          )
          .await
          .map_err(|err| {
              format!("idempotency sweep failed for tenant {tenant_slug:?}: {err}")
          })?;

          println!("deleted={deleted}");
      }
  },
  ```

  `run_for_tenant` takes `config.database_max_connections` as its fifth parameter and threads
  it straight into `connect_tenant_pool`. (Applicability-gate note: `partition_lifecycle`
  itself hardcodes `5` at this call site rather than reading `Config` — there is no existing
  precedent to mirror here, so this ticket makes its own call to thread `Config` through
  instead, since a hardcoded literal has no upside over the field that already exists for it.)

#### Task 3 — docs (`docs/user-manual/control-plane-cli.adoc`)

Add a new section immediately after the existing "partition-lifecycle" section (they are the
same kind of scheduled-hygiene command), mirroring its structure exactly:

```
[source,bash]
----
cargo run --bin messgr-control -- idempotency-sweep run --tenant-slug acme
----

`idempotency` (DESIGN.md §4.3) rows are retained 30 days from the write that created them, then
swept. This command (T-029) deletes every row whose `expires_at` is at or before the time it
runs, for one tenant. No Vault/Transit dependency -- the table holds no PII. Safe to re-run;
meant to be invoked on a schedule (cron/systemd timer), not run continuously -- this binary does
not daemonize.
```

### Acceptance test

New file `tests/idempotency_sweep.rs`, following `tests/partition_lifecycle.rs`'s conventions
(real provisioning against the local stack, no mocks; `idempotency` has no foreign-key
constraints — rows can be inserted directly with fabricated `producer_id`/`comms_request_id`
UUIDs, no need to provision a producer):

```rust
#[tokio::test]
async fn sweep_deletes_only_rows_past_their_expiry() {
    // provision a real test tenant (tests/partition_lifecycle.rs's own helpers)
    // insert one idempotency row with expires_at in the past, one with expires_at
    // in the future
    // call idempotency_sweep::run_for_tenant(..., as_of: <a fixed instant between the two>)
    // assert: exactly 1 row deleted, the expired row is gone, the future one remains
}
```

Run:

```
just build
just test    # includes tests/idempotency_sweep.rs
just lint
just docs-check
```

### Docs update (mandatory when user-facing)

`docs/user-manual/control-plane-cli.adoc` — see Task 3 above.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated per Task 3.
3. Write a summary: files touched (`src/idempotency_sweep.rs`, `src/lib.rs`,
   `src/bin/control.rs`, `tests/idempotency_sweep.rs`,
   `docs/user-manual/control-plane-cli.adoc`), decisions made (the four above), anything
   deferred (none — this ticket is fully bounded).
4. Suggested commit message:

   ```
   feat(control): add idempotency-sweep run subcommand (T-029)

   Deletes idempotency rows past their 30-day retention window
   (DESIGN.md §4.3), per tenant, mirroring partition-lifecycle's
   scheduled-command shape. Meant to run on cron/systemd timer.
   ```

5. Root-path child (`path = "."`) — tidy WIP commits into atomic ones before presenting.
6. Commit locally on `feat/T-029-idempotency-sweep`. Publish only per commit policy (no push/MR
   without user approval). Present the commit message; after approval, verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing, then push and open the MR. Hand back to the user.

## Review

- [x] Reviewer independence settled (step 0): I authored branch `feat/T-029-idempotency-sweep`
  this session, so steps 2–4a were **delegated** to an independent sub-agent (fresh, no memory
  of writing the code, briefed adversarially against the ticket + the review addendum). Every
  delegated finding below was re-verified by hand before recording.
- [x] Implementation audit (steps 1–2): all four tasks done in the named files/shape. Acceptance
  test re-run (`cargo test --test idempotency_sweep`) — green. `just build` — green. `just test`
  (full suite) — green (no Vault approle gap hit on the independent reviewer's run). `just lint`
  — green, zero warnings. Every confirmed design decision honoured (mirrors
  `partition_lifecycle`'s shape including pool close-on-return; explicit `as_of`; no Vault
  connection; bare DELETE).
- [x] Quality audit (step 3): idiomatic; `SweepError` has correct `Debug`/`Display`/`Error`
  impls; no `unwrap`/`expect` on the production path. PII claim independently verified against
  `migrations/tenant/0004_ledger_outbox_schema.sql:57-63` — `idempotency` has exactly
  `producer_id`/`key`/`comms_request_id`/`expires_at`, nothing else.
- [x] Consistency audit (step 4): CLI wiring, comment style, and pool-lifecycle handling
  (`tenant_pool.pool.close().await`) all match `partition_lifecycle`'s sibling pattern. No stale
  cross-references inside the new code itself.
- [x] Documentation audit (step 4a): `just docs-check` could not run in this environment
  (`snowball` binary not installed — pre-existing local gap, not a code defect); the new
  "Idempotency sweep" section was instead checked by hand against the sibling "Partition
  lifecycle" section (heading level, code-fence, prose style) and confirmed correct. Whole-tree
  grep for "idempotency" found no other stale/duplicate coverage.
- [ ] Docs-readability pass (step 4b): no docs-readability reviewer configured in this host —
  conscious skip.
- [x] Findings recorded below, with severity/class/disposition and a cost line (step 5).
- [x] Ticket moved (step 6).
- [x] Governing documents reconciled (step 7) — see F2.
- [x] Impact sweep (step 8) — see below.
- [x] Summary + commit message/MR attributes presented for approval (step 9).

### Findings

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | test-gap | fixed inline | Acceptance test only inserted rows strictly before/after `as_of`, so it passed equally against a `<=`-vs-`<` boundary inversion — confirmed by mutating the operator and re-running (test stayed green). | `tests/idempotency_sweep.rs` (pre-fix); mutation test on `src/idempotency_sweep.rs:70` | Add a row at exactly `as_of` and assert it is swept too. |
| F2 | non-blocking | stale-xref | fixed inline | `development/design/03-data-model.md` §4.3 still said "the sweep job itself is not yet built" after this ticket built it. | `development/design/03-data-model.md:127` (pre-fix) | Reword to name the shipped command; bump `DESIGN.md`'s version stamp per the review addendum step 5. |

Disposition summary: 2 findings, both `fixed inline` (F1, F2). No findings folded, spawned, or
merely noted.

cost: estimated S, actual S

**F1 fix** — commit `b77bf23` on `feat/T-029-idempotency-sweep` (adds the boundary row +
strengthens the assertion; re-verified the mutation now fails the test).

**F2 fix** — commit `2fc839b` on `main` (governing-document reconciliation is overarching
bookkeeping, per `AGENTS.md`'s "docs" carve-out — committed straight to the base branch, not the
feature branch).

### Impact sweep (step 8)

`tickets/2-ready/T-030-reconcile-orphan-event-rows-into-comms-event.md` cites T-029 repeatedly as
a shape precedent ("mirrors T-029's shape exactly", docs section placed "after `idempotency-sweep`'s
(Task 3 of T-029)", CLI wiring "same file"). Checked against what actually shipped: the
`IdempotencySweep`/`IdempotencySweepCommand` wiring lives in `src/bin/control.rs` as T-030 assumes,
and the "Idempotency sweep" doc section sits immediately before "Message stats" — exactly where
T-030's own planned section would land right after it. No assumption invalidated; T-030 needs no
patch.

## History

- 2026-09-04 — created (TO DO). source: field-use: spawned while refining T-022, which named this deferred, unowned sweep job (deferred by T-009 and T-011, neither claiming it) as a follow-up to file if no ticket already existed.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — plan amended inline: applicability-gate audit (independent sub-agent) found all
  9 load-bearing assumptions still hold; two non-blocking findings fixed inline —
  `run_for_tenant` signature/call site given the `max_connections` parameter Task 2 already
  called for (Task 1's sample omitted it), and the Description's "T-022 rescopes" reworded to
  past tense (T-022 is merged). A third finding — the `SweepError::UnknownTenant`
  "matches partition_lifecycle" claim is loose (that error has no such variant) — is noted and
  closed; the plan's own error shape is fine as written, no ticket change needed.
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
- 2026-09-15 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-15 — IN REVIEW → DONE: review clean, 2 non-blocking findings fixed inline
