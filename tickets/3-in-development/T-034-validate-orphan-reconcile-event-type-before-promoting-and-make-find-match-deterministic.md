---
id: T-034
title: Validate orphan-reconcile event_type before promoting and make find_match deterministic
project: messgr
depends-on: []
spawned-by: [T-030]
impact: medium
complexity: low
cost: S
---

# T-034 — Validate orphan-reconcile event_type before promoting and make find_match deterministic

## Outcome

`orphan_reconcile` refuses to promote a row whose `event_type` isn't a recognized status/event
value instead of writing it straight into `comms_request.final_status`, and `find_match`'s row
choice is deterministic (and tested) when more than one `comms_event` row shares a
`provider_ref`.

## Description

T-030's review (findings F4, F5) found two related gaps in `src/orphan_reconcile`'s
matching/promotion path, both surfaced because this is the first codepath where a value that
can originate from third-party (eventually `messgr-webhook`) input reaches
`comms_request.final_status` without validation:

- **F4 (event_type validation).** `reconcile::should_advance`'s `None => true` arm advances
  unconditionally regardless of what `new_event_type` actually is, and `repo::promote` then
  binds `orphan.event_type` directly into `comms_request.final_status`. Neither
  `comms_event.event_type` nor `orphan_event.event_type` has a database `CHECK` constraint
  (`migrations/tenant/0004_ledger_outbox_schema.sql` only documents the valid set in a
  comment). `dispatcher::repo::write_terminal` has the same latitude today, but that path's
  `final_status` argument is dispatcher-internal, never third-party-controlled — this ticket's
  gap is specific to `orphan_reconcile` being the first path where it can be.
- **F5 (nondeterministic match).** `repo::find_match`'s `WHERE ce.provider_ref = $1 LIMIT 1`
  has no `ORDER BY`, so its row choice is undefined when multiple `comms_event` rows
  legitimately share a `provider_ref` (e.g. `sent` and `delivered` on the same request). It
  happens to resolve to the same `comms_request_id` either way today, but that is untested and
  unstated.

Scope: validate `orphan.event_type` against the documented `comms_event.event_type` set before
promoting (treat an unrecognized value as a non-match, aged out through the existing cap path
rather than promoted) and add `ORDER BY occurred_at DESC` to `find_match` plus a test pinning
the multi-match behavior. Soft coupling: touches the same module as T-033
(`src/orphan_reconcile`), filed separately because the two are independently schedulable.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-034-orphan-event-type-validation
```

### Prerequisite gate (hard)

None. No `depends-on:`. `spawned-by: T-030` is already merged to `main` (PR #48, 4e5d31e).

### Confirmed design decisions (do not deviate without asking)

1. **The valid `event_type` set is the union of the existing `STATUS_ORDER` and
   `ABSORBING_STATUSES` constants already in `reconcile.rs`**, not a new hardcoded list — the
   two together already enumerate exactly the 12 values migration
   `0004_ledger_outbox_schema.sql`'s comment documents (`queued | sent | delivered | failed |
   bounced | read | complaint | expired | cancelled | suppressed_consent | suppressed_list |
   unverified_address`). A third list would be a second place for that set to drift out of
   sync.
2. **An unrecognized `event_type` is treated as a non-match, not an error.** `run()` skips the
   `find_match` DB lookup entirely for such a row and falls through the existing `None` arms —
   so it accrues `reconcile_attempts` and eventually ages out through the pre-existing cap path
   (`RECONCILE_ATTEMPTS_CAP`), exactly like a `provider_ref` that matches nothing. No new error
   variant, no separate deletion path, no change to `should_advance` or `repo::promote`.
3. **No database `CHECK` constraint added.** F4 scoped this to the reconcile path, not the
   schema — `comms_event`/`orphan_event.event_type` stays a documented-by-comment `text`
   column, matching `dispatcher::repo::write_terminal`'s existing latitude on the same column.
   Out of scope per this ticket's Description.
4. **`find_match` adds `ORDER BY ce.occurred_at DESC` before its existing `LIMIT 1`.** When
   several `comms_event` rows share a `provider_ref`, the most recently occurred one wins —
   the same "a later receipt is more authoritative" reasoning `should_advance` already applies
   at the request level. No new column needed; `comms_event.occurred_at` is already in the
   joined table.

### Tasks

#### Task 1 — reject unrecognized `event_type` before matching (`src/orphan_reconcile/reconcile.rs`)

Add a private helper beside `STATUS_ORDER`/`ABSORBING_STATUSES`:

```rust
fn is_recognized_event_type(event_type: &str) -> bool {
    STATUS_ORDER.contains(&event_type) || ABSORBING_STATUSES.contains(&event_type)
}
```

In `run()`, gate the lookup so an unrecognized `orphan.event_type` never reaches `find_match`/
`promote_match`:

```rust
let m = if is_recognized_event_type(&orphan.event_type) {
    repo::find_match(tenant_pool, &orphan.provider_ref).await?
} else {
    None
};
match m {
    Some(m) => { /* unchanged */ }
    None if orphan.reconcile_attempts + 1 >= RECONCILE_ATTEMPTS_CAP => { /* unchanged */ }
    None => { /* unchanged */ }
}
```

#### Task 2 — deterministic `find_match` (`src/orphan_reconcile/repo.rs`)

Add `ORDER BY ce.occurred_at DESC` immediately before the existing `LIMIT 1` in `find_match`'s
query. No other change to the query or `Match`.

#### Task 3 — unit test for the validation gate (`src/orphan_reconcile/reconcile.rs`)

Add a `#[cfg(test)] mod tests` exercising `is_recognized_event_type` directly (no DB): every
`STATUS_ORDER` and `ABSORBING_STATUSES` value returns `true`; an arbitrary unrecognized string
(e.g. `"made_up_status"`) and `""` return `false`.

#### Task 4 — integration tests (`tests/orphan_reconcile.rs`)

Two new `#[tokio::test]`s, using the file's existing `TestTenant`/`insert_*` helpers:

- **Unrecognized `event_type` ages out instead of promoting.** Insert a `comms_request` +
  `comms_event` pair with `provider_ref = "abc"`, and an `orphan_event` row with the same
  `provider_ref` but `event_type = "made_up_status"`. Run `reconcile::run_for_tenant`. Assert
  `report.reconciled == 0` and `report.still_pending == 1`; assert the orphan's
  `reconcile_attempts` incremented by 1 and no new `comms_event` row was written for it.
- **Multi-match resolves to the most recently occurred row.** Insert two
  `comms_request`/`comms_event` pairs (distinct `customer_id`) sharing one `provider_ref` but
  different `occurred_at`, plus one `orphan_event` row carrying that `provider_ref`. Run
  reconcile and assert the promoted `comms_event`/`final_status` update landed against the pair
  with the **later** `occurred_at` — pinning the `ORDER BY ... DESC` choice (F5) instead of
  leaving it to whatever Postgres happens to return.

### Acceptance test

```
just build
just test    # includes the two new tests/orphan_reconcile.rs cases and the new reconcile.rs unit test
just lint
just docs-check
```

All green; the five pre-existing `tests/orphan_reconcile.rs` cases still pass unchanged.

### Docs update (mandatory when user-facing)

No user-facing surface. No new config, CLI flag, or API — `03-data-model.md`'s `orphan_event`/
`comms_event` schema comments already document the valid `event_type` set this ticket now
enforces in code; nothing there changes.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. No docs to update (see above).
3. Write a summary of files touched and decisions made.
4. Suggest a Conventional Commit message, e.g.:

   ```
   fix(orphan-reconcile): validate event_type and order find_match deterministically (T-034)
   ```

5. Tidy WIP commits into a small number of atomic commits before presenting (root-path child,
   rules §0).
6. Commit locally on the ticket branch. Do not push or open a merge request without user
   approval; present the commit message and, once approved, finalize, verify the remote base
   is not behind, push, and open the merge request. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F4, F5) found orphan_reconcile promotes an unvalidated event_type into comms_request.final_status and find_match's row choice is nondeterministic when provider_ref is shared — batched into one follow-up ticket.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
