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

**Reviewer independence (step 0):** delegated. The orchestrating reviewer authored this branch
in this same session, so steps 2-4a were run by a fresh sub-agent with no memory of writing the
code, briefed adversarially against the ticket, `AGENTS.md`'s hard invariants, `DESIGN.md`
§4.4/§10, and this project's review addendum. Every delegated finding below was re-verified by
hand against the actual diff/files before being recorded (protocol step 0: "delegation buys
independence, not accuracy").

**Step 2 (Implementation audit):** all tasks met. Acceptance test re-run verbatim on the branch:
`just build`, `just test` (every suite green, including the new `orphan_reconcile.rs`/
`tenant_config.rs`/`control.rs` cases), `just lint`, `just docs-check` — all clean. Migration
0012 confirmed as the next-free number and shaped like 0010's precedent. Plumbing parity with
`kill_switch_release_rate` verified site by site across `model.rs`, `repo.rs`, `configure.rs`,
`control.rs`. All confirmed decisions honoured, including the `event_type` amendment. Addendum
Step 2 items 1-8 checked explicitly; none triggered (no NULL-distinctness hole, no lease/lock,
no secrets, no new PII table, new column has a reader, no hard-invariant touch, no
justfile/workflow diff).

**Step 3 (Quality audit):** mutation-tested `configured_reconcile_attempts_cap_is_honored` by
hand — reverting the cap resolution to the hardcoded default flips `report.aged_out` from 1 to 0
and the assertion goes red, confirming it is not vacuous. No security or error-handling defects
found beyond F1/F2/F6 below.

**Step 4 (Consistency audit):** no caller/callee contract drift — `run(...)`'s new params have
one call site, updated; `run_for_tenant`'s public signature is unchanged so no test call site
needed edits. Project-wide grep confirms zero surviving references to the old
`RECONCILE_ATTEMPTS_CAP` name. F3 is the one consistency defect found (a governing document, not
this branch's own code).

**Step 4a (Documentation audit):** coverage present at the right place (mirrors
`kill-switches.adoc`'s point-of-use convention); `just docs-check` clean; whole-tree sweep found
F4 and F5.

**Step 4b (Docs-readability pass):** conscious skip — no docs-readability reviewer configured in
this host environment.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | plan-wrong | noted | Decision 4's rationale for `range(1..)` doesn't hold: the age-out predicate is `orphan.reconcile_attempts + 1 >= cap`, so `cap = 1` ages out a fresh row (`reconcile_attempts = 0`) on its very first pass with zero retries — behaviourally identical to the `cap = 0` the decision banned. | `src/bin/control.rs` (`.range(1..)`) vs `src/orphan_reconcile/reconcile.rs:208` | If "at least one retry" is the intent, the bound should be `range(2..)`; otherwise amend decision 4's stated reason. |
| F2 | non-blocking | design | noted | `--reconcile-attempts-cap` has no upper bound (`range(1..)`, i16); a very large cap reinstates the "permanent plaintext-PII table" DESIGN.md §4.4's correction note exists to prevent, and the value is settable well past what "a small bound" means. | `src/bin/control.rs`; `development/design/03-data-model.md` §4.4/§10 correction note | Bound the range (e.g. `range(2..=100)`) in a future pass. |
| F3 | non-blocking | stale-xref | fixed inline | `DESIGN.md` §4.10's canonical `tenant_config` DDL and its "T-007 ships only ... six fields" prose predated `kill_switch_release_rate` (T-016) and now also `reconcile_attempts_cap` (T-033); this branch made the drift worse. | `development/design/03-data-model.md` §4.10 (pre-fix) | Fixed in this review: both columns added to the DDL block and the prose corrected; `DESIGN.md`'s version stamp bumped to 6. |
| F4 | non-blocking | docs-gap | fixed inline | The CLI manual said the age-out warn names "the tenant, orphan id, and provider_ref", omitting `event_type` — the field decision 5 (amended 2026-09-15 from T-034's F7) added specifically so a paged operator can distinguish a genuine no-match from an `event_type` rejection. The docs task text predated that amendment and was transcribed unchanged. | `docs/user-manual/control-plane-cli.adoc` (pre-fix) vs `src/orphan_reconcile/reconcile.rs:210-217` | Fixed in this review: `event_type` added to the sentence. |
| F5 | non-blocking | docs-gap | fixed inline | `control-plane-cli.adoc`'s "Tenant configuration" section still said `tenant_config` was scoped to "the six fields above" and showed neither tunable flag in its example — stale since T-016, worsened by this ticket adding an eighth field. | `docs/user-manual/control-plane-cli.adoc` "Tenant configuration" section (pre-fix) | Fixed in this review: prose now points to each tunable's own point-of-use documentation, following the same convention the ticket's own docs task used. |
| F6 | non-blocking | test-gap | new ticket (T-035) | Nothing asserts the age-out `tracing::warn!` actually fires — the ticket's headline Outcome has zero coverage. The plan's stated reason for skipping this (needing the `tracing_test` crate) doesn't hold: `tracing-subscriber` is already a direct dependency and can capture the event with no new dependency. Separately, `unconfigured_tenant_still_ages_out_at_the_hardcoded_default` is near-duplicate of the pre-existing `no_match_at_cap_deletes_the_row`. | `Cargo.toml`; `tests/orphan_reconcile.rs` | Filed as T-035 (`spawned-by: T-033`), batched as a single follow-up. |

**Disposition summary:** fixed inline — F3, F4, F5; noted — F1, F2; new ticket — F6 (T-035).

cost: estimated S, actual S

**Governing-document reconciliation (step 7):** `DESIGN.md` §4.10 amended (F3) and its version
stamp bumped to 6. Filing T-035 collided with a ticket-number placeholder `DESIGN.md` and the CLI
manual had been using for a not-yet-filed OIDC ticket (`development/design/03-data-model.md`
§4.10, `docs/user-manual/control-plane-cli.adoc`); both forward-references were corrected in the
same review to stop naming a number that now belongs to something else. `tickets/6-done/T-007-...`
keeps its original "T-035" mention as-is — a done ticket's own historical prose, not a live
governing document.

**Impact sweep (step 8):** no ticket in `1-to-do/` or `2-ready/` references T-033 in `depends-on:`
or Description. No corrections needed.

## History

- 2026-09-15 — created (TO DO). source: review: T-030's review (findings F2, F3) found the reconcile-attempts cap is hardcoded and its exhaustion path emits no alert, both contradicting DESIGN.md §4.4/§10's own correction note — batched into one follow-up ticket.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — plan amended inline: decision 5 and Task 5's `tracing::warn!` now also carry
  `event_type`, folded in from T-034's review (finding F7) — T-034 gave `orphan_reconcile` a
  second age-out reason (an unrecognized `event_type` never reaches `find_match`) that this
  ticket's warn couldn't previously distinguish from a genuine no-match.
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
- 2026-09-15 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-15 — IN REVIEW → DONE: review verdict: no blocking findings; 3 fixed inline (F3-F5), 2 noted (F1-F2), 1 spawned as T-035 (F6)
- 2026-09-15 — MR opened: PR #50 (`feat/T-033-orphan-reconcile-cap-config`) against `main`, pending human merge.
- 2026-09-15 — MERGED: PR #50 merged to `main` (`06ef25b`).
