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

**Reviewer independence (step 0):** the implementing agent authored this branch in the same
session, so the implementation/quality/consistency/docs audits (steps 2–4a) were delegated to an
independent sub-agent (spawned fresh, no memory of writing the code, briefed adversarially).
Every delegated finding was re-verified by hand before being recorded below (grep/read the cited
evidence directly; ran `just build`/`just test`/`just lint`/`just docs-check` myself). Classification,
severity, disposition, and the routing/move decisions stayed with the reviewing session
throughout.

Gates re-run independently and by hand: `just build`, `just test` (all suites, including the 7
`tests/orphan_reconcile.rs` cases and the 4 `orphan_reconcile::reconcile::tests` unit tests),
`just lint`, `just docs-check` — all green both before and after the fixes below.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | docs-gap | — | `docs/user-manual/control-plane-cli.adoc`'s "Orphan-event reconciliation" section states "a row with no match has its `reconcile_attempts` counter incremented and is deleted once that counter reaches a fixed cap of 5" — no longer true: a row whose `event_type` is outside the documented set is now also incremented/deleted, without ever reaching `find_match`. | `docs/user-manual/control-plane-cli.adoc` (Orphan-event reconciliation section, single paragraph) | Add a clause covering the unrecognized-`event_type` case in the same sentence. Addendum step 4a names this exact failure mode ("the manual has previously documented a message-loss behaviour as if it were a designed feature"). |
| F2 | non-blocking | stale-xref | fixed inline | `03-data-model.md` §4.4's `comms_event.event_type` comment (and migration 0004's copy of it) never listed `discarded`, which `src/dispatcher/drain.rs`'s `write_discarded` has written since that code shipped — pre-existing drift, surfaced by this review's consistency audit because T-034 now enforces exactly that list. No live bug (a provider will never send `discarded`). | `development/design/03-data-model.md` §4.4; `migrations/tenant/0004_ledger_outbox_schema.sql:69-71`; `src/dispatcher/drain.rs:187-198` | Fixed: added `discarded` to §4.4's comment + a Correction note; DESIGN.md bumped to Version 5. Addendum step 5 requires this fix in-review, not deferred. |
| F3 | non-blocking | correctness | fixed inline | `find_match`'s new `ORDER BY ce.occurred_at DESC` is not a total order: two `comms_event` rows sharing both `provider_ref` and `occurred_at` (distinct `comms_request_id`) still tie-break arbitrarily, short of the ticket's own "deterministic" Outcome. | `src/orphan_reconcile/repo.rs` (`find_match` query) | Fixed: added `ce.comms_request_id DESC` as a tiebreaker. |
| F4 | non-blocking | test-gap | fixed inline | Two of the four new `is_recognized_event_type` unit tests iterate `STATUS_ORDER`/`ABSORBING_STATUSES` and assert the function recognizes its own inputs — tautological, can't catch either constant drifting from migration 0004's documented 12 values. | `src/orphan_reconcile/reconcile.rs` (`tests` module) | Fixed: added `recognized_set_matches_the_documented_twelve_event_types`, pinning the union against the literal migration-comment list. |
| F5 | non-blocking | design | noted | The `event_type` guard lives only in `run()`; `promote_match`/`repo::promote` still accept any value, and decision 3 declined a DB `CHECK`. No live bypass today (`run()` is the sole caller), but the doc comment anticipates `messgr-webhook` as a future input source, which — calling lower layers directly — would have nothing to catch it. Decision 2 explicitly scoped validation to this placement, so the branch is compliant; this is a forward-looking gap, not a defect in what shipped. | `src/orphan_reconcile/reconcile.rs` (`run`), `repo.rs` (`promote`) | Carry onto the `messgr-webhook` ticket, or revisit the `CHECK` constraint decision (T-034 decision 3) there. |
| F6 | non-blocking | stale-xref | fixed inline | The new `ORDER BY` gives up the early-partition-termination a bare `LIMIT 1` allowed, which is exactly what migration 0011's own comment justified the `provider_ref` index against ("otherwise scan every partition") — neither 0011 nor the original `find_match` doc comment noted the plan-shape change this ticket introduces. | `migrations/tenant/0011_comms_event_provider_ref_index.sql`; `src/orphan_reconcile/repo.rs` (`find_match` doc comment) | Fixed: added a clause to `find_match`'s doc comment noting the partition-scan implication. |
| F7 | non-blocking | design | folded | T-033 (still `2-ready/`, not yet built) plans a `tracing::warn!` on age-out carrying `tenant_slug`/`id`/`provider_ref` but not `event_type` (its confirmed decision 5). Once T-033 lands, its warn can't distinguish "no match" from "matched but rejected on `event_type`" — the second age-out reason T-034 just introduced. | `tickets/2-ready/T-033-...md` decision 5, Task 5 | Folded into T-033: patched decision 5 and Task 5's `tracing::warn!` to add `event_type`, with a dated History note on T-033 (impact sweep, review-protocol.md step 8). |

**Disposition summary:** 1 blocking (F1, routed to rework — not dispositioned). 4 `fixed inline`
(F2, F3, F4, F6). 1 `noted` (F5). 1 `folded` (F7, into T-033). 0 `new ticket`.

cost: estimated S, actual S — the review surfaced more findings than the S estimate implies
effort for, but every fix was a one- or two-line change; no re-estimate warranted.

- [x] Reviewer independence settled (step 0): delegated, findings re-verified by hand
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a)
- [x] Docs-readability pass — conscious skip: no docs-readability reviewer configured in this
      host/session
- [x] Findings recorded with severity, class, disposition; disposition summary + cost line above
      (step 5)
- [x] Ticket moved to `tickets/5-rework/` for F1 (step 6)
- [x] Other references updated: T-033 patched (folded F7); governing documents reconciled
      (F2 — DESIGN.md Version 5, `03-data-model.md` §4.4); board regenerated by the move (step 7)
- [x] Remaining-tickets impact sweep done (step 8): T-033 re-read and patched; no other
      `2-ready/`/`1-to-do/` ticket references T-034 or `src/orphan_reconcile`
- [ ] Summary + commit message & MR attributes presented for approval (step 9) — pending F1's
      rework and scoped re-review

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F4, F5) found orphan_reconcile promotes an unvalidated event_type into comms_request.final_status and find_match's row choice is nondeterministic when provider_ref is shared — batched into one follow-up ticket.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
- 2026-09-15 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-15 — IN REVIEW → REWORK: F1: user manual now misstates the age-out invariant
