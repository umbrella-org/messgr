---
id: T-033
title: Alert and make configurable the orphan-reconcile attempts cap
project: messgr
depends-on: []
spawned-by: [T-030]
impact: low
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

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-033-orphan-reconcile-cap-config
```

### Prerequisite gate (hard)

None. No `depends-on:`. `spawned-by: T-030` is already merged to `main` (PR #48, 4e5d31e).

### Confirmed design decisions (do not deviate without asking)

1. **The cap becomes a `tenant_config` column, following `kill_switch_release_rate`'s exact
   pattern** (`migrations/tenant/0010_tenant_config_kill_switch_release_rate.sql`,
   `src/tenant_config/{model,repo,configure}.rs`, `src/bin/control.rs`'s `TenantConfigCommand::Set`):
   a new nullable-by-default-but-`NOT NULL DEFAULT`-backed column, a new `TenantConfig`/
   `TenantConfigInput` field, a new optional `--reconcile-attempts-cap` CLI flag that falls back
   to the column's own SQL default when omitted, and a consumer-side
   `tenant_config_repo::load(...).map(|c| c.reconcile_attempts_cap).unwrap_or(DEFAULT)` read —
   mirroring `messgr-dispatcher`'s `release_rate` resolution exactly.
2. **Column type is `smallint`/`i16`**, matching `orphan_event.reconcile_attempts` (also
   `smallint`) and the existing `RECONCILE_ATTEMPTS_CAP: i16` constant it replaces — not `i32`
   like `kill_switch_release_rate`, which has no such sibling to match.
3. **Default stays 5** — the column's SQL default and the CLI fallback both reproduce today's
   hardcoded behavior exactly; this ticket changes *where* the value lives, not what it is.
4. **CLI accepts `1..` only.** A cap of 0 would delete every unmatched row on its very first
   reconcile pass with zero retries, which is not a meaningful "small bound" per §4.4/§10 — it's
   effectively "never reconcile." Matches the existing `.range(0..)` convention on sibling flags,
   narrowed by one because 0 is nonsensical here specifically (`kill_switch_release_rate` and
   `schedule_horizon_days` both have a sensible 0).
5. **The warn is emitted from `orphan_reconcile::reconcile::run`'s existing age-out arm**, once
   per aged-out row, carrying `tenant_slug`, the orphan's `id`, its `provider_ref`, and its
   `event_type` — enough to find the lost receipt in provider logs without the ciphertext-free
   `orphan_event` row itself surviving to be inspected. `event_type` was added by T-034's review
   (2026-09-15): that ticket landed a second age-out reason (an `event_type` outside the
   documented set never reaches `find_match` at all), and without it in the warn, "no match"
   and "matched but rejected on `event_type`" are indistinguishable to whoever gets paged — see
   `event_type`'s own doc comment in `reconcile.rs` for which set is enforced. No `platform_audit`
   row: F2/F3 and this ticket's own Scope line ask only for `tracing::warn!`, matching
   `partition_lifecycle`'s `retention_skipped` precedent, which also doesn't audit.

### Tasks

#### Task 1 — migration (`migrations/tenant/0012_tenant_config_reconcile_attempts_cap.sql`)

```sql
-- Orphan-reconcile attempts cap (DESIGN.md §4.4/§10 correction note, T-033): how many
-- reconcile passes a pending orphan_event row survives before it's aged out and deleted.
-- Was a hardcoded RECONCILE_ATTEMPTS_CAP constant in src/orphan_reconcile/reconcile.rs;
-- moved here so an operator can tune it, per the design's own "config, not hardcoded"
-- correction. A fresh migration, not an edit to 0002_tenant_config.sql, matching 0010's
-- own precedent for a later-added tunable.
ALTER TABLE tenant_config ADD COLUMN reconcile_attempts_cap smallint NOT NULL DEFAULT 5;
```

#### Task 2 — model + repo (`src/tenant_config/model.rs`, `src/tenant_config/repo.rs`)

- `TenantConfig`: add `pub reconcile_attempts_cap: i16` (doc comment mirroring
  `kill_switch_release_rate`'s: names the default, notes a fresh tenant with no row falls back
  to the column's own SQL default via the consumer, not auto-seeding).
- `TenantConfigInput`: add `pub reconcile_attempts_cap: i16`; add the field to `matches()`.
- `repo::load`, `repo::load_tx`: add `reconcile_attempts_cap` to both `SELECT` column lists.
- `repo::upsert_tx`: add `reconcile_attempts_cap` to the `INSERT` column list, its `VALUES`
  placeholder, the `ON CONFLICT ... DO UPDATE SET` list, and the corresponding `.bind(...)`.

#### Task 3 — CLI (`src/bin/control.rs`)

- `TenantConfigCommand::Set`: add
  `#[arg(long = "reconcile-attempts-cap", value_parser = clap::value_parser!(i16).range(1..))]
  reconcile_attempts_cap: Option<i16>,` with a doc comment stating the default of 5 (the
  column's own SQL default), matching the `kill_switch_release_rate` arg's doc-comment
  convention immediately above it.
- The `Set` handler: add `reconcile_attempts_cap: reconcile_attempts_cap.unwrap_or(5),` to the
  constructed `TenantConfigInput`.
- The `Show` handler: append `reconcile_attempts_cap={}` to the printed line and its argument
  list.
- `tenant_config::configure::audit`: add `"reconcile_attempts_cap": input.reconcile_attempts_cap,`
  to the `platform_audit` JSON payload, next to `kill_switch_release_rate`.

#### Task 4 — fix every other `TenantConfigInput` literal (compile-breaking otherwise)

Add `reconcile_attempts_cap: 5,` (matching each site's existing `kill_switch_release_rate:
500,` line) to every other struct literal, so the crate and test suite compile:
- `tests/kill_switch.rs` (`sample_tenant_config`)
- `tests/tenant_registry.rs` (`sample_tenant_config`)
- `tests/ingest.rs` (`sample_tenant_config`)
- `tests/partition_lifecycle.rs` (`sample_input`)
- `tests/tenant_config.rs` (`sample_input`) — see Task 6 below, this one also gets a real
  assertion, not just a filler value.

#### Task 5 — cap resolution + warn (`src/orphan_reconcile/reconcile.rs`)

- Replace `pub const RECONCILE_ATTEMPTS_CAP: i16 = 5;` with a private
  `const DEFAULT_RECONCILE_ATTEMPTS_CAP: i16 = 5;`, doc-commented as matching migration 0012's
  own column default (mirrors `messgr-dispatcher`'s `DEFAULT_KILL_SWITCH_RELEASE_RATE`
  convention exactly).
- Add `use crate::tenant_config::repo as tenant_config_repo;`.
- In `run_for_tenant`, after connecting `tenant_pool` and before calling `run`, resolve the cap:
  ```rust
  let cap = tenant_config_repo::load(&tenant_pool.pool)
      .await?
      .map(|c| c.reconcile_attempts_cap)
      .unwrap_or(DEFAULT_RECONCILE_ATTEMPTS_CAP);
  ```
  (`?` converts via the existing `From<sqlx::Error> for OrphanReconcileError`.) Update the
  function's doc comment: "resolves `tenant_slug`, opens its pool, loads the reconcile-attempts
  cap from `tenant_config` (falling back to the hardcoded default for an unconfigured tenant,
  matching `messgr-dispatcher`'s `release_rate` resolution), and reconciles...".
- Thread `cap: i16` and `tenant_slug: &str` as new parameters into `run(...)`, replacing every
  use of `RECONCILE_ATTEMPTS_CAP` in its body with `cap`.
- In the `None if orphan.reconcile_attempts + 1 >= cap` arm, after `repo::record_miss`, add:
  ```rust
  tracing::warn!(
      tenant_slug,
      orphan_id = %orphan.id,
      provider_ref = %orphan.provider_ref,
      event_type = %orphan.event_type,
      "orphan_reconcile: row exceeded reconcile_attempts_cap and was deleted -- \
       the delivery receipt it held is now unrecoverable"
  );
  ```
  `event_type` distinguishes a genuine no-match from a row whose `event_type` T-034's
  `is_recognized_event_type` rejected outright (that row never reaches `find_match`, but still
  ages out through this same arm) — decision 5, amended by T-034's review.

#### Task 6 — tests

- `tests/tenant_config.rs`: extend `sample_input` to pass a real `reconcile_attempts_cap`
  (e.g. `7`, distinct from the default so a round-trip actually proves the column is read/written,
  not just present) and extend `setting_and_loading_round_trips_every_typed_field` with
  `assert_eq!(loaded.reconcile_attempts_cap, 7);`.
- `src/bin/control.rs`'s `mod tests`: add
  `tenant_config_set_rejects_a_zero_reconcile_attempts_cap`, mirroring
  `tenant_config_set_rejects_a_negative_kill_switch_release_rate` but passing
  `--reconcile-attempts-cap 0` and asserting `result.is_err()`.
- `tests/orphan_reconcile.rs`: two new `#[tokio::test]`s using the file's `TestTenant`/
  `insert_orphan_event` helpers and a new `set_reconcile_attempts_cap` helper that calls
  `messgr::tenant_config::configure::set_tenant_config` directly against the test tenant:
  - **Configured cap is honored**: set `reconcile_attempts_cap = 2` for the test tenant, insert
    an unmatched `orphan_event` row with `reconcile_attempts = 1` (one below the configured
    cap), run `reconcile::run_for_tenant`, assert `report.aged_out == 1` and the row is deleted
    — proving the tenant's configured value is what gates aging-out, not the old hardcoded 5
    (which this same row would survive under).
  - **Unconfigured tenant still ages out at the hardcoded default of 5**: no `tenant_config`
    row set; insert a row with `reconcile_attempts = 4`; assert `report.aged_out == 1` (the
    existing `no_match_at_cap_deletes_the_row` test already covers this shape but never checks
    a `tracing::warn!` fired — capture via `tracing_test` if the crate already depends on it,
    otherwise assert only the existing deletion behavior; do not add a new dev-dependency for
    this alone).

### Acceptance test

```
just build
just test    # includes the new tenant_config.rs/orphan_reconcile.rs cases and the new control.rs CLI test
just lint
just docs-check
```

All green; every pre-existing test that constructs a `TenantConfigInput` still compiles and
passes unchanged (Task 4).

### Docs update (mandatory when user-facing)

`docs/user-manual/control-plane-cli.adoc`'s "Orphan-event reconciliation" section (around the
`orphan-reconcile run` example): replace "is deleted once that counter reaches a fixed cap of
5" with "is deleted once that counter reaches `tenant_config.reconcile_attempts_cap` (default
5; set with `messgr-control tenant-config set --reconcile-attempts-cap <n> ...`), which also
emits a `tracing::warn!` naming the tenant, orphan id, and provider_ref" — following
`docs/user-manual/kill-switches.adoc:71-72`'s exact phrasing convention for documenting a
`tenant_config`-backed tunable at its point of use rather than in the CLI's own "Tenant
configuration" section (that section's field enumeration already excludes
`kill_switch_release_rate` for the same reason).

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated per above.
3. Write a summary of files touched and decisions made.
4. Suggest a Conventional Commit message, e.g.:

   ```
   feat(orphan-reconcile): make the reconcile-attempts cap configurable and alert on age-out (T-033)
   ```

5. Tidy WIP commits into a small number of atomic commits before presenting (root-path child,
   rules §0).
6. Commit locally on the ticket branch. Do not push or open a merge request without user
   approval; present the commit message and, once approved, finalize, verify the remote base
   is not behind, push, and open the merge request. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F2, F3) found the reconcile-attempts cap is hardcoded and its exhaustion path emits no alert, both contradicting DESIGN.md §4.4/§10's own correction note — batched into one follow-up ticket.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — plan amended inline: decision 5 and Task 5's `tracing::warn!` now also carry
  `event_type`, folded in from T-034's review (finding F7) — T-034 gave `orphan_reconcile` a
  second age-out reason (an unrecognized `event_type` never reaches `find_match`) that this
  ticket's warn couldn't previously distinguish from a genuine no-match.
