---
id: T-018
title: Fix the two open races on customer_address's (kind, value_hmac) index
project: messgr
depends-on: []
spawned-by: [T-015]
impact: high
complexity: medium
cost: M
---

# T-018 — Fix the two open races on customer_address's (kind, value_hmac) index

## Outcome

A lost address-only provisional-mint race no longer leaves a permanent, unreferenced
`customer_dek` row (and its live Vault Transit datakey) behind — the row never gets created
for the losing attempt in the first place. Separately, resolving a
send against a destination already active under a different customer no longer rejects the
send — it is resolved to an outcome, per §4.7's "never reject a send because resolution
failed," matching how every other resolution path in `src/customer/resolve.rs` already
handles a lost race.

**Scope widened during the 2026-09-02 design/implementation audit** — see the second
Description section below. Both problems are consequences of the same `(kind, value_hmac)
WHERE active_to IS NULL` unique index added during T-015's own refinement, so they belong in
one ticket rather than being split and re-discovering the shared context twice.

## Description

`customer::resolve::mint_provisional_customer_and_address` (T-015, `src/customer/resolve.rs`)
must encrypt the new address's value before it can insert the row, which means it calls
`get_or_create_dek` — and thereby persists a `customer_dek` row via `insert_if_absent` — for a
freshly-minted `customer_id`, *before* opening the transaction that inserts the `customer` +
`customer_address` rows. If that transaction loses DESIGN.md §4.6's `(kind, value_hmac)` unique
index race (two concurrent address-only resolutions for the same never-seen destination), the
transaction rolls back the `customer` insert — but the `customer_dek` row committed moments
earlier survives, keyed to a `customer_id` that will now never appear in `customer`,
`customer_address`, or the ledger. `customer_dek` has no FK to `customer`, so nothing rejects
or later reaps this row.

This matters for a bank's compliance posture specifically: AGENTS.md hard invariant 6 requires
every table holding customer data to appear in §7.2's erasure statements, and those statements
are keyed by `customer_id` reachable from `customer`/`customer_alias` — an orphaned
`customer_dek` row is invisible to that sweep by construction, so it (and the Vault datakey it
wraps) persists forever with no path to crypto-shredding.

**Decided during this ticket's refinement:** not a periodic sweep, and not reordering the
`customer`-then-DEK commits either — both racers would then each get a permanent `customer`
row, which breaks the "exactly one provisional customer" invariant the existing race tests
check. Instead, `keystore.create_dek` still runs up front (the address ciphertext needs the
plaintext DEK before the insert is attempted), but the resulting `customer_dek` row is only
persisted (`customer_dek::repo::insert_if_absent`) *after* the `customer`+`customer_address`
transaction commits. A lost race then simply discards the freshly-minted `Dek` — Vault's
`generate-data-key` call leaves no server-side artifact to clean up, so there is nothing to
sweep. See the Implementation Plan's Confirmed design decisions for the exact mechanics.
`pre_provision_deks` (`src/customer_dek/lifecycle.rs`) is untouched by this — it already
creates `customer_dek` rows ahead of any `customer` row on purpose, on a different call path.

The race is narrow (only two concurrent first-time resolutions of the exact same
never-before-seen destination), so this is a hygiene/compliance-completeness fix, not a
golden-path bug — nothing about resolution itself misbehaves.

### Second problem, folded in from the design/implementation audit: `AddressConflict` rejects a send

`resolve()`'s `Explicit`/`External` path (`src/customer/resolve.rs`, around line 250) hits the
same `(kind, value_hmac) WHERE active_to IS NULL` index when a caller supplies a `customer_id`
or external id together with a destination that is *already* the active address of a
*different* customer. Unlike every other lost-race path in this same function — which all
re-fetch the winner and return it as a normal `Resolved` — this one returns
`Err(ResolveError::AddressConflict { existing_customer_id })`, which `src/ingest/model.rs`
maps to `409 Conflict` and rejects the send outright.

This contradicts DESIGN.md §4.7 directly: "**Never reject a send because resolution
failed.** A missing timeline entry is bad; a blocked OTP is worse." An `AddressConflict` is
not evidence of a resolution *failure* — it is evidence that the caller's asserted identity
disagrees with what the address index already knows, which is a real, meaningful signal,
but a signal at least worth recording rather than one that should block the send it arrived
on. Options to weigh at refinement, not decided here:

- Send under the caller-supplied `customer_id`/external id, using the winner's `address_id`
  (a customer's message still goes out under the identity the caller vouched for) — records
  the conflict as a `comms_event`-adjacent note or a dedicated audit row rather than blocking.
- Send under the winning `customer_id` instead (identity resolution defers to the address
  index, since two customers cannot legitimately share one currently-active destination) —
  same effect as every other lost-race path in this function already has.

**Decided during this ticket's refinement:** the address index wins — resolve to the winning
`customer_id`/`address_id`, the same outcome every other lost-race branch in this function
already returns, rather than sending under the caller's asserted identity with a logged note.
This needed a decision, not a silent pick, because it is a materially different case from the
address-only race above: it can move a send from the customer the caller vouched for to a
different, already-real customer that happens to hold the destination now.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-018-customer-address-races
```

### Prerequisite gate (hard)

None. `depends-on: []`, no unmerged branch this ticket builds on.

### Confirmed design decisions (do not deviate without asking)

1. **Defer `customer_dek` persistence until the customer+address transaction commits, not
   just its Vault mint.** `mint_provisional_customer_and_address`
   (`src/customer/resolve.rs`) still calls `keystore.create_dek` up front — the address
   ciphertext needs the plaintext DEK before the insert is attempted — but no longer calls
   `customer_dek::repo::insert_if_absent` until after `tx.commit()` succeeds. A lost race then
   discards the freshly-minted `Dek` (plaintext + wrapped) entirely; Vault's Transit
   `generate-data-key` call has no server-side artifact for a discarded datakey, so there is
   nothing to clean up. `get_or_create_dek` is not touched — its cache/DB-lookup path exists
   for a `customer_id` that may already have a DEK, which is never true for the fresh
   `customer_id` minted inside this function.
2. **`AddressConflict` stops being an error; resolve to the winner instead.** In `resolve()`'s
   Explicit/External path (`src/customer/resolve.rs`, around line 250), when the
   `(kind, value_hmac)` insert loses the race against a different customer's already-active
   address, return the winner's `customer_id`/`address_id` (re-fetching the winner's locale the
   same way the `AddressOnly` branch already does at lines 176-179) instead of
   `Err(ResolveError::AddressConflict)`. Matches DESIGN.md §4.7 ("never reject a send because
   resolution failed") and every other lost-race branch in this function. Supersedes T-015's
   Confirmed design decision 9 (`tickets/6-done/T-015-customer-projection-resolution-at-ingest.md`)
   — that record is left as-is (what T-015 actually shipped); this ticket's decision list is
   what governs from here on.
3. **Remove the now-dead error path** rather than leave an unreachable variant:
   `ResolveError::AddressConflict` (`src/customer/resolve.rs`) and `IngestError::AddressConflict`
   plus its `409 CONFLICT` mapping (`src/ingest/model.rs`).
4. **Update every comment/doc citing the old behaviour in the same commit** — the `(decision 9)`
   comments in `src/customer/resolve.rs` and `src/customer/repo.rs:200`, the doc comment on
   `IngestError::AddressConflict` in `src/ingest/model.rs`, and the `409`/"one exception" prose
   in `docs/user-manual/ingest.adoc` and `docs/user-manual/control-plane-cli.adoc`'s "Customer
   projection" section — so nothing in the tree still describes the removed behaviour.

### Tasks

#### Task 1 — Defer `customer_dek` persistence past the commit point

`src/customer/resolve.rs`, `mint_provisional_customer_and_address`:

- Replace the `get_or_create_dek(...)` call with `keystore.create_dek(vault_mount).await?`
  directly (skip the cache/DB lookup — `customer_id` is always fresh here).
- Keep encrypting the address ciphertext with `dek.plaintext` as today.
- On the `inserted` branch, after `tx.commit().await?`, call
  `crate::customer_dek::repo::insert_if_absent(pool, customer_id, &dek.wrapped, now).await?`
  and `dek_cache.put(customer_id, dek.plaintext)` before returning
  `Ok((customer_id, address_id))`.
- On the lost-race branch (`tx.rollback()`), persist and cache nothing — `dek` simply drops.

#### Task 2 — Resolve `AddressConflict` to the winner instead of rejecting

`src/customer/resolve.rs`, `resolve()`'s Explicit/External tail (around line 247):

- Replace `if winner.customer_id != customer_id { return Err(...) }` with: re-fetch the
  winner's locale via `repo::find_by_id(pool, winner.customer_id).await?` +
  `non_provisional_locale`, then `return Ok(Resolved { customer_id: winner.customer_id,
  address_id: winner.id, locale: winner_locale })`.
- Remove the now-unreachable `ResolveError::AddressConflict` variant and its `Display`/`source`
  match arms and doc comment (lines ~39-44, 56-63, 72).

#### Task 3 — Remove the dead error path from the ingest error surface

`src/ingest/model.rs`: remove `IngestError::AddressConflict`, its `From<ResolveError>` arm, its
`Display` arm, and its `StatusCode::CONFLICT` mapping.

#### Task 4 — Update stale comments and docs

- `src/customer/repo.rs:200`, `src/customer/resolve.rs`'s decision-9 comment: reword to the
  current behaviour, drop the stale `(decision 9)` citation.
- `docs/user-manual/ingest.adoc` (~line 66-69): remove the `409`/`AddressConflict` sentence;
  state resolution never rejects a send (no exception left).
- `docs/user-manual/control-plane-cli.adoc` "Customer projection" section (~line 91-94): remove
  the "(`409` is the one exception ...)" parenthetical; resolution always returns a resolved id.

#### Task 5 — Tests

`tests/customer.rs`:

- Add `total_customer_dek_count(pool: &PgPool) -> i64` (mirrors `total_customer_count`,
  `SELECT count(*) FROM customer_dek`).
- Extend `concurrent_address_only_resolution_mints_exactly_one_provisional_customer`: capture
  `total_customer_dek_count` before/after alongside the existing customer count, assert it also
  increases by exactly 1 — proves the loser's `customer_dek` row was never persisted.
- Replace `conflicting_customer_ids_over_the_same_destination_is_rejected` with
  `conflicting_customer_ids_over_the_same_destination_resolves_to_the_winner`: assert `result_b`
  is `Ok`, and its `customer_id`/`address_id` equal `resolved_a`'s.

### Acceptance test

- `just test` green — includes the Task 5 cases above (needs the local Postgres+Vault stack per
  the existing `tests/customer.rs` harness).
- `just lint` and `just build` clean.
- `just docs-check` clean.
- `cargo test --test customer concurrent_address_only_resolution_mints_exactly_one_provisional_customer -- --nocapture`
  and
  `cargo test --test customer conflicting_customer_ids_over_the_same_destination_resolves_to_the_winner -- --nocapture`
  both pass individually.

### Docs update (mandatory when user-facing)

`docs/user-manual/ingest.adoc` and `docs/user-manual/control-plane-cli.adoc` per Task 4 — both
describe `POST /comms`'s resolution/error contract to producers, which this ticket changes.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just docs-check` clean.
2. Docs updated per the Docs update step above.
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggested commit message:

   ```
   fix(customer): stop orphaning customer_dek rows and rejecting address-conflict sends (T-018)
   ```

5. Tidy WIP commits into atomic ones (root-path child, `path = "."`).
6. Commit locally on `feat/T-018-customer-address-races`. Do not push or open a merge request
   without user approval — present the commit message first. Under `layout = "in-tree"`, before
   pushing verify the remote base isn't behind (`git fetch origin main && git diff --name-only
   origin/main...HEAD | grep '^tickets/'` must print nothing). Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: review: T-015's review (F2) found a lost address-only mint race leaves an orphaned, unreachable `customer_dek` row — narrow but genuine, batched here since it needs design thought (restructure vs. sweep), not a one-line fix.
- 2026-09-02 — scope widened, retitled (TO DO). source: audit: design/implementation audit found `resolve()`'s AddressConflict path rejects a send, contradicting DESIGN.md §4.7's "never reject a send because resolution failed" — same `(kind, value_hmac)` index as this ticket's existing scope, folded in rather than filed separately.
- 2026-09-04 — TO DO → READY: plan complete
- 2026-09-04 — READY → IN DEVELOPMENT: picked up
- 2026-09-04 — plan amended inline: Task 5 also had to fix `tests/ingest.rs`'s `same_destination_under_two_customer_ids_is_conflicted` (asserted `409` at the HTTP layer) — missed during refinement and the pickup applicability audit, which only checked `tests/customer.rs`. Renamed to `same_destination_under_two_customer_ids_resolves_to_the_first` and rewritten to assert `201` and that the second send's `comms_request.customer_id` resolves to the first (winning) customer.
- 2026-09-04 — IN DEVELOPMENT → IN REVIEW: acceptance green
