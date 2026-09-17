---
id: T-037
title: Consent gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-037 — Consent gate at dispatch

## Outcome

After this ships, marketing no longer sends to an address without a recorded opt-in: the
dispatcher checks `consent.opted_in` for (`address_id`, `class`) at send time and blocks
unconsented marketing with a terminal `suppressed_consent` event (transactional is unaffected —
it never required opt-in). An operator records an opt-in/opt-out with `messgr-control consent
set`, against a destination that has already resolved to a real address (a prior message, not
this command, is what creates one — see decision 3). A customer's consent records are erased,
alongside the rest of their ledger, on a physical-redaction erasure request.

## Description

Second of the three regulatory gates from DESIGN.md §5 (build-order step 5; see T-036 for the
family context). The `consent` table's schema already exists **in the design** —
`03-data-model.md` §4.4 has a correct `CREATE TABLE consent (address_id, class, opted_in, source,
updated_at)` snippet, keyed on `(address_id, class)` per AGENTS.md hard invariant 4 (keys on
`customer_address.id`, never `customer_id` or the raw address value, so a recycled number's new
address row starts with no consent record — absence of consent defaults to opted-out for
marketing, by design, not a bug to fix) — but **no migration has ever created it**; this ticket
writes that migration verbatim from the design. Transactional does not require opt-in (§5); auth
skips this gate entirely (AGENTS.md invariant 1) and never reaches the outbox anyway (T-011
decision 3).

**Scope note — the design's real consent source doesn't exist yet.** §5 says the master system
normally publishes consent via the customer event feed, but that feed consumer is still a stub
(build-order step 10, unbuilt). Building this gate against a data source that doesn't exist yet
would ship a gate nothing can ever satisfy. This ticket therefore also needs a minimal, explicit
way to write consent records: `messgr-control consent set`, mirroring the existing `tenant-config
set` / `provider-config set` CLI pattern (a plain upsert, not `suppression`'s add/list/remove —
there's no separate "remove" concept here, an opt-out is just `--opted-in false`, recorded as its
own row so the evidence trail (`source`) survives). Wiring the real event-feed consent source is
out of scope here and belongs to whatever ticket eventually builds step 10. §5 also mentions a
**consent pre-filter at bulk-campaign ingestion** — that depends on the bulk campaign path (step
18, unbuilt) and is out of scope for this ticket; the dispatch-time gate is authoritative
regardless.

**Design decision, confirmed during refinement: `consent set` never mints an address — it is
rejected against a destination with no active `customer_address` row.** The obvious alternative —
reusing `customer::resolve::resolve`'s `ResolutionInput::AddressOnly` path to mint a provisional
customer+address on demand, the way ingest does — was considered and rejected: it would let a
consent-recording call *win* a destination ahead of the real customer who eventually messages it.
Concretely, `customer::resolve::resolve`'s `Explicit`/`External` branches resolve an address via
`find_active_address_for_customer(pool, customer_id, ...)`, scoped to the customer_id *that call*
resolved — a different customer_id (the provisional shell `consent set` would have minted) never
matches, so `insert_address`'s `(kind, value_hmac) WHERE active_to IS NULL` unique index rejects
the real customer's insert as a lost race, and resolution falls back to
`find_active_address_by_hmac`, silently handing every future message to that destination to the
bogus provisional customer instead of the real one. Requiring a pre-existing address avoids this
entirely, and needs no Vault/DEK access at all — just the tenant pepper (already used for
suppression's own HMAC) and `customer::repo::find_active_address_by_hmac`.

**Design-doc gap found during refinement, fixed as part of this ticket: `consent` was omitted
from §7.2's erasure statements and from its named-exemption list — both a documentation gap and,
per AGENTS.md hard invariant 6, an implementation one.** Confirmed with the user: consent choices
are the customer's own expressed preference, not routing metadata, so they are erased like the
rest of the ledger — `06-pii-retention.md` §7.2 gains a `DELETE FROM consent WHERE address_id IN
(SELECT id FROM customer_address WHERE customer_id = $1)` statement (run before
`customer_address`'s own `UPDATE` zeroes `value_hmac`, so the subquery can still resolve by
`customer_id`). `consent` is not added to the named-exemption list — it is now a **covered**
table, alongside `comms_request`/`comms_event`/`customer_address`/`customer_external_id`.

**A second, mechanical gap found during the same pass: `tests/erasure_coverage.rs`'s own
detection query cannot see this class of table at all, and that is the more consequential bug.**
`customer_linkable_tables()` flags a table via a `customer_id` column, a `*_ciphertext`/`*_hmac`/
`*_raw` column, or a foreign key from a `customer_id` column — `consent` has none of those; its
only foreign key is `address_id REFERENCES customer_address(id)`, a hop the query never follows.
Left as-is, adding `"consent"` to `COVERED` would immediately trip the *other* half of that same
test (an entry in `COVERED`/`EXEMPT` for a table that exists but the detection query no longer
returns is flagged **stale**) — so the query itself needs a third branch, not just a manifest
edit. Fixed by adding a UNION arm matching any foreign key whose target is `customer_address`
(`co.confrelid = 'customer_address'::regclass`) — this also means any *future* table that follows
AGENTS.md invariant 4's own keying convention is caught automatically, not just this one.
Confirmed no existing table declares such an FK today (`outbox.address_id`, the one other
`address_id` column in the schema, has none — T-009 decision 1), so this is additive: it changes
nothing about any table already classified.

Runs after verification (T-036), before suppression (T-038) per §5's ordering table — moot in
practice, since both already merged and settled on suppression-first (unconditional, cheapest)
ahead of verification (class-conditional); this ticket's own check lands as the second
class-conditional gate, immediately after verification's, both before decrypt. See decision 2.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-037-consent-gate-at-dispatch
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1.

### Confirmed design decisions (do not deviate without asking)

1. **`consent set` requires an existing, active `customer_address` row for the destination; it
   never mints one.** Resolved via the tenant pepper (`tenant_pepper::ensure_tenant_pepper`) +
   `destination_hmac::compute` + `customer::model::kind_for_channel(channel)` +
   `customer::repo::find_active_address_by_hmac(pool, kind, value_hmac)` — the same primitives
   `messgr-ingest`'s own resolution uses, without the minting path. See Description for why
   minting was rejected. No Vault DEK access needed — only the pepper.
2. **The check lives inline in `try_process` (`src/dispatcher/worker.rs`), immediately after the
   verification gate (T-036) and before `repo::load_ciphertexts`.** Matches §5's ordering table
   for the two class-conditional gates; suppression (T-038, unconditional) stays first, unchanged.
3. **Gate applies to `class::MARKETING` only — no separate `matches!` allowlist needed.**
   Transactional does not require opt-in (§5) and auth never reaches the outbox (T-011 decision
   3), so a plain `row.class == class::MARKETING` equality check is the entire condition; unlike
   T-036's verification gate, there is no second class to allow through the same branch.
4. **Absence of a `consent` row means "not opted in," identical to `opted_in = false`.** One
   query, `SELECT opted_in FROM consent WHERE address_id = $1 AND class = $2`, `.unwrap_or(false)`
   on a missing row — matches §5's own "absence of consent defaults to opted-out for marketing."
5. **The gate's own query lives in `dispatcher::repo`, not `consent::repo`** — mirrors T-036's
   `load_verified_at` and T-038's `is_suppressed`: the dispatcher's send-path queries live beside
   `try_process`, and `consent::repo` serves only the CLI/configure side (`load_one`/`upsert`),
   the same split `suppression::repo` (CRUD) vs. `dispatcher::repo::is_suppressed` (gate) already
   established.
6. **No `DispatcherContext` field.** Unlike verification's per-tenant `enforce`/`observe` mode,
   consent has no ambiguous-input problem (the table's absence-means-opted-out default is
   unconditionally correct, not something the operator needs to override) — mirrors T-038
   decision 4. No existing `DispatcherContext { .. }` literal, production or test, needs
   touching.
7. **`consent::configure::set_consent` closes the tenant pool on every exit path, including the
   new "no active address" rejection and a mid-call Vault/DB error** — avoiding T-038/F2's finding
   (a leaked tenant pool on an error path after `connect_tenant_pool` succeeded) rather than
   repeating it.
8. **No `consent list`/`consent show` command.** The Outcome only asks for "a way to record an
   opt-in/opt-out" — `set`'s own success output plus the `platform_audit` trail (`consent.set`,
   same shape as `suppression.add`/`tenant_config.set`) covers verifying what was written. Add a
   `show`/`list` later if an operator actually needs to query it back through the CLI rather than
   the audit log.
9. **`--opted-in` is a plain clap bool flag** (`value_parser = clap::value_parser!(bool)`,
   accepting literal `true`/`false`), **`--class` is closed to `transactional`/`marketing`**
   (`auth` excluded — it can never reach this table meaningfully) via `PossibleValuesParser`, and
   **`--source` is free text** (no closed vocabulary in the design, unlike suppression's
   `reason`) — "where the opt-in/out was captured, for evidence" (e.g. `web_form`, `ivr_call`,
   `branch_visit`, `sms_stop_reply`), not the customer's own words.

### Tasks

#### Task 1 — Schema migration

`migrations/tenant/0014_consent.sql`:

```sql
-- Consent table (DESIGN.md §5, §4.4, T-037), verbatim from
-- development/design/03-data-model.md's own CREATE TABLE consent snippet.
-- Keyed on (address_id, class), never customer_id or the raw destination
-- (AGENTS.md hard invariant 4): customer_address rows are append-only, so
-- a recycled phone number's new address row starts with no consent record
-- of its own, and absence of consent defaults to opted-out for marketing
-- (§5) -- correct behaviour falls out of the schema, no cleanup job needed.
CREATE TABLE consent (
    address_id  uuid        NOT NULL REFERENCES customer_address(id),
    class       text        NOT NULL,  -- transactional | marketing
    opted_in    bool        NOT NULL,
    source      text        NOT NULL,  -- where the opt-in/out was captured, for evidence
    updated_at  timestamptz NOT NULL,
    PRIMARY KEY (address_id, class)
);
```

#### Task 2 — Erasure statement + named-exemption list (`06-pii-retention.md`)

`development/design/06-pii-retention.md` §7.2: add, after the `customer_address` `UPDATE`
statement and before the `customer_external_id` `DELETE`:

```sql
-- consent choices are the customer's own expressed preference, not just
-- routing metadata; erase them with the rest of the ledger (T-037)
DELETE FROM consent WHERE address_id IN (
    SELECT id FROM customer_address WHERE customer_id = $1
);
```

Add a short prose note directly beneath the statements block (matching the doc's existing
correction-callout style) recording that `consent` was added here, and to `COVERED` in
`tests/erasure_coverage.rs` (Task 3), by T-037 — it is not a named exemption like `suppression`,
because a consent record is the customer's own preference, not destination-scoped block-list
data. No change needed to `13-build-order.md`'s exemption-list mention (§34) — `consent` isn't
joining that list.

#### Task 3 — Fix the erasure-coverage detection query, then classify `consent`

`tests/erasure_coverage.rs`:

1. In `customer_linkable_tables`'s query, add a third `UNION` arm that follows a foreign key to
   `customer_address` — the same one-hop-removed shape `consent` (and any future table keyed the
   same way, per AGENTS.md invariant 4) uses:

   ```sql
   UNION

   SELECT DISTINCT co.conrelid::regclass::text AS table_name
   FROM pg_constraint co
   WHERE co.contype = 'f' AND co.confrelid = 'customer_address'::regclass
   ```

2. Add `"consent"` to the `COVERED` constant.

Comment above `COVERED`/the query change: note that this arm is what makes `consent` visible to
the check at all — without it, adding `"consent"` to `COVERED` would trip the *stale-entry*
assertion instead (the table exists but the detection query wouldn't return it).

#### Task 4 — `consent` module (model + repo)

New files, mirroring `src/suppression/` structurally but with `consent::repo` scoped to the
CLI/configure side only (decision 5 — the gate's own query lives in `dispatcher::repo`, Task 6).

`src/consent/model.rs`:

```rust
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Consent {
    pub address_id: Uuid,
    pub class: String,
    pub opted_in: bool,
    pub source: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ConsentInput {
    pub address_id: Uuid,
    pub class: String,
    pub opted_in: bool,
    pub source: String,
}

impl ConsentInput {
    /// `updated_at` is excluded deliberately -- it's server-set on every
    /// write, so a re-`set` can only ever change `opted_in`/`source`.
    pub fn matches(&self, existing: &Consent) -> bool {
        self.address_id == existing.address_id
            && self.class == existing.class
            && self.opted_in == existing.opted_in
            && self.source == existing.source
    }
}
```

`src/consent/repo.rs`:

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Consent, ConsentInput};

pub async fn load_one(
    pool: &PgPool,
    address_id: Uuid,
    class: &str,
) -> Result<Option<Consent>, sqlx::Error> {
    sqlx::query_as::<_, Consent>(
        "SELECT address_id, class, opted_in, source, updated_at FROM consent \
         WHERE address_id = $1 AND class = $2",
    )
    .bind(address_id)
    .bind(class)
    .fetch_optional(pool)
    .await
}

pub async fn upsert(
    pool: &PgPool,
    input: &ConsentInput,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO consent (address_id, class, opted_in, source, updated_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (address_id, class) DO UPDATE SET
            opted_in = EXCLUDED.opted_in,
            source = EXCLUDED.source,
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(input.address_id)
    .bind(&input.class)
    .bind(input.opted_in)
    .bind(&input.source)
    .bind(now)
    .execute(pool)
    .await
    .map(|_| ())
}
```

#### Task 5 — `consent::configure` (actor-facing `set`)

`src/consent/configure.rs`, mirroring `suppression::configure`'s `ConfigureError`/`rejected`/
`audit` shape:

```rust
use sqlx::PgPool;
use chrono::Utc;

use crate::customer::model::kind_for_channel;
use crate::customer::repo as customer_repo;
use crate::destination_hmac;
use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use crate::tenant_pepper::{TenantPepperError, ensure_tenant_pepper};

use super::model::ConsentInput;
use super::repo;

#[derive(Debug)]
pub enum ConfigureError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}
// Display/Error/From<sqlx::Error>/From<KeyStoreError>/From<TenantPepperError> impls,
// identical shape to suppression::configure::ConfigureError.

fn rejected(message: String) -> ConfigureError {
    sqlx::Error::Configuration(message.into()).into()
}

#[derive(Debug)]
pub struct ConfigureOutcome {
    pub outcome: &'static str, // "created" | "updated" | "idempotent"
}

/// Resolves `tenant_slug`, opens its pool, derives the destination's active
/// `customer_address` via the tenant pepper (decision 1 -- never mints one:
/// rejected if none exists), and upserts a `consent` row keyed on that
/// address's id + `class`, auditing exactly one `consent.set` row with
/// outcome `created`/`updated`/`idempotent`/`rejected`.
///
/// Closes the tenant pool on every exit path (decision 7).
#[allow(clippy::too_many_arguments)]
pub async fn set_consent(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    destination: &str,
    channel: &str,
    class: &str,
    opted_in: bool,
    source: &str,
    actor: &str,
) -> Result<ConfigureOutcome, ConfigureError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(control_pool, actor, None, None, class, opted_in, source, "rejected").await?;
            return Err(rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool, base_db_url, tenant.id, &tenant.database_name, 5,
    ).await?;

    let pepper = match ensure_tenant_pepper(control_pool, keystore, &tenant).await {
        Ok(pepper) => pepper,
        Err(err) => {
            tenant_pool.pool.close().await;
            return Err(err.into());
        }
    };
    let value_hmac = destination_hmac::compute(&pepper, destination);
    let kind = kind_for_channel(channel);

    let address = match customer_repo::find_active_address_by_hmac(&tenant_pool.pool, kind, &value_hmac).await {
        Ok(address) => address,
        Err(err) => {
            tenant_pool.pool.close().await;
            return Err(err.into());
        }
    };
    let Some(address) = address else {
        tenant_pool.pool.close().await;
        audit(control_pool, actor, Some(tenant.id), None, class, opted_in, source, "rejected").await?;
        return Err(rejected(
            "no active address on file for this destination -- consent can only be recorded \
             against an address that has already resolved (e.g. via a prior message); it is \
             never minted by this command".to_string(),
        ));
    };

    let input = ConsentInput {
        address_id: address.id,
        class: class.to_string(),
        opted_in,
        source: source.to_string(),
    };
    let result = set_consent_inner(control_pool, &tenant_pool.pool, tenant.id, actor, input).await;
    tenant_pool.pool.close().await;
    result
}

async fn set_consent_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: uuid::Uuid,
    actor: &str,
    input: ConsentInput,
) -> Result<ConfigureOutcome, ConfigureError> {
    let existing = repo::load_one(tenant_pool, input.address_id, &input.class).await?;

    let outcome = match &existing {
        None => "created",
        Some(existing) if input.matches(existing) => "idempotent",
        Some(_) => "updated",
    };

    if outcome != "idempotent" {
        repo::upsert(tenant_pool, &input, Utc::now()).await?;
    }

    audit(
        control_pool, actor, Some(tenant_id), Some(input.address_id),
        &input.class, input.opted_in, &input.source, outcome,
    ).await?;

    Ok(ConfigureOutcome { outcome })
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<uuid::Uuid>,
    address_id: Option<uuid::Uuid>,
    class: &str,
    opted_in: bool,
    source: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "consent.set",
        tenant_id,
        serde_json::json!({
            "address_id": address_id,
            "class": class,
            "opted_in": opted_in,
            "source": source,
            "outcome": outcome,
        }),
    )
    .await
}
```

`address_id` is safe to audit directly (unlike suppression's raw destination) — it's an opaque
UUID, not PII.

#### Task 6 — register the module + the dispatcher gate query

`src/lib.rs`: add `pub mod consent;` (alphabetically between `config` and `customer`).

`src/dispatcher/repo.rs`: add (mirrors `is_suppressed`'s shape — a query the dispatcher owns,
not `consent::repo`, per decision 5):

```rust
/// The dispatcher's own consent check (DESIGN.md §5, T-037): absence of a
/// row means "not opted in," identical to an explicit `opted_in = false`
/// row (decision 4) -- callers never need to distinguish the two.
pub async fn is_consented(
    pool: &PgPool,
    address_id: Uuid,
    class: &str,
) -> Result<bool, sqlx::Error> {
    let opted_in: Option<bool> = sqlx::query_scalar(
        "SELECT opted_in FROM consent WHERE address_id = $1 AND class = $2",
    )
    .bind(address_id)
    .bind(class)
    .fetch_optional(pool)
    .await?;
    Ok(opted_in.unwrap_or(false))
}
```

#### Task 7 — the gate check in `try_process`

`src/dispatcher/worker.rs`: immediately after the existing verification-gate block (decision 2),
before `repo::load_ciphertexts`:

```rust
// Consent gate (DESIGN.md §5, T-037) — marketing only; transactional does
// not require opt-in (§5), and auth never reaches the outbox (T-011
// decision 3). Absence of a row defaults to opted-out (decision 4).
if row.class == class::MARKETING
    && !repo::is_consented(&ctx.pool, row.address_id, row.class.as_str()).await?
{
    repo::write_terminal(
        &ctx.pool,
        row.created_at,
        row.comms_request_id,
        row.customer_id,
        "suppressed_consent",
        None,
        None,
        "suppressed_consent",
    )
    .await?;
    return Ok(());
}
```

No new imports needed — `class` and `repo` are both already imported/in scope in this file.

#### Task 8 — CLI wiring (`src/bin/control.rs`)

Add `use messgr::consent::configure::set_consent;` and `use messgr::ingest::model::class;`
(`channel` is already imported, from `template::model::channel` — reuse it, no second import).

Add a `Command::Consent { command: ConsentCommand }` variant (after `Suppression`, same doc-
comment style):

```rust
#[derive(Subcommand)]
enum ConsentCommand {
    /// Record a customer's opt-in or opt-out for one message class on a
    /// destination that has already resolved to a real address. Rejected
    /// if no active address is on file yet -- this command never mints
    /// one (T-037 decision 1); record consent after the customer's first
    /// message, or via whatever event-feed consumer eventually lands
    /// (build-order step 10).
    Set {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long)]
        destination: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                channel::SMS,
                channel::EMAIL,
                channel::WHATSAPP,
            ])
        )]
        channel: String,
        #[arg(
            long,
            value_parser = clap::builder::PossibleValuesParser::new([
                class::TRANSACTIONAL,
                class::MARKETING,
            ])
        )]
        class: String,
        #[arg(long = "opted-in", value_parser = clap::value_parser!(bool))]
        opted_in: bool,
        /// Where this opt-in/out was captured, for evidence (e.g. web_form,
        /// ivr_call, branch_visit, sms_stop_reply) -- not the customer's own words.
        #[arg(long)]
        source: String,
        /// Operator identity recorded on the platform_audit row.
        #[arg(long)]
        actor: String,
    },
}
```

In `run()`, a new arm (Vault-connecting, like `Suppression`):

```rust
Command::Consent { command } => {
    let vault_keystore = VaultKeyStore::connect(config.profile).expect(
        "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
    );
    match command {
        ConsentCommand::Set {
            tenant_slug, destination, channel, class, opted_in, source, actor,
        } => {
            let outcome = set_consent(
                &control_pool, &config.control_database_url, &tenant_slug,
                &vault_keystore, &destination, &channel, &class, opted_in, &source, &actor,
            ).await.map_err(|err| format!(
                "failed to set consent for tenant {tenant_slug:?}: {err}"
            ))?;
            println!("outcome={}", outcome.outcome);
        }
    }
}
```

### Acceptance test

**`tests/dispatcher.rs`** — add, using the existing `write_outbox_row_with_verification` helper
directly (class `"marketing"`, already-verified, unique destination_hmac per test so the
suppression gate never interferes) and `claimed[0].address_id` (no new helper needed):

1. **`marketing_without_any_consent_row_is_blocked_with_suppressed_consent`.** Write a marketing
   row, no `consent` row inserted. Claim + `try_process`. Assert: `comms_event` has exactly one
   row, `event_type = 'suppressed_consent'`; `comms_request.final_status =
   Some("suppressed_consent")`; outbox row count 0; the wiremock mock (mounted with `.expect(0)`)
   never receives a request.
2. **`marketing_with_an_explicit_opt_out_is_blocked`.** Same as above, but insert a `consent` row
   directly (`address_id` = the claimed row's, `class = 'marketing'`, `opted_in = false`) before
   claiming. Same assertions as test 1 — proves the gate reads the row's value, not just
   row-presence.
3. **`marketing_with_an_explicit_opt_in_sends`** (negative control / mutation test). Same
   `consent` row, `opted_in = true`. Assert: only a `sent` event, `final_status = Some("sent")`,
   mock called once.
4. **`transactional_sends_without_any_consent_row`.** Class `"transactional"`, no `consent` row.
   Assert the send still reaches the mock — proves decision 3's class scoping.

**`tests/consent.rs`** (new file, following `tests/suppression.rs`'s conventions — real
provisioning, no mocks). Needs one local helper beyond `tests/suppression.rs`'s own set:
`insert_active_address(control_pool, tenant_pool, vault, slug, destination) -> Uuid` — computes
the tenant's pepper + `destination_hmac::compute`, then inserts a `customer` + `customer_address`
row directly via SQL (`kind = 'msisdn'`, matching `--channel sms`), returning the new
`address_id`, so `set_consent`'s own HMAC computation resolves to that same row.

- `setting_and_reading_back_round_trips_every_typed_field` (against an address inserted via the
  new helper)
- `setting_again_with_a_different_opted_in_value_updates_the_row_without_duplicating_it`
- `resetting_with_identical_inputs_is_idempotent`
- `consent_set_against_an_unknown_tenant_slug_is_rejected_and_audited` (mirrors
  `tests/suppression.rs`'s own T-005/F1 test)
- `consent_set_against_a_destination_with_no_active_address_is_rejected_and_audited` (no
  `insert_active_address` call — asserts a `rejected` `platform_audit` row with a real
  `tenant_id` this time, unlike the unknown-tenant-slug case)

**`src/bin/control.rs`'s existing `mod tests`** (clap-parsing only, no DB — mirrors
`provider_config_set_parses...`/`provider_config_set_rejects_an_invalid_channel`):

- `consent_set_parses_every_flag`
- `consent_set_rejects_an_invalid_class`
- `consent_set_rejects_an_invalid_channel`
- `consent_set_rejects_a_non_boolean_opted_in`

Run: `just build && just test && just lint`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/control-plane-cli.adoc`: new `== Consent` section, placed after `==
  Suppression list` (grouping the two regulatory-gate CLIs), in the same style — a command
  example, then prose covering: `set` requires a pre-existing active address and is rejected
  otherwise; `--class` is closed to `transactional`/`marketing`; `--opted-in` is a plain
  `true`/`false`; `--source` is free-text evidence, not customer-supplied content; re-running with
  identical inputs is idempotent.
- `docs/user-manual/dispatcher.adoc`: new paragraph immediately after the existing `T-036`
  paragraph (before the `Requires DISPATCHER_TENANT_SLUG...` paragraph) — fulfilling that
  paragraph's own forward reference ("`T-038` ... unlike verification/consent below"). State: the
  consent gate runs right after verification, before decrypt; applies to `marketing` only; absence
  of a `consent` row defaults to not-opted-in, same as an explicit `opted_in = false` row; blocks
  with a terminal `suppressed_consent` event. Cross-reference "Consent" (control-plane CLI) for
  managing entries.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` clean.
2. `docs/user-manual/control-plane-cli.adoc` and `docs/user-manual/dispatcher.adoc` updated per
   above; `just docs-check` clean.
3. Write a summary (files touched, decisions made, anything deferred) and hand back.
4. Suggested commit message:

   ```
   feat(dispatcher): enforce the consent gate at send time (T-037)

   Adds the consent table (schema already specified in DESIGN.md,
   migration was missing), a consent module with a set-only CLI
   (rejects an unresolved destination rather than minting one), and an
   unconditional-for-marketing check in try_process alongside
   verification/suppression. Also closes an erasure gap: consent rows
   are now covered by physical redaction, and the erasure-coverage
   test's detection query gains a customer_address-FK arm so any table
   keyed the same way is caught automatically.
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on `feat/T-037-consent-gate-at-dispatch`. Do not push or open an MR without
   user approval. Present the commit message; after approval, verify `origin/main...HEAD` carries
   no `tickets/` path, then push and open the MR. Merging is the human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed alongside T-036/T-038 from a
  build-order-vs-shipped-tickets gap analysis — step 5 (the gate chain) is unbuilt despite steps
  0-4 and 17 later hardening tickets being done.
- 2026-09-16 — Description amended (T-038 review, impact sweep): T-038 shipped first, so the
  suppression check is already `try_process`'s first statement — this ticket's own consent-check
  placement relative to it is now this ticket's own call at refinement, not §5's prose ordering.
- 2026-09-17 — TO DO → READY: plan complete. Refinement re-graded complexity/cost from
  medium-high/M-L to medium/M, in line with sibling gates T-036/T-038 — see Description for
  what refinement found (erasure gap, detection-query fix, no-mint decision).
- 2026-09-17 — READY → IN DEVELOPMENT: picked up
- 2026-09-17 — IN DEVELOPMENT → IN REVIEW: acceptance green
