---
id: T-018
title: Clean up orphaned customer_dek rows from lost address-only mint races
project: messgr
depends-on: []
spawned-by: [T-015]
impact: medium
complexity: medium
cost: M
---

# T-018 — Clean up orphaned customer_dek rows from lost address-only mint races

## Outcome

A lost address-only provisional-mint race no longer leaves a permanent, unreferenced
`customer_dek` row (and its live Vault Transit datakey) behind — either the row never gets
created for the losing attempt, or it is swept up after the fact.

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

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: review: T-015's review (F2) found a lost address-only mint race leaves an orphaned, unreachable `customer_dek` row — narrow but genuine, batched here since it needs design thought (restructure vs. sweep), not a one-line fix.
