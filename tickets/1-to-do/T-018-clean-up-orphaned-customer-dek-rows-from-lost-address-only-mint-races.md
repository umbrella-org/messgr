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
`customer_dek` row (and its live Vault Transit datakey) behind — either the row never gets
created for the losing attempt, or it is swept up after the fact. Separately, resolving a
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

Two directions worth weighing during refinement, not decided here: (a) restructure
`mint_provisional_customer_and_address` so the `customer` row commits before the DEK is
requested (harder — the address row's ciphertext needs the DEK before the same insert), or
(b) a periodic sweep that finds `customer_dek` rows with no matching `customer` row and either
backfills a tombstone customer or deletes the orphaned key. `pre_provision_deks`
(`src/customer_dek/lifecycle.rs`) already creates `customer_dek` rows ahead of any `customer`
row on purpose, so whichever direction is chosen must not break that path.

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

Either resolves the contradiction; which one is a product decision (does the caller's
asserted identity or the address index win a genuine conflict), not an implementation detail,
so it needs a decision recorded during this ticket's refinement rather than picked silently.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: review: T-015's review (F2) found a lost address-only mint race leaves an orphaned, unreachable `customer_dek` row — narrow but genuine, batched here since it needs design thought (restructure vs. sweep), not a one-line fix.
- 2026-09-02 — scope widened, retitled (TO DO). source: audit: design/implementation audit found `resolve()`'s AddressConflict path rejects a send, contradicting DESIGN.md §4.7's "never reject a send because resolution failed" — same `(kind, value_hmac)` index as this ticket's existing scope, folded in rather than filed separately.
