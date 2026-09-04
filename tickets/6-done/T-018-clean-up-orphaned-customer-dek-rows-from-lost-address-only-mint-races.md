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

- [x] Reviewer independence settled (step 0): the reviewing agent authored this branch in this
  same session, so audits (steps 2-4a) were **delegated** to a fresh, independent sub-agent
  briefed adversarially against `feat/T-018-customer-address-races` (commit `77558e8`), `AGENTS.md`'s
  ten hard invariants, `development/review-addendum.md`, and DESIGN.md §4.6/§4.7/§7.1-7.2. Every
  delegated finding below was re-verified by hand before being recorded.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2): all
  four Tasks and both Confirmed design decisions done as written. `just build`, `just lint`,
  `just docs-check`, `just test` (full suite, all 17 binaries) all green, including
  `tests/customer.rs` (9/9) and `tests/ingest.rs` (11/11); both acceptance-named tests pass in
  isolation.
- [x] Quality audit (step 3): idiomatic, no dead code from the removed `AddressConflict` paths.
  One finding (F1, below) on the DEK-persistence ordering.
- [x] Consistency audit (step 4): grepped the whole tree for `AddressConflict`/`409` — the only
  remaining hits are `tickets/6-done/T-015-*.md` (historical record, correctly left as-is per
  this ticket's own Description) and this ticket's own regression-test comments. Nothing live
  still describes the removed behaviour.
- [x] Documentation audit (step 4a): `just docs-check` clean; both changed `.adoc` files now
  correctly state resolution never rejects a send, including the conflict case. No other doc
  references the old `409` contract.
- [x] Docs-readability pass (step 4b): **conscious skip** — no docs-readability reviewer
  (tool/subagent) is configured in this environment.
- [x] Findings recorded (step 5, table below); disposition summary and cost line present.
- [x] Ticket moved (step 6): round 1 → `5-rework/` (F1), fixed same session, → `4-in-review/`;
  scoped re-review (round 2, delegated) confirmed clean → `6-done/`.
- [x] Other references updated; governing documents reconciled (step 7): grepped
  `development/design/` for `AddressConflict`/`409`/"decision 9" — none found. DESIGN.md §4.7
  already stated "never reject a send"; this ticket fixed code contradicting an already-correct
  design, so no DESIGN.md edit is owed. `tickets/6-done/T-015-*.md`'s own decision-9 record is
  historical and correctly left as-is (its Description already says so).
- [x] Remaining-tickets impact sweep (step 8): no ticket in `1-to-do/` or `2-ready/` lists T-018
  in `depends-on:` or references it in Description. Nothing to patch.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | plan-wrong | — | Confirmed design decision 1 (persist `customer_dek` only after `tx.commit()`) opened a worse failure mode than the one it closed: on the *winning* path, a crash or DB error between `tx.commit()` (which durably writes `customer_address` with ciphertext encrypted under `dek.plaintext`) and the follow-up `insert_if_absent` call leaves a permanent, real customer row whose ciphertext is encrypted under a DEK stored nowhere — unrecoverable, not just orphaned. | `src/customer/resolve.rs:371-377` (pre-fix); contrast the persist-before-use pattern at `src/customer_dek/lifecycle.rs:57-89` | Insert the `customer_dek` row inside the same transaction as `customer`/`customer_address`, so all three commit or roll back atomically — closes both the original orphan and this new window at once. |
| F2 | non-blocking | design | fixed inline | The winning-path `customer_dek_repo::insert_if_absent(...).await?` call discarded its returned bool and unconditionally cached the plaintext, unlike its sibling `get_or_create_dek`, which checks the bool and re-fetches the winner's row on `false`. No behaviour bug (`customer_id` here is always a fresh `Uuid::new_v4()`, so `false` is unreachable), but undocumented. | `src/customer/resolve.rs:374-375` (pre-fix) | Add a comment explaining why ignoring the bool is safe here specifically. |
| F3 | non-blocking | test-gap | noted | The F1 rework fix (commit `9883c50`) shipped with no new/updated test asserting the atomicity guarantee directly (that `customer_dek` can never commit-without or be-missing-after a committed `customer_address`). The existing `concurrent_address_only_resolution_mints_exactly_one_provisional_customer` still passes and incidentally covers the loser-leaves-no-row half, but nothing exercises the commit-atomicity half beyond code inspection. Accepted as `noted`, not promoted: the crash-mid-transaction scenario this fix closes isn't practically simulable in this integration-test harness (no fault-injection hook between a `COMMIT` and the surrounding function returning), so a new test would assert the SQL shape, not the actual guarantee. | `src/customer/resolve.rs` (`mint_provisional_customer_and_address`, post-fix), `src/customer_dek/repo.rs` (`insert_if_absent_tx`) | None actioned — revisit if/when the harness gains a way to inject a mid-transaction fault. |

**Disposition summary:** round 1 — 1 blocking (F1 → `5-rework/`, fixed same round, see rework
record below), 1 non-blocking → fixed inline (F2). Scoped re-review (round 2, delegated,
independent) — confirmed F1 closed and F2 addressed with no new defect beyond 1 non-blocking →
noted (F3).

cost: estimated M, actual M-L (the F1 rework round pushed this past a plain M — one extra
design iteration, a new repo function, and two independent-reviewer delegations)

### Rework fix record — round 1 (commit 9883c50)

- **F1** — Rewrote `mint_provisional_customer_and_address` (`src/customer/resolve.rs`) to insert
  the `customer_dek` row inside the same transaction as `customer`/`customer_address`, via a new
  `customer_dek::repo::insert_if_absent_tx` (`src/customer_dek/repo.rs`). All three rows now
  commit or roll back together: a lost race discards the DEK insert along with the others (no
  orphan), and a won race can never commit `customer_address` without its DEK already durably
  stored in the same transaction (no unrecoverable-ciphertext window).
- **F2** — Fixed in the same commit: added a comment on the now-atomic `insert_if_absent_tx` call
  explaining why ignoring its returned bool is safe (a fresh `Uuid::new_v4()` generated by this
  function alone cannot race any other transaction for the same `customer_id`).
- Re-ran `just build`, `just lint`, and the full `just test` suite after the fix — all green
  (`tests/customer.rs` 9/9, `tests/ingest.rs` 11/11).

### Scoped re-review — round 2 (independent, delegated; reads commit 9883c50)

Verdict: **F1 closed, F2 addressed, approved.** Confirmed by an independent sub-agent (this
reviewing agent authored the round-1 fix in this same session, so step 0 applies again) that
`insert_if_absent_tx` runs on `&mut tx` before `tx.commit()`; both the winner path (all three
rows commit atomically) and the loser path (rollback discards all three, zero orphan) trace
correctly; `dek_cache.put` runs only after `tx.commit()` succeeds; the new SQL is
shape-identical to the original `insert_if_absent`, whose pool-level call site
(`get_or_create_dek`) is untouched and still correct for its own genuine-race case. One
incidental, unrelated observation: `just test`'s first full-suite run hit a flake in
`tests/kill_switch.rs` (`engaged_producer_switch_excludes_only_that_producers_rows`), confirmed
pre-existing (passes in isolation and on a full-suite rerun) and unrelated to this branch's
diff — not a T-018 finding, noted here only for the record.

## History

- 2026-09-01 — created (TO DO). source: review: T-015's review (F2) found a lost address-only mint race leaves an orphaned, unreachable `customer_dek` row — narrow but genuine, batched here since it needs design thought (restructure vs. sweep), not a one-line fix.
- 2026-09-02 — scope widened, retitled (TO DO). source: audit: design/implementation audit found `resolve()`'s AddressConflict path rejects a send, contradicting DESIGN.md §4.7's "never reject a send because resolution failed" — same `(kind, value_hmac)` index as this ticket's existing scope, folded in rather than filed separately.
- 2026-09-04 — TO DO → READY: plan complete
- 2026-09-04 — READY → IN DEVELOPMENT: picked up
- 2026-09-04 — plan amended inline: Task 5 also had to fix `tests/ingest.rs`'s `same_destination_under_two_customer_ids_is_conflicted` (asserted `409` at the HTTP layer) — missed during refinement and the pickup applicability audit, which only checked `tests/customer.rs`. Renamed to `same_destination_under_two_customer_ids_resolves_to_the_first` and rewritten to assert `201` and that the second send's `comms_request.customer_id` resolves to the first (winning) customer.
- 2026-09-04 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-04 — IN REVIEW → REWORK: F1 blocking: customer_dek persistence ordering (plan-wrong)
- 2026-09-04 — REWORK → IN REVIEW: findings fixed
- 2026-09-04 — IN REVIEW → DONE: review clean; F1 fixed same round, F2 fixed inline, F3 noted
- 2026-09-04 — merge request opened: PR #24 (`feat/T-018-customer-address-races`, commits 77558e8, 9883c50) — pending human merge
