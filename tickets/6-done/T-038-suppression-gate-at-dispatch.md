---
id: T-038
title: Suppression gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-038 — Suppression gate at dispatch

## Outcome

After this ships, a destination on the suppression list (hard bounce, complaint, regulatory hold)
no longer receives any further send: the dispatcher checks `destination_hmac` against a
suppression list at send time and blocks with a terminal `suppressed_list` event.

## Description

Third of the three regulatory gates from DESIGN.md §5 (build-order step 5; see T-036 for the
family context). **No schema exists yet** — needs a new `suppression` table keyed on
`destination_hmac` (deliberately the raw address hash, not `address_id` or `customer_id` — §5:
suppression is fail-safe, so over-suppressing a recycled number is the acceptable direction to
err). Applies to all classes per §5's table, unconditionally — confirmed during refinement:
unlike verification/consent, §5's gate table names no auth exemption for suppression, and
`class = "auth"` never reaches the outbox anyway (T-011 decision 3), so an unconditional check is
correct, not an oversight.

**Design-doc bug found during refinement.** The prose (`04-gate-chain.md`: "entries carry a
review date rather than living forever"; `06-pii-retention.md`: "until its own review date...
retires it independently") promises a review-date column that `03-data-model.md`'s own
`CREATE TABLE suppression` snippet never actually had — just `destination_hmac`/`reason`/
`added_at`. Confirmed with the user: the review date **auto-expires** the block (the gate query
itself stops matching once it passes — no sweep job needed), rather than being a manual-only
marker. Fixed as part of this ticket, with a correction note in `03-data-model.md`, per AGENTS.md's
instruction to correct an error like this in place and say so, not quietly patch it.

**Scope note — same population gap as T-037.** §5 lists the real sources as hard bounce,
complaint, and regulatory hold — the first two normally arrive via the webhook/delivery-receipt
path (build-order step 12, unbuilt). This ticket needs a minimal manual/CLI way to manage entries
— confirmed with the user: `messgr-control suppression add/list/remove` (mirroring the existing
config-CLI pattern; `remove` retires an entry early by the same mechanism as natural expiry —
moving `review_at` to now). Wiring automatic population from delivery receipts is out of scope
and belongs with step 12.

Runs after verification (T-036) and consent (T-037), per §5's ordering table. No hard dependency
on either — independently observable and buildable; WIP=1 serializes pickup anyway.

Migration note for whoever refines this: `migrations/tenant/0004_ledger_outbox_schema.sql`'s
header comment says "suppression (T-021)" — that referred to a planned ticket number from when
T-009 was written; T-021 ended up being a different, unrelated ticket ("Outbox lease lifecycle
and dispatcher retry with backoff"). Corrected here to point at T-038. The adjacent "Consent
(T-020)" reference in the same comment has the identical staleness (T-020 is also unrelated —
"Make tenant pool identity unrepresentable to mis-wire") but is out of scope here; flagging it for
whoever refines T-037 next.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-038-suppression-gate-at-dispatch
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1. Does not
require T-036/T-037 to be merged first — if neither has landed yet when this is picked up, this
ticket's own check is still added the same way (see decision 5).

### Confirmed design decisions (do not deviate without asking)

1. **Suppression applies to every message class, with no exemption.** §5's gate table lists no
   auth/transactional exception for suppression, unlike verification/consent which explicitly
   name one. No `matches!` class allowlist is added — `class = "auth"` never reaches the outbox
   anyway (T-011 decision 3), so an allowlist would just be dead branching. The check in
   `try_process` runs unconditionally for every claimed row.
2. **`review_at` is mandatory and auto-expiring, not a manual-only marker.** Confirmed with the
   user during refinement. An entry blocks only while `review_at > now()`; there is no separate
   "removed" storage state, so retiring an entry early (`suppression remove`) and letting one
   expire naturally are the exact same mechanism — moving `review_at` to now — and the gate query
   never needs a second condition to tell them apart.
3. **Design-doc correction, done as part of this ticket:** `development/design/03-data-model.md`'s
   `CREATE TABLE suppression` snippet gains a `review_at timestamptz NOT NULL` column (after
   `added_at`), with a short correction note (matching this doc's existing correction callouts)
   stating the column was missing despite being promised by the surrounding prose.
4. **The gate is one JOIN query, not a raw-HMAC lookup plus a separate suppression check.**
   `dispatcher::repo::is_suppressed(pool, created_at, comms_request_id)` joins `comms_request` to
   `suppression` on `destination_hmac` in a single round trip. No new field is added to
   `ClaimedOutbox` or `RequestCiphertexts`, and — unlike T-036's `verification_mode` — no new
   `DispatcherContext` field is needed either, since suppression has no per-tenant mode. **No
   existing `DispatcherContext { .. }` construction site (production or test) needs updating.**
5. **The check lives inline in `try_process` (`src/dispatcher/worker.rs`), at the very top,
   before `repo::load_ciphertexts`.** Cheapest-gate-first, matching T-036 decision 1's placement
   rationale — a blocked send never spends a decrypt or a DEK fetch. Ordering relative to
   T-036's/T-037's own in-line checks (whichever of them have landed by the time this is picked
   up) is left to pickup order, per this ticket's own Description — no hard dependency.
6. **Entries are added/listed/removed by raw destination, never by pre-computed HMAC bytes.**
   `messgr-control suppression add`/`remove` compute `destination_hmac` themselves via
   `destination_hmac::compute` + `tenant_pepper::ensure_tenant_pepper`, exactly mirroring how
   `messgr-ingest` and `customer::resolve` derive the same column. An operator never types or
   stores a raw HMAC value. `list` prints the stored `destination_hmac` hex-encoded — the HMAC is
   one-way, so there is nothing to reverse it back to an address (the same reason §5 keys
   suppression this way in the first place).
7. **`reason` is a closed vocabulary (`hard_bounce` | `complaint` | `regulatory_hold`), not free
   text.** Matches §5's three named sources and this codebase's own `class`/`verification_mode`
   precedent for a small closed-string-vocab module, validated by clap's
   `PossibleValuesParser` the same way `provider-config set --channel` is.
8. **`remove` requires an existing, currently-active entry for that destination.** A `retire_now`
   call that matches zero rows (already expired, or never suppressed) is a rejection, audited the
   same way `set_provider_config`'s unknown-tenant-slug case is (T-005/F1: audit the rejection,
   then error — never skip the audit just because the call returns early).

### Tasks

#### Task 1 — Design-doc correction

`development/design/03-data-model.md`: add `review_at timestamptz NOT NULL` to the
`CREATE TABLE suppression` snippet, and add a short correction note directly beneath it (matching
the doc's existing correction-callout style, e.g. the "comms_event's dedup constraint was inert"
one a few lines below) stating that the review-date column was missing from the table despite
being promised by `04-gate-chain.md`/`06-pii-retention.md`'s prose, found during T-038's
refinement.

#### Task 2 — Schema migration

`migrations/tenant/0013_suppression.sql`:

```sql
-- Suppression list (DESIGN.md §5, §4.4, T-038): hard bounces, complaints, and
-- regulatory holds, keyed on the raw destination_hmac -- deliberately not
-- address_id or customer_id, so a recycled destination cannot inherit or
-- escape a suppression entry that belongs to whoever held it before (§5).
-- review_at is mandatory: an entry blocks only while review_at > now(), so
-- retiring one early is the same UPDATE as letting it expire naturally --
-- no separate "removed" state, no sweep job needed.
CREATE TABLE suppression (
    destination_hmac bytea       PRIMARY KEY,
    reason            text       NOT NULL,  -- hard_bounce | complaint | regulatory_hold
    added_at          timestamptz NOT NULL,
    review_at         timestamptz NOT NULL
);
```

Also fix `migrations/tenant/0004_ledger_outbox_schema.sql`'s header comment: `suppression
(T-021)` → `suppression (T-038)`. Leave the adjacent `Consent (T-020)` reference alone (T-037's
own fix, out of scope here — see Description).

#### Task 3 — `suppression` module (model + repo)

New files, mirroring `src/provider_config/` exactly.

`src/suppression/model.rs`:

```rust
use chrono::{DateTime, Utc};

pub mod reason {
    pub const HARD_BOUNCE: &str = "hard_bounce";
    pub const COMPLAINT: &str = "complaint";
    pub const REGULATORY_HOLD: &str = "regulatory_hold";
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Suppression {
    pub destination_hmac: Vec<u8>,
    pub reason: String,
    pub added_at: DateTime<Utc>,
    pub review_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SuppressionInput {
    pub destination_hmac: Vec<u8>,
    pub reason: String,
    pub review_at: DateTime<Utc>,
}

impl SuppressionInput {
    /// `added_at` is excluded deliberately -- it's server-set on first
    /// insert and never part of caller input, so a re-`add` can only ever
    /// change `reason`/`review_at`.
    pub fn matches(&self, existing: &Suppression) -> bool {
        self.destination_hmac == existing.destination_hmac
            && self.reason == existing.reason
            && self.review_at == existing.review_at
    }
}
```

`src/suppression/repo.rs`:

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::model::{Suppression, SuppressionInput};

pub async fn list(pool: &PgPool) -> Result<Vec<Suppression>, sqlx::Error> {
    sqlx::query_as::<_, Suppression>(
        "SELECT destination_hmac, reason, added_at, review_at FROM suppression ORDER BY added_at",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_one(
    pool: &PgPool,
    destination_hmac: &[u8],
) -> Result<Option<Suppression>, sqlx::Error> {
    sqlx::query_as::<_, Suppression>(
        "SELECT destination_hmac, reason, added_at, review_at FROM suppression \
         WHERE destination_hmac = $1",
    )
    .bind(destination_hmac)
    .fetch_optional(pool)
    .await
}

pub async fn upsert(
    pool: &PgPool,
    input: &SuppressionInput,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO suppression (destination_hmac, reason, added_at, review_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (destination_hmac) DO UPDATE SET
            reason = EXCLUDED.reason,
            review_at = EXCLUDED.review_at
        "#,
    )
    .bind(&input.destination_hmac)
    .bind(&input.reason)
    .bind(now)
    .bind(input.review_at)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Retires an entry early by moving `review_at` to `now` -- a no-op (0 rows
/// affected) against an entry that's already expired or was never
/// suppressed, so the caller (`configure::remove_suppression`) can use the
/// row count to decide rejected vs. retired (T-038 decision 8).
pub async fn retire_now(
    pool: &PgPool,
    destination_hmac: &[u8],
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE suppression SET review_at = $2 WHERE destination_hmac = $1 AND review_at > $2",
    )
    .bind(destination_hmac)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
```

#### Task 4 — `suppression::configure` (actor-facing operations)

`src/suppression/configure.rs`, mirroring `provider_config::configure` (same `ConfigureError`/
`rejected`/`audit` shape):

```rust
pub async fn add_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    reason: &str,
    review_at: DateTime<Utc>,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> { /* resolve tenant (audit-then-reject on
    unknown, T-005/F1), connect tenant pool, ensure_tenant_pepper once, compute
    destination_hmac, repo::load_one to decide created/updated/idempotent, repo::upsert
    when not idempotent, audit, close tenant pool */ }

pub async fn remove_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    actor: &str,
) -> Result<(), ConfigureError> { /* resolve tenant, ensure_tenant_pepper, compute hmac,
    repo::retire_now; 0 rows affected -> audit "rejected" and return
    rejected("no active suppression entry for this destination") (decision 8);
    >0 -> audit "retired" and Ok(()) */ }

pub async fn list_suppression(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
) -> Result<Vec<Suppression>, ConfigureError> { /* resolve tenant, connect tenant pool,
    repo::list -- no pepper needed, nothing to reverse */ }
```

`add_suppression`/`remove_suppression` call `tenant_pepper::ensure_tenant_pepper(control_pool,
keystore, &tenant)` once each, then `destination_hmac::compute(&pepper, destination)` — identical
to `messgr-ingest`'s own derivation. `audit()` records `suppression.add` / `suppression.remove` on
`platform_audit`, same shape as `provider_config`'s.

#### Task 5 — register the module

`src/lib.rs`: add `pub mod suppression;` alongside `pub mod provider_config;`.

#### Task 6 — dispatcher gate

`src/dispatcher/repo.rs`: add

```rust
/// One round trip: joins the claimed row's own `comms_request.destination_hmac`
/// against `suppression`, so `try_process` never needs the raw HMAC bytes
/// itself (DESIGN.md §5, T-038). `review_at > now()` is the entire "still
/// blocking" condition -- an expired entry simply stops matching, no sweep
/// job needed (decision 2).
pub async fn is_suppressed(
    pool: &PgPool,
    created_at: DateTime<Utc>,
    comms_request_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM comms_request cr
            JOIN suppression s ON s.destination_hmac = cr.destination_hmac
            WHERE cr.created_at = $1 AND cr.id = $2 AND s.review_at > now()
        )
        "#,
    )
    .bind(created_at)
    .bind(comms_request_id)
    .fetch_one(pool)
    .await
}
```

`src/dispatcher/worker.rs`: at the very top of `try_process`, before the existing
`repo::load_ciphertexts` call:

```rust
if repo::is_suppressed(&ctx.pool, row.created_at, row.comms_request_id).await? {
    repo::write_terminal(
        &ctx.pool,
        row.created_at,
        row.comms_request_id,
        row.customer_id,
        "suppressed_list",
        None,
        None,
        "suppressed_list",
    )
    .await?;
    return Ok(());
}
```

No `DispatcherContext` field, no class check (decisions 1 and 4). No existing
`DispatcherContext { .. }` literal, production or test, needs touching.

#### Task 7 — CLI wiring (`src/bin/control.rs`)

Add a `Command::Suppression { command: SuppressionCommand }` variant, in the same style as
`ProviderConfig`/`TenantConfig`:

```rust
#[derive(Subcommand)]
enum SuppressionCommand {
    /// Add a new entry, or update an existing one's reason/review date.
    Add {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                reason::HARD_BOUNCE,
                reason::COMPLAINT,
                reason::REGULATORY_HOLD,
            ])
        )]
        reason: String,
        /// RFC 3339 timestamp; the entry stops blocking once this passes.
        #[arg(long = "review-at")]
        review_at: String,
        #[arg(long)]
        actor: String,
    },
    /// List every suppression entry for a tenant. destination_hmac is
    /// hex-printed -- it cannot be reversed to the original address.
    List {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
    /// Retire an entry early (sets review_at to now).
    Remove {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(long)]
        actor: String,
    },
}
```

In `run()`, a new arm — Vault-connecting, like `Provision`/`DevPki`:

```rust
Command::Suppression { command } => {
    let vault_keystore = VaultKeyStore::connect(config.profile).expect(
        "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
    );

    match command {
        SuppressionCommand::Add { tenant_slug, destination, reason, review_at, actor } => {
            let review_at = DateTime::parse_from_rfc3339(&review_at)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|err| format!(
                    "invalid --review-at {review_at:?} (expected RFC 3339): {err}"
                ))?;
            let outcome = add_suppression(
                &control_pool, &config.control_database_url, &tenant_slug,
                &vault_keystore, &destination, &reason, review_at, &actor,
            ).await.map_err(|err| format!(
                "failed to add suppression entry for tenant {tenant_slug:?}: {err}"
            ))?;
            println!("outcome={}", outcome.outcome);
        }
        SuppressionCommand::List { tenant_slug } => {
            let rows = list_suppression(&control_pool, &config.control_database_url, &tenant_slug)
                .await.map_err(|err| format!(
                    "failed to list suppression entries for tenant {tenant_slug:?}: {err}"
                ))?;
            if rows.is_empty() {
                println!("no suppression entries for tenant {tenant_slug}");
            } else {
                let now = Utc::now();
                for row in rows {
                    let status = if row.review_at > now { "active" } else { "expired" };
                    let hmac_hex: String =
                        row.destination_hmac.iter().map(|b| format!("{b:02x}")).collect();
                    println!(
                        "destination_hmac={hmac_hex} reason={} added_at={} review_at={} status={status}",
                        row.reason, row.added_at, row.review_at,
                    );
                }
            }
        }
        SuppressionCommand::Remove { tenant_slug, destination, actor } => {
            remove_suppression(
                &control_pool, &config.control_database_url, &tenant_slug,
                &vault_keystore, &destination, &actor,
            ).await.map_err(|err| format!(
                "failed to remove suppression entry for tenant {tenant_slug:?}: {err}"
            ))?;
            println!("outcome=retired");
        }
    }
}
```

**Pass `&vault_keystore` itself (it implements `KeyStore` directly), not `.client()`** —
`.client()` returns the raw `&VaultClient` that `dev_pki`/`provision_tenant` need for lower-level
Vault calls; `ensure_tenant_pepper` wants `&dyn KeyStore`. No new dependency for the hex print —
this codebase has no existing hex-encoding helper or crate, and one `format!("{b:02x}")` fold
over the bytes is not worth adding one for.

### Acceptance test

**`tests/dispatcher.rs`** — add a lower-level `write_outbox_row_with_hmac(tenant, vault, cache,
destination, body, destination_hmac: &[u8]) -> (Uuid, DateTime<Utc>, Uuid)` (identical body to
today's `write_ready_outbox_row`, but taking the HMAC as a parameter instead of the hardcoded
`b"unused-hmac"` literal); make `write_ready_outbox_row` a one-line wrapper calling it with
`b"unused-hmac"` — every existing call site is unaffected.

1. **`an_active_suppression_entry_blocks_the_send`.** Insert a `suppression` row directly
   (`destination_hmac = b"suppressed-address"`, `reason = 'hard_bounce'`,
   `review_at = now() + 1 day`); write an outbox row via `write_outbox_row_with_hmac(..., b"suppressed-address")`;
   claim + `try_process`. Assert: `comms_event` has exactly one row, `event_type =
   'suppressed_list'`; `comms_request.final_status = Some("suppressed_list")`; outbox row count 0;
   the wiremock mock (mounted with `.expect(0)`) never receives a request.
2. **`an_expired_suppression_entry_no_longer_blocks`.** Same insert, `review_at = now() - 1 hour`.
   Assert: normal `sent` event, `final_status = Some("sent")`, outbox row removed, mock called
   once — the direct proof of decision 2's auto-expiry.

**`tests/suppression.rs`** (new file, following `tests/provider_config.rs`'s conventions):

- `list_returns_empty_before_any_entry_is_added`
- `adding_and_listing_round_trips_every_typed_field`
- `adding_again_with_a_different_reason_updates_the_row_without_duplicating_it`
- `resetting_with_identical_inputs_is_idempotent`
- `removing_an_active_entry_retires_it` (assert `repo::load_one(...).review_at <= now()`
  afterward)
- `add_suppression_against_an_unknown_tenant_slug_is_rejected_and_audited` (mirrors
  `provider_config.rs`'s own T-005/F1 test)
- `remove_suppression_against_a_destination_with_no_active_entry_is_rejected`

**`src/bin/control.rs`'s existing `mod tests`** (clap-parsing only, no DB — mirrors the
`provider_config_set_parses...`/`provider_config_set_rejects_an_invalid_channel` pair):

- `suppression_add_parses_every_flag`
- `suppression_add_rejects_an_invalid_reason`
- `suppression_list_parses`
- `suppression_remove_parses`

Run: `just build && just test && just lint`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/control-plane-cli.adoc`: new `== Suppression list` section (after
  `== Provider configuration`, same style — command examples, then prose) documenting
  `add`/`list`/`remove`, the closed `reason` vocabulary, RFC 3339 `--review-at`, and that
  `remove` is exactly "move `review_at` to now."
- `docs/user-manual/dispatcher.adoc`: new paragraph in the `T-016`/`T-021` paragraph style,
  stating: the suppression gate runs at send time for every message class with no exemption;
  a destination present in `suppression` with `review_at` still in the future blocks with a
  terminal `suppressed_list` event; an entry with a past `review_at` no longer blocks.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` clean.
2. `docs/user-manual/control-plane-cli.adoc` and `docs/user-manual/dispatcher.adoc` updated per
   above.
3. Write a summary (files touched, decisions made, anything deferred) and hand back.
4. Suggested commit message:

   ```
   feat(dispatcher): enforce the suppression gate at send time (T-038)

   Adds the suppression table/module/CLI (add/list/remove, keyed on
   destination_hmac with a mandatory auto-expiring review_at) and wires an
   unconditional dispatch-time check into try_process: a currently-suppressed
   destination gets a terminal suppressed_list event instead of a send.
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on `feat/T-038-suppression-gate-at-dispatch`. Do not push or open an MR without
   user approval. Present the commit message; after approval, verify `origin/main...HEAD` carries
   no `tickets/` path, then push and open the MR. Merging is the human's.

## Review

- [x] Reviewer independence settled (step 0): **delegated** — the orchestrating reviewer
  authored this branch in this same session, so steps 2-4a's audits ran in a fresh sub-agent with
  no memory of writing the code, briefed adversarially. Every finding it reported was re-verified
  by hand (source read directly, or command re-run) before being recorded below.
- [x] Implementation audit — acceptance test re-run (`an_active_suppression_entry_blocks_the_send`,
  `an_expired_suppression_entry_no_longer_blocks`, all 7 of `tests/suppression.rs`), all green.
  All 7 Implementation Plan tasks verified done in the files they name; all 8 confirmed design
  decisions verified against the code, not the plan's prose (steps 1, 2).
- [x] Quality audit (step 3) — idiomatic, mirrors `provider_config`'s shape as intended; sound
  secret handling (HMAC one-way, never stored/logged raw); no injection risk (`sqlx` bind params
  throughout). Mutation-tested the two dispatcher-gate tests and the T-005/F1 audit-on-rejection
  test by describing the exact deletion/inversion that would flip each red — all three are
  falsifiable, none tautological.
- [x] Consistency audit (step 4) — project-wide grep for stale `T-021`/ticket-number references
  to `suppression` found only the one already fixed by Task 2. `tenant_id` naming unaffected
  (`suppression` has none, consistent with §2.1). Hard invariants 1 and 3 hold (gate runs at
  dispatch, in `try_process`, never at ingest; no auth-class exemption logic added because none
  was needed).
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a). New CLI
  surface and dispatcher behavior both documented and verified accurate against the code.
  `just docs-check` passes. Whole-tree sweep (beyond the two pages the ticket itself touched)
  turned up three places this branch's own migration made prose false — see F1/F3/F4 below.
- [ ] Docs-readability pass — no docs-readability reviewer configured in this environment;
  conscious skip (step 4b, optional, never blocks).
- [x] Findings recorded below with severity, class, and disposition; disposition summary and
  cost line present (step 5).
- [x] Ticket moved to `tickets/6-done/`; `## History` appended (step 6).
- [x] Other references updated; `03-data-model.md`'s `review_at` correction (Task 1) already
  reconciled the one governing-document gap this ticket's own filing found. The impact sweep
  (step 8) found and patched one more, in T-037 — see below (step 7).
- [x] Remaining-tickets impact sweep done (step 8) — `T-037`'s Description assumed suppression
  would land *after* it; T-038 landed first instead. Patched T-037's Description and History to
  say so — no other `1-to-do/`/`2-ready/` ticket references T-038.
- [x] Summary + child-project commit message & MR attributes presented for approval; remote-base
  check and overarching-repo bookkeeping to follow approval (step 9).

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | stale-xref | `tests/erasure_coverage.rs`'s own comments (x2) used `suppression` as their running example of a table that "doesn't exist yet" — false as of this branch's migration. | `tests/erasure_coverage.rs:227-231`, `:271-276` | Fixed inline: reworded both to past tense ("before its migration landed, T-038"), commit `bfe43c5`. |
| F2 | non-blocking | correctness | `suppression::configure::add_suppression`/`remove_suppression` leak the tenant `PgPool` if `ensure_tenant_pepper` fails after `connect_tenant_pool` already succeeded — no `.close()` on that error path, unlike `list_suppression` in the same file. No test exercises a Vault failure mid-call. Impact bounded: `messgr-control` is a short-lived CLI process, so the OS reclaims the sockets on exit. | `src/suppression/configure.rs:118-127` (add), `:197-206` (remove); contrast `:243-256` (list, correctly closes) | Bind `ensure_tenant_pepper`'s result before the `?`, close the pool, then propagate — same shape `add_suppression_inner`'s own call site already uses for its downstream `Result`. Not fixed inline: a functional change, not a prose/idiom one. |
| F3 | non-blocking | stale-xref | `docs/user-manual/ingest.adoc` said "Consent, quotas, and suppression (§5, §5.1) are still unbuilt" — false for suppression as of this branch (it's enforced, at dispatch time, not at ingest). | `docs/user-manual/ingest.adoc:10` (pre-fix) | Fixed inline: reworded to split consent/quotas (still unbuilt) from suppression (enforced at dispatch — cross-referenced to "messgr-dispatcher" rather than duplicated), commit `bfe43c5`. |
| F4 | non-blocking | stale-xref | `docs/user-manual/introduction.adoc`'s Status section listed `suppression` as part of "no gate chain ... yet" — false as of this branch. | `docs/user-manual/introduction.adoc:16-17` (pre-fix) | Fixed inline: split out the suppression clause as already enforced, commit `bfe43c5`. |
| F5 | non-blocking | stale-xref | The same sentence also lists `kill switches` as part of "no gate chain ... yet" — also false, but pre-existing (kill switches shipped in `T-016`, well before this branch): found during the same whole-tree sweep but not this branch's causation, so out of this ticket's inline-fix bar. | `docs/user-manual/introduction.adoc:16` | Leave for whoever next touches that page's Status section, or a documentation-accuracy sweep ticket if one is ever filed; not promoted alone — doesn't clear the batching bar by itself. |

Disposition summary: 5 non-blocking findings — 3 `fixed inline` (F1, F3, F4, commit `bfe43c5` on
`feat/T-038-suppression-gate-at-dispatch`), 2 `noted` (F2, F5). No blocking findings. No new
tickets spawned; one existing ticket (T-037) patched by the impact sweep (step 8), recorded in
its own History.

cost: estimated M, actual M

## History

- 2026-09-15 — created (TO DO). source: chat: filed alongside T-036/T-037 from a
  build-order-vs-shipped-tickets gap analysis — step 5 (the gate chain) is unbuilt despite steps
  0-4 and 17 later hardening tickets being done.
- 2026-09-16 — TO DO → READY: plan complete
- 2026-09-16 — READY → IN DEVELOPMENT: picked up
- 2026-09-16 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-16 — IN REVIEW → DONE: review clean, no blocking findings
- 2026-09-16 — PR #53 opened (`feat/T-038-suppression-gate-at-dispatch` → `main`)
