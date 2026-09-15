---
id: T-035
title: Assert orphan-reconcile's age-out tracing::warn! actually fires
project: messgr
depends-on: []
spawned-by: [T-033]
impact: low
complexity: low
cost: S
---

# T-035 — Assert orphan-reconcile's age-out tracing::warn! actually fires

## Outcome

`orphan_reconcile::reconcile::run`'s age-out `tracing::warn!` (T-033) has a test that fails if
the warn stops firing or drops a field, closing the one part of T-033's headline behavior that
currently has zero coverage.

## Description

T-033's review (finding F6) found that nothing asserts the `tracing::warn!` T-033 added on the
orphan-reconcile age-out path actually fires. T-033's own Implementation Plan (Task 6)
anticipated this gap and pre-authorized skipping it, on the stated condition that capturing a
`tracing::warn!` in a test would require a new dev-dependency (`tracing_test`) it didn't want to
add for this alone. That condition turns out to be false: `tracing-subscriber` (with the
`env-filter` feature) is already a direct dependency (`Cargo.toml`), and a custom
`tracing_subscriber::layer::Layer` collecting emitted events into a shared `Vec`/`Mutex` needs no
additional crate.

Scope: add a `tracing::subscriber::with_default` (or a project-appropriate equivalent) around a
call to `orphan_reconcile::reconcile::run_for_tenant` in `tests/orphan_reconcile.rs`, using a
minimal capturing layer, and assert the captured event carries `tenant_slug`, `orphan_id`,
`provider_ref`, and `event_type` when a row ages out. Also worth a look while there: T-033's
`unconfigured_tenant_still_ages_out_at_the_hardcoded_default` test is functionally identical to
the pre-existing `no_match_at_cap_deletes_the_row` (same cap, same shape) — decide whether it
earns its own name (a fallback-path regression guard, kept deliberately parallel to the
configured-cap test next to it) or should be dropped in favor of just extending the existing one
with a warn assertion.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: review: T-033's review (finding F6) found the age-out
  `tracing::warn!` has no test coverage, and that the ticket's own stated reason for skipping it
  (no `tracing_test` dependency) doesn't hold — `tracing-subscriber` is already a direct
  dependency and can capture the event without a new one.
