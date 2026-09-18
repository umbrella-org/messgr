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

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-18 — created (TO DO). source: review: T-042's review (F1) found `refresh_config`'s
  real SQL override path untested end-to-end.
