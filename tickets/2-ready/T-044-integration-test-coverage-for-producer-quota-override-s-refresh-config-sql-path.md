---
id: T-044
title: Integration test coverage for producer_quota_override's refresh_config SQL path
project: messgr
depends-on: []
spawned-by: [T-042]
impact: low
complexity: low
cost: S
---

# T-044 — Integration test coverage for producer_quota_override's refresh_config SQL path

## Outcome

After this ships, a broken `producer_quota_override`/`producer_quota` real-DB read path (the
`WHERE valid_from <= $1 AND valid_to > $1` filter and the merge-into-effective-limit loop in
`QuotaTracker::refresh_config`) fails a test instead of shipping silently.

## Description

T-042's review (F1) found that `QuotaTracker`'s override logic is only exercised at the unit
level with a hand-mutated `config` map, never through `refresh_config`'s actual SQL query against
`producer_quota`/`producer_quota_override`. `tests/producer_quota.rs`'s
`flushing_and_rebuilding_preserves_in_progress_window_counts` is the closest existing precedent
(drives `QuotaTracker` against a real tenant pool) but covers usage flush/rebuild, not the
override-merge path.

Add to `tests/producer_quota.rs`, alongside that test:

- one case that inserts a real `producer_quota` row plus an active `producer_quota_override` row
  (via `producer_quota::configure::set_producer_quota` /
  `producer_quota::configure::add_producer_quota_override`), calls `QuotaTracker::refresh_config`
  against the same pool, and asserts the raised `per_day` limit is actually in effect
  (`check_and_record` admits past the base limit, defers past the override's raised one);
- one case with an override whose `valid_to` is already in the past, asserting `refresh_config`
  does **not** raise the limit (the base `per_day` still applies).

This matches messgr's own most-repeated defect class named in `development/review-addendum.md`
(§3's "an assertion must be able to fail" — T-001/F13, T-003/F1, T-004/F2): the existing unit
tests would keep passing even if `refresh_config`'s SQL filter or merge loop were broken, because
neither one drives that code path at all.

Soft coupling: none — builds and tests entirely inside `tests/producer_quota.rs` against
already-shipped `producer_quota`/`producer_quota_override` tables and `QuotaTracker` API.

## Implementation Plan

### 0. Feature branch (mandatory)

Inside this repo (`messgr`, `path = "."`):

```
git checkout main
git checkout -b feat/T-044-producer-quota-override-refresh-config-test
```

WIP commits locally as you go. Publish only per the project's commit policy (no push/MR
without explicit user approval); tidy WIP commits into atomic ones before presenting
(root-path child).

### Prerequisite gate (hard)

None — `depends-on: []`, and the tables/API this ticket exercises
(`producer_quota`/`producer_quota_override`, `producer_quota::configure`, `QuotaTracker`) are
already merged (T-042, PR #58).

### Confirmed design decisions (do not deviate without asking)

1. **Both new tests live in `tests/producer_quota.rs`, next to
   `flushing_and_rebuilding_preserves_in_progress_window_counts`.** That test is the only
   existing precedent for driving a real tenant pool + `QuotaTracker` together; reuse its
   provisioning helpers (`provision_test_tenant`, `register_test_producer`,
   `control_database_url`, `vault_keystore`, `unique_name`, `drop_test_tenant`) rather than
   duplicating setup.
2. **Drive the override through the real `configure` API, not direct SQL inserts.** Use
   `set_producer_quota` for the base row and `add_producer_quota_override` for the override,
   exactly as production code would populate these tables — a hand-written `INSERT` would
   validate the test's assertions but not the tables' actual write path.
3. **The assertion that proves the SQL path works is an admit/defer boundary, not an equality
   check on `EffectiveQuota`.** `check_and_record` calls up to and past the limit, following
   `flushing_and_rebuilding_preserves_in_progress_window_counts`'s and
   `an_active_override_raises_the_effective_per_day_limit`'s existing pattern — an assertion
   that can actually fail if the SQL filter or merge loop breaks (review-addendum §3).
4. **Base quota uses `enforcement::HARD` and a `per_day` of `1`.** A hard limit turns "did the
   override actually raise the ceiling" into an observable defer point a few calls later,
   rather than a decision that would come out the same whether the override applied or not.

### Tasks

#### Task 1 — active override raises the effective limit through `refresh_config`

Add `producer_quota_override_raises_the_limit_through_refresh_config` to
`tests/producer_quota.rs`, alongside `flushing_and_rebuilding_preserves_in_progress_window_counts`:

- provision a tenant + producer (same helpers as the neighbouring test);
- `set_producer_quota(..., "sms", class::MARKETING, None, Some(1), enforcement::HARD, "test-actor")`
  — base `per_day` of 1;
- `add_producer_quota_override(..., "sms", class::MARKETING, 5, now - 1h, now + 1h,
  "test-approver", "test override", "test-actor")` — active window straddling `now`;
- `let tracker = QuotaTracker::new("UTC"); tracker.refresh_config(&tenant_pool, now).await`;
- call `tracker.check_and_record(producer_id, "sms", class::MARKETING, now)` 5 times, asserting
  `QuotaDecision::Admit` each time (the raised limit of 5, not the base 1); the 6th call must
  be `QuotaDecision::Defer(_)`;
- close the tenant pool, `drop_test_tenant`.

#### Task 2 — an expired override does not raise the limit

Add `an_expired_override_is_not_merged_by_refresh_config` to the same file:

- provision a tenant + producer;
- `set_producer_quota(..., "sms", class::MARKETING, None, Some(1), enforcement::HARD, "test-actor")`
  — same base row;
- `add_producer_quota_override(..., "sms", class::MARKETING, 5, now - 2 days, now - 1 day, ...)`
  — `valid_to` already in the past;
- `refresh_config(&tenant_pool, now)`;
- call `check_and_record` once: `QuotaDecision::Admit`; call it again: must be
  `QuotaDecision::Defer(_)` (base limit of 1 still applies — the expired override was never
  merged into `config`);
- close the tenant pool, `drop_test_tenant`.

### Acceptance test

```
just test
```

(runs the full suite, including the two new `tests/producer_quota.rs` cases against the local
stack — same command the neighbouring tests already require). Both new tests must fail if
`refresh_config`'s `WHERE valid_from <= $1 AND valid_to > $1` filter or the
`per_day = max(base, override)` merge loop is reverted or broken — confirm this locally by
temporarily reverting `refresh_config`'s merge loop (`src/producer_quota/tracker.rs`, the `for o
in overrides` block) to a no-op and observing both new tests fail, then restoring it.

### Docs update (mandatory when user-facing)

no user-facing surface — internal test coverage only, no `DESIGN.md` or API surface changes.

### Finish (mandatory)

1. `just test` green (including the two new tests), `just build` and `just lint` clean.
2. No docs to update (see above).
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggested commit message:

   ```
   test(producer-quota): cover override merge through refresh_config's SQL path (T-044)

   Drives QuotaTracker::refresh_config against a real producer_quota/producer_quota_override
   pool instead of a hand-mutated config map, so a broken WHERE filter or merge loop fails a
   test instead of shipping silently (T-042 review F1).
   ```

5. Tidy WIP commits into a small number of atomic commits (root-path child) before presenting.
6. Commit locally on `feat/T-044-producer-quota-override-refresh-config-test`. Do not push or
   open an MR without user approval; present the commit message and hand back.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-18 — created (TO DO). source: review: T-042's review (F1) found `refresh_config`'s
  real SQL override path untested end-to-end.
- 2026-09-19 — TO DO → READY: plan complete
