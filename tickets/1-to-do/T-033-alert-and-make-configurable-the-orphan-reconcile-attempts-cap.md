---
id: T-033
title: Alert and make configurable the orphan-reconcile attempts cap
project: messgr
depends-on: []
spawned-by: [T-030]
impact: low-medium
complexity: low
cost: S
---

# T-033 — Alert and make configurable the orphan-reconcile attempts cap

## Outcome

An orphan_event row that exhausts `RECONCILE_ATTEMPTS_CAP` and gets deleted now emits a
`tracing::warn!` instead of vanishing silently, and the cap itself is a `tenant_config` field
an operator can tune instead of a compiled constant.

## Description

T-030's review (finding F2/F3) found `src/orphan_reconcile` deviates from what
`development/design/03-data-model.md`'s own correction note (§4.4/§10) requires of
`reconcile_attempts`: "a row that fails to reconcile past a small bound (**config, not
hardcoded**) **pages someone** rather than accumulating as a permanent plaintext-PII table."
Today `RECONCILE_ATTEMPTS_CAP` is `pub const RECONCILE_ATTEMPTS_CAP: i16 = 5` in
`src/orphan_reconcile/reconcile.rs` (no config path), and `repo::record_miss`'s cap-exhaustion
delete path emits nothing — no `tracing::warn!`, no `platform_audit` row, nothing an operator
could alert on. The sibling one-shot job in the same family,
`src/partition_lifecycle/lifecycle.rs`'s `retention_skipped` branch, already sets the codebase
convention of `tracing::warn!` for an analogous "this would otherwise silently lose data"
condition.

Scope: move the cap into the existing `tenant_config` mechanism (the same pattern
`kill_switch_release_rate` already uses for a per-tenant tunable with a hardcoded default),
and add a `tracing::warn!` (tenant slug, orphan id, provider_ref) when
`orphan_reconcile::reconcile::run` ages a row out. Soft coupling: touches the same module as
T-034 (`src/orphan_reconcile`), filed as a separate ticket because the two are independently
schedulable and address different concerns (observability/configurability here vs. input
validation there).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F2, F3) found the reconcile-attempts cap is hardcoded and its exhaustion path emits no alert, both contradicting DESIGN.md §4.4/§10's own correction note — batched into one follow-up ticket.
