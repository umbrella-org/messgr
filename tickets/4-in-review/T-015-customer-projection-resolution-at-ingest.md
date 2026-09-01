---
id: T-015
title: Customer projection + resolution at ingest
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-015 — Customer projection + resolution at ingest

## Outcome

Ingest resolves any inbound send request (explicit `customer_id`, external id + system, or
address alone) to a `customer_id` and `address_id`, minting a provisional shell when nothing
resolves. Every ledger row from this point on carries a real customer key instead of nothing to
key a DEK or a consent record against.

## Description

Builds the customer projection schema (`customer`, `customer_external_id`,
`customer_address`, `customer_alias` — §4.6) and wires resolution into the ingest path (§4.7):
`customer_id` used directly after alias expansion, external id resolved via
`customer_external_id`, address resolved via `value_hmac` against active addresses, and a
provisional customer + address minted when nothing resolves. Resolution never rejects a send —
a missing timeline entry is an acceptable outcome, a blocked OTP is not (§4.8, though OTP itself
bypasses this path entirely per §3).

This is build-order step 3 (§14), and it has to land before step 4/5's gates because consent
keys on `customer_address.id` (§5) — the gates need the resolution path's `address_id` to exist
first. The event feed consumer that keeps the projection fresh (build-order step 10) is stubbed
for this ticket; the schema and the resolution logic are not. Contact values are encrypted under
the customer DEK from the first write (§7, invariant #7 in AGENTS.md) — depends on T-008's DEK
lifecycle being in place, which is already done.

Out of scope: the event feed consumer itself and nightly reconciliation (step 10), staleness-gated
deferral for transactional/marketing resolution (§4.8 — needs the staleness threshold, an open
question, §"Still open" #2), and customer-split adjudication tooling (§4.7, flagged for a human
rather than automated).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-015-customer-projection-resolution
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented
(rules §0). Do not push and do not open a merge request without explicit user approval. Ticket
and board bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-008` (customer DEK lifecycle) and `T-011` (ingest) are both in `6-done/` and merged to
  `main` — this ticket resolves onto the same `customer_dek` and encrypts under the same
  `get_or_create_dek` this ticket reuses unchanged.
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init` — the
  integration tests provision real tenants.

### Confirmed design decisions (do not deviate without asking)

1. **DESIGN.md §4.6 gets one schema correction as part of this ticket, already applied to the
   design doc**: a `CREATE UNIQUE INDEX ON customer_address (kind, value_hmac) WHERE active_to
   IS NULL` line, absent from the original draft. Without it, two concurrent address-only
   resolutions (decision 6 below) for the same never-seen destination can each mint a separate
   provisional customer for the same number. The migration in Task 1 must match §4.6 as it now
   reads, including this index — same "verbatim from the design's own `CREATE TABLE`
   statements" discipline T-009 set.
2. **Module layout mirrors `src/producer/`**: `src/customer/{mod.rs, model.rs, repo.rs,
   resolve.rs}` — typed rows in `model.rs`, raw `sqlx` queries in `repo.rs`, the resolution
   error enum + orchestration in `resolve.rs` (mirrors `producer::resolve`'s shape exactly:
   error enum with `Display`/`Error`/`From<sqlx::Error>`, one public entry function).
3. **Channel → `kind` mapping**: `sms → msisdn`, `email → email`, `whatsapp → whatsapp`. These
   are the only three channels `ingest::model::channel` accepts (`validate_channel` already
   gates this), so the mapping function has no fallible case to handle.
4. **Request shape.** `CreateCommsRequest` gains three fields and loses `customer_id`'s
   mandatory-ness:
   ```rust
   pub customer_id: Option<Uuid>,
   pub external_id: Option<String>,
   pub external_id_system: Option<String>,
   ```
   `destination` stays mandatory — every send needs somewhere to go, resolution only changes
   how the *customer* is identified. Exactly one of three shapes is legal: `customer_id` set (and
   both `external_id`/`external_id_system` unset); `external_id` **and** `external_id_system`
   both set (and `customer_id` unset); or all three unset (address-only resolution via
   `destination`'s `value_hmac`). Any other combination — both `customer_id` and `external_id`
   set, or `external_id` set without `external_id_system` (or vice versa) — is a new
   `IngestError::InvalidResolutionInput` (`422`), checked before any DB or Vault call.
5. **Alias expansion applies only to the explicit-`customer_id` path**, matching §4.7's table
   literally (only that row mentions "after alias expansion"). `customer_external_id` rows are
   kept current by the master feed on a merge (out of scope here — the feed consumer is step 10),
   so an external-id lookup is assumed already current; address-only resolution has no id to
   expand yet.
6. **Explicit `customer_id` for an unknown id creates a provisional customer under that exact
   id**, not a fresh random one — the caller's own id is the one thing already asserted, and
   substituting a different id would silently break the caller's own correlation. External-id
   and address-only resolution, which have no caller-supplied `customer_id` to preserve, mint a
   fresh random id as usual.
7. **Every resolution path ends with a real `customer_address` row for `destination`** — either
   an existing active one it matches, or one it creates. `value_ciphertext`'s AAD is the address
   row's own `id` (fresh `Uuid::new_v4()`, generated before the encrypt call), following
   `encryption.rs`'s "bind ciphertext to the row it belongs to" convention — same pattern
   `comms_request`/`outbox` already use with `comms_request_id`.
8. **Three concurrency races, three fixes, matching `customer_dek::insert_if_absent`'s
   insert-then-refetch-on-conflict shape:**
   - Address-only, unknown destination: insert customer + address in one transaction; the
     address insert is `ON CONFLICT (kind, value_hmac) WHERE active_to IS NULL DO NOTHING`
     (decision 1's new index). Zero rows affected → roll back, re-fetch the winning active
     address by `(kind, value_hmac)`, use its `customer_id`/`id` instead of what this call
     almost minted.
   - External id, unknown: insert customer + `customer_external_id` in one transaction,
     `ON CONFLICT (system, external_id) DO NOTHING` on the second insert. Zero rows affected →
     roll back, re-fetch via `find_customer_by_external_id`, use the winner.
   - Explicit/external customer_id attaching a *new* address under an already-resolved
     customer: `ON CONFLICT (customer_id, kind, rank) WHERE active_to IS NULL DO NOTHING` (the
     existing §4.6 index) is not the risk here — `next_rank_for` racing with itself is. Compute
     the next rank and insert inside the same transaction that holds the row lock implied by
     `next_rank_for`'s own `SELECT ... FOR UPDATE` over that customer's active address rows, so
     two concurrent sends to two different new destinations for the same customer never
     collide on the same rank.
9. **Explicit/external `customer_id` whose destination's `value_hmac` is already active under a
   *different* customer is a conflict, not a silent misattribution.** The address insert hits
   decision 1's `(kind, value_hmac)` unique index; on conflict, roll back and return
   `ResolveError::AddressConflict { existing_customer_id }`, surfaced as `IngestError::
   AddressConflict` (`409`). This is the one case genuinely different from "resolution found
   nothing" (§4.7's "never reject a send" is about absence, not about two identities actively
   disagreeing over the same contact point) — expected to be rare (stale caller data, address
   recycling never reaching this caller), but silently attaching the message to whichever
   customer_id wins the race is worse than a `409` the producer team can investigate.
10. **Locale precedence: `body.locale` (if the caller supplied one) → the resolved customer's
    `locale` (only when that customer is **not** provisional — a provisional row's `locale` is
    just the tenant default it was minted with, so falling through to the tenant default for it
    is equivalent and clearer about why) → `tenant.config.default_locale`.** This is a strict
    superset of T-011's current `body.locale ?? tenant default` — it only adds a middle tier,
    never removes the caller's ability to override. `resolve()` returns `locale: Option<String>`
    (`Some` only for a non-provisional customer), and `handler.rs` folds the three tiers.
11. **`timezone` is written (customer default at mint, event-feed value once step 10 exists) but
    not read by anything in this ticket** — nothing consumes it before quiet hours (step 8) and
    scheduled local-time delivery (step 9). Storing it now is required (`NOT NULL` column, and
    retrofitting it onto the ledger later is exactly the migration AGENTS.md's build-order notes
    warn against); consuming it is out of scope.
12. **`get_or_create_dek` is called from inside `customer::resolve` whenever an address row is
    being written** (encrypting `value_ciphertext` needs the DEK), and unconditionally once more
    from `handler.rs` for the `comms_request`/`outbox` encryption exactly as T-011 already does
    — the second call is always a cache hit when the first ran, and the only cost when it
    didn't (an already-existing address, no write) is the same lookup T-011 always paid anyway.

### Tasks

#### Task 1 — Migration: customer projection schema

Add `migrations/tenant/0008_customer_projection.sql`: `customer`, `customer_external_id`,
`customer_address` (including decision 1's new unique index), `customer_alias`, verbatim from
DESIGN.md §4.6 as it now reads. Header comment names §4.6 and this ticket, following
`0003_customer_dek.sql`/`0004_ledger_outbox_schema.sql`'s header style. No `tenant_id` column on
any of the four tables, matching every other tenant-database table (§2.1 — the tenant already is
the database).

#### Task 2 — `src/customer/model.rs`

```rust
pub struct Customer {
    pub id: Uuid,
    pub locale: String,
    pub timezone: String,
    pub provisional: bool,
    pub source_system: Option<String>,
    pub source_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
pub struct CustomerAddress {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub kind: String,
    pub value_ciphertext: Vec<u8>,
    pub value_hmac: Vec<u8>,
    pub rank: i16,
    pub label: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
    pub active_from: DateTime<Utc>,
    pub active_to: Option<DateTime<Utc>>,
    pub source_updated_at: DateTime<Utc>,
}
pub mod kind {
    pub const EMAIL: &str = "email";
    pub const MSISDN: &str = "msisdn";
    pub const WHATSAPP: &str = "whatsapp";
    #[allow(dead_code)] // no push/postal channel exists yet (step 11)
    pub const PUSH: &str = "push";
    #[allow(dead_code)]
    pub const POSTAL: &str = "postal";
}
pub fn kind_for_channel(channel: &str) -> &'static str {
    match channel {
        crate::ingest::model::channel::SMS => kind::MSISDN,
        crate::ingest::model::channel::EMAIL => kind::EMAIL,
        crate::ingest::model::channel::WHATSAPP => kind::WHATSAPP,
        other => unreachable!("validate_channel already rejected {other:?}"),
    }
}
```

#### Task 3 — `src/customer/repo.rs`

Raw-query functions, `sqlx::FromRow` for the two structs, following `src/producer/repo.rs`'s
style:

- `find_by_id`, `expand_alias` (follows `customer_alias.old_customer_id → customer_id`,
  re-querying up to a fixed hop limit of 8 and returning the input id unchanged the moment no
  row matches — a cycle is a data bug, not a hang).
- `find_customer_by_external_id(pool, system, external_id) -> Option<Uuid>`.
- `find_active_address_by_hmac(pool, kind, value_hmac) -> Option<CustomerAddress>` (global — no
  `customer_id` filter; this is address-only resolution's lookup).
- `find_active_address_for_customer(pool, customer_id, kind, value_hmac) -> Option<CustomerAddress>`.
- `next_rank_for_update(tx, customer_id, kind) -> i16` — `SELECT COALESCE(MAX(rank), 0) + 1 FROM
  customer_address WHERE customer_id = $1 AND kind = $2 AND active_to IS NULL FOR UPDATE`,
  callable only with an open transaction (decision 8's third bullet).
- `insert_customer(tx, id, locale, timezone, provisional, source_updated_at, created_at)`.
- `insert_external_id(tx, system, external_id, customer_id) -> bool` (`ON CONFLICT (system,
  external_id) DO NOTHING`; returns whether the row was actually inserted).
- `insert_address(tx, id, customer_id, kind, value_ciphertext, value_hmac, rank, active_from,
  source_updated_at) -> bool` (`ON CONFLICT (kind, value_hmac) WHERE active_to IS NULL DO
  NOTHING`; returns whether inserted — the caller distinguishes "lost the race" (decision 8) from
  "conflicts with a different customer" (decision 9) by re-fetching and comparing
  `customer_id`).

#### Task 4 — `src/customer/resolve.rs`

```rust
pub enum ResolutionInput {
    Explicit(Uuid),
    External { system: String, external_id: String },
    AddressOnly,
}
pub struct Resolved {
    pub customer_id: Uuid,
    pub address_id: Uuid,
    pub locale: Option<String>, // Some only for a non-provisional customer (decision 10)
}
pub enum ResolveError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
    Encryption(EncryptionError),
    AddressConflict { existing_customer_id: Uuid },
}
```

`pub async fn resolve(pool, keystore, dek_cache, vault_mount, pepper, input, destination,
channel, tenant_default_locale, tenant_default_timezone) -> Result<Resolved, ResolveError>`:

1. Compute `kind = model::kind_for_channel(channel)` and `value_hmac =
   destination_hmac::compute(pepper, destination)` (reused as-is — §4.6's own comment: "same as
   destination_hmac").
2. Branch on `input`:
   - `Explicit(id)`: `id = repo::expand_alias(pool, id)`; if `repo::find_by_id` is `None`, mint
     a provisional customer **under this exact `id`** (decision 6); either way fall through to
     step 3 with this `customer_id` and `locale = if provisional { None } else { Some(row.locale) }`.
   - `External { system, external_id }`: `repo::find_customer_by_external_id`; if found, fall
     through to step 3 with that id (fetch its row for `locale`/`provisional`). If not found,
     mint a fresh-uuid provisional customer + `customer_external_id` row together in one
     transaction (decision 8, bullet 2); on lost race, re-fetch and use the winner instead.
   - `AddressOnly`: `repo::find_active_address_by_hmac(pool, kind, value_hmac)`; if found, return
     `Resolved` directly from that row (customer already known, address already exists — no
     write at all). If not found, mint a fresh-uuid provisional customer + address together in
     one transaction (decision 8, bullet 1); on lost race, re-fetch and use the winner's
     `(customer_id, address.id)` directly.
3. (Explicit/External paths only, when the branch didn't already return in step 2) —
   `repo::find_active_address_for_customer(pool, customer_id, kind, value_hmac)`; if found, done.
   If not found: `get_or_create_dek` for `customer_id`, encrypt `destination` under it
   (AAD = the new address id's bytes — decision 7), start a transaction, `next_rank_for_update`,
   insert the address (`ON CONFLICT (kind, value_hmac) ... DO NOTHING`); zero rows affected means
   decision 9's conflict — roll back, re-fetch by `(kind, value_hmac)`, return
   `ResolveError::AddressConflict { existing_customer_id: winner.customer_id }`.

Provisional mint's `locale`/`timezone` are `tenant_default_locale`/`tenant_default_timezone`
(decision 11); `source_system`/`source_updated_at` are `None` (§4.6 — `NULL` for provisional).

#### Task 5 — `src/customer/mod.rs` and `src/lib.rs`

`pub mod model; pub mod repo; pub mod resolve;` in `src/customer/mod.rs`; register `pub mod
customer;` in `src/lib.rs` (alphabetical position, after `config`).

#### Task 6 — `src/ingest/model.rs`

- `CreateCommsRequest`: apply decision 4's field changes.
- `IngestError`: add `InvalidResolutionInput` and `AddressConflict(Uuid)`; `From<ResolveError>`
  mapping `Database`/`Vault`/`Encryption` straight through to the existing `IngestError`
  variants of the same name, and `AddressConflict { existing_customer_id }` to
  `IngestError::AddressConflict(existing_customer_id)`.
- `IntoResponse`: `InvalidResolutionInput → 422`, `AddressConflict → 409`.
- `Display`: one line each, `AddressConflict` includes the existing customer id.

#### Task 7 — `src/ingest/handler.rs`

- Before the template lookup: validate the request's `customer_id`/`external_id`/
  `external_id_system` combination (decision 4) and build a `ResolutionInput`; call
  `customer::resolve::resolve(...)`.
- Compute `locale` per decision 10's three-tier fallback, replacing the current `body.locale ??
  tenant.config.default_locale` line.
- Use `resolved.customer_id`/`resolved.address_id` everywhere `body.customer_id`/the old random
  `address_id` were used (DEK fetch, `insert_transactional` call). Delete the old "T-011 decision
  4: mints its own throwaway `address_id`" comment and the `Uuid::new_v4()` line it justified —
  this ticket is the thing that comment said would replace it.
- `insert_transactional`'s signature in `src/ingest/repo.rs` is unchanged — it already takes
  `customer_id`/`address_id` as parameters (T-011); only the values the handler passes change.

#### Task 8 — Tests

`tests/customer.rs` (new, integration, following `tests/customer_dek.rs`'s
provision-a-real-tenant conventions):

1. Explicit `customer_id`, first-ever destination for that customer → address created with
   `rank = 1`; second distinct destination, same customer, same `kind` → `rank = 2`.
2. Explicit `customer_id` unknown to the `customer` table → provisional row minted under that
   exact id (decision 6); `resolve` again with the same id → no second customer row, address
   reused.
3. Unknown external id → provisional customer minted with a fresh id, `customer_external_id` row
   written; resolving the same `(system, external_id)` again returns the same `customer_id`.
4. Address-only, unknown destination → provisional customer + address minted; resolving the same
   destination again (still address-only) returns the same `customer_id`/`address_id`, no second
   customer.
5. **Alias expansion**: explicit `customer_id` pointing at a retired id (a `customer_alias` row
   inserted directly by the test) resolves to the current id, not the retired one.
6. **Race, address-only**: `tokio::join!` two `resolve` calls with `AddressOnly` for the same
   never-seen destination; assert exactly one provisional customer exists afterward and both
   calls return the same `(customer_id, address_id)`.
7. **Race, external id**: same shape as 6 but for two concurrent unknown-external-id resolutions
   sharing `(system, external_id)`.
8. **Conflict (decision 9)**: explicit `customer_id` A resolves a destination (creating its
   address); explicit `customer_id` B (different, real, non-alias) resolving the *same*
   destination returns `ResolveError::AddressConflict { existing_customer_id: A }`.

`tests/ingest.rs` additions (extends the existing suite, `sample_body` gets an `external_id`/
`external_id_system`-flavored sibling):

9. `sample_body` (explicit `customer_id`, unchanged shape) still returns `201`/writes a real
   `customer_address` row now (was previously a throwaway random `address_id` — assert the
   `outbox.address_id` now matches a real row in `customer_address`).
10. A request with only `destination` set (no `customer_id`/`external_id`) still succeeds
    (`201`) and provisions a customer.
11. A request with both `customer_id` and `external_id` set → `422`,
    `error == "..."` matching `InvalidResolutionInput`'s `Display`.
12. Two customer_ids resolving the same destination (decision 9, exercised through the real HTTP
    path this time) → the second request is `409`.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/customer.rs and the extended tests/ingest.rs
```

### Docs update (mandatory when user-facing)

User-facing surface: `POST /comms`'s request shape (new optional fields, `customer_id` no
longer mandatory) and its new `422`/`409` error cases.

- `README.md`, "### messgr-ingest: `POST /comms`" section — replace the sentence "No gate chain
  and no resolution-at-ingest exist yet (`T-016`/`T-017`) ... must supply `customer_id` and
  `destination` directly" (stale: it forward-referenced ticket numbers assigned before this
  ticket and T-016 existed — resolution is this ticket, T-015, and T-016 is kill switches, not
  gates) with an accurate description of the three resolution modes and the new error cases; add
  a curl example using `external_id`/`external_id_system`.
- New `README.md` section "### Customer projection", after "### Tenant configuration" (parking it
  next to the other tenant-database-schema section, mirroring where "### Partition lifecycle"
  sits relative to what it operates on) — describes the four tables, the three resolution modes,
  provisional shells, and that the event feed (step 10) and staleness gating (§4.8) are not yet
  built.
- `DESIGN.md` — already updated with the §4.6 index correction (done during refinement, ahead of
  this section normally being "no change expected"); no further edit expected. If implementation
  forces another deviation, stop and raise it rather than editing the design to match the code.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(ingest): resolve customers at ingest instead of minting throwaway addresses (T-015)

   Adds the customer projection (customer, customer_external_id,
   customer_address, customer_alias) and wires ingest to resolve a real
   customer_id/address_id from an explicit id, an external id, or the
   destination alone, minting a provisional shell when nothing resolves.
   Closes a concurrency gap in DESIGN.md §4.6's original schema with a
   new partial unique index. The event feed that keeps the projection
   fresh (build-order step 10) is not part of this change.
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration / module / ingest wiring / tests / docs is a natural split) before
   presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify `git fetch
   origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints nothing
   (in-tree layout, rules §0), then push and open the merge request. Merging is the human's.

## Review

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | Explicit-`customer_id` provisional mint (`mint_provisional_customer`) is a 4th, unprotected concurrency race — decision 8 enumerates exactly three races with insert-then-refetch-on-conflict protection, but the explicit-id-unknown path uses a plain `INSERT` with no `ON CONFLICT`. Two concurrent resolutions of the same never-before-seen `customer_id` both pass `find_by_id == None` and both insert; the loser gets a raw unique-violation surfaced as a `500`, contradicting §4.7's "never reject a send because resolution failed." | `src/customer/resolve.rs:266-285` (`mint_provisional_customer`); reproduced via `tests/ingest.rs::concurrent_identical_requests_do_not_double_send`, intermittently `[201, 500]` instead of `[201, 200]` — Postgres: `duplicate key value violates unique constraint "customer_pkey"`. `cargo test --test ingest` is not reliably green. | Give `insert_customer`'s caller-supplied-id call site the same `ON CONFLICT (id) DO NOTHING` + refetch-and-compare shape decision 8's other three races got. |
| F2 | non-blocking | design | new ticket | Lost address-only mint race leaves an orphaned `customer_dek` row: the DEK is created (and persisted) for a fresh `customer_id` *before* the transaction that inserts `customer`+`customer_address`; on a lost race the transaction rolls back but the DEK row survives, unreachable from any `customer` row and therefore invisible to §7.2's erasure sweep. Narrow race, not a golden-path bug — batched into a follow-up since the fix needs design thought (restructure vs. sweep), not a one-liner. | `src/customer/resolve.rs:338-377` (`mint_provisional_customer_and_address`); `customer_dek` has no FK to `customer` (`migrations/tenant/0003_customer_dek.sql`); precedent for DEK-before-customer already exists in `pre_provision_deks` (`src/customer_dek/lifecycle.rs:97-105`). | T-018 (spawned). |
| F3 | non-blocking | stale-xref | fixed inline | This ticket's own docs task rewrote the `POST /comms` README paragraph but the new sentence still cited `T-016` for the whole "gate chain," reproducing the exact T-016↔gate-chain conflation the docs task itself called out as wrong when it was written into the *original* stale sentence. | `README.md:255` (pre-fix, on `feat/T-015-customer-projection-resolution`) | Corrected in place — see disposition. |
| F4 | blocking | correctness | — | Self-found during F1's rework, re-running the mandated acceptance test: `next_rank_for_update` locks `customer_address` rows `FOR UPDATE` to serialize concurrent address inserts for the same customer, but `FOR UPDATE` cannot lock a row that doesn't exist yet — a customer's *first* address of a given `kind` was unprotected. Two concurrent resolutions converging on the same customer (e.g. two racing external-id lookups landing on the same winner) could both see zero existing rows, both compute `rank = 1`, and collide on `customer_address`'s `(customer_id, kind, rank)` unique index. | `src/customer/repo.rs` (`next_rank_for_update`, pre-fix); reproduced via `tests/customer.rs::concurrent_external_id_resolution_mints_exactly_one_provisional_customer`, intermittently `duplicate key value violates unique constraint "customer_address_customer_id_kind_rank_idx"`. | Fixed alongside F1 (same rework pass): lock the `customer` row itself instead — it always exists by this point, so locking it serializes every concurrent address insert under that customer regardless of `kind` or whether any address rows exist yet. |

Disposition summary: 2 blocking (F1, F4 — both fixed via this rework pass, commit `caee931`), 1 new ticket (F2 → T-018), 1 fixed inline (F3).

Rework fix confirmation: F1 and F4 fixed on `feat/T-015-customer-projection-resolution` (commit `caee931`); `just fmt`/`just lint`/`just test` clean, plus 5 repeated runs each of `tests/customer.rs` and `tests/ingest.rs::concurrent_identical_requests_do_not_double_send` with no failures (both races were intermittent, not deterministic).

cost: estimated L, actual L

- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a)
- [x] Docs-readability pass: skipped — no docs-readability reviewer configured in this session (step 4b)
- [x] Findings recorded above with severity, class, and disposition; disposition summary + cost line present (step 5)

## History

- 2026-09-01 — created (TO DO). source: chat: build-order step 3 (§14), filed after T-014 (step 2 work) landed.
- 2026-09-01 — TO DO → READY: plan complete
- 2026-09-01 — READY → IN DEVELOPMENT: picked up
- 2026-09-01 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-01 — IN REVIEW → REWORK: F1: unprotected explicit-customer_id mint race
- 2026-09-01 — REWORK → IN REVIEW: F1 and F4 fixed
