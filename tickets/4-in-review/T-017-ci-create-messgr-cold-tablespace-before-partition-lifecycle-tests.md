---
id: T-017
title: CI: create messgr_cold tablespace before partition-lifecycle tests
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: low
cost: S
---

# T-017 — CI: create messgr_cold tablespace before partition-lifecycle tests

## Outcome

`main`'s CI `test` job goes green again: `tests/partition_lifecycle.rs`'s three tablespace-move
tests stop failing with `tablespace "messgr_cold" does not exist`.

## Description

T-014 added `tests/partition_lifecycle.rs`, which exercises `move_to_tablespace` against a real
`messgr_cold` tablespace — created locally by the `just tablespace-init` recipe T-014 also added.
`.github/workflows/ci.yml`'s `test` job never runs an equivalent step, so every push to `main`
and every PR since T-014 merged (PR #16, `10f6a88`) has had its `test` job fail on
`a_partition_inside_the_retention_window_moves_but_is_not_dropped`,
`a_partition_past_the_retention_boundary_is_detached_and_dropped`, and
`no_tenant_config_means_no_drop_ever` — all three `PgDatabaseError` code `42704`, "tablespace
\"messgr_cold\" does not exist". Confirmed against three real run failures: runs `33530977868`
(PR #16 itself, before merge — masked at the time because the PR's own merge gate apparently
didn't block on it), `33531032733`, and `33531095818` (both direct pushes to `main` after the
merge).

`ci.yml`'s `postgres` service is a plain `postgres:18-alpine` service container, not the
`messgr-postgres` named container `just tablespace-init` assumes — the fix has to reach it via
GitHub Actions' `job.services.postgres.id` context instead of a hardcoded container name.

Soft coupling: T-014 (already `6-done/`) is the ticket that should have caught this — its own
Acceptance test only ran `just test` locally, where `tablespace-init` had already been run by a
prior step in the same justfile group. Not reopening T-014; this ships as its own ticket since
the fix is entirely in `.github/workflows/ci.yml`, nothing T-014 touched.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-017-ci-messgr-cold-tablespace
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged, then interactive-rebased
into atomic commits before presenting (rules §0). Do not push and do not open a merge request
without explicit user approval. Ticket and board bookkeeping is committed on `main`, never on
this branch.

### Prerequisite gate (hard)

None — `.github/workflows/ci.yml` and `justfile` already exist; this only edits the former.

### Confirmed design decisions (do not deviate without asking)

1. **Reach the `postgres` service container via `${{ job.services.postgres.id }}`, not a
   hardcoded name.** `just tablespace-init` assumes a container literally named
   `messgr-postgres` (its own `docker exec messgr-postgres ...`), which is what `docker compose`
   names it locally — GitHub Actions' `services:` containers get generated names instead. The
   `job.services.<service_id>.id` context expression is the documented way to get a service
   container's id from within a step.
2. **New step placed after the `postgres`/`vault` services are declared healthy but before
   `cargo test`** — same constraint `tablespace-init` has locally (Task 5 of T-014's plan: "the
   integration tests provision real tenants and need the `messgr_cold` tablespace to exist").
   Placed immediately after `cargo run --bin messgr-control -- migrate`, since tablespace
   creation is independent of both the app build and the Vault bootstrap step and doesn't need
   to block either.
3. **Mirrors `just tablespace-init` exactly** (mkdir, chown, `CREATE TABLESPACE ... LOCATION
   '/var/lib/postgresql/tablespaces/messgr_cold'`) rather than inventing a different mechanism —
   one tablespace-creation recipe to keep in sync, not two. Unlike the local recipe, this step
   does **not** append `|| true`: CI runs this exactly once per job, on a container that never
   already has the tablespace, so a failure here should fail the build loudly rather than being
   silently swallowed the way repeated local `just db-up`/`tablespace-init` invocations need it
   to be.

### Tasks

#### Task 1 — Add the CI step

In `.github/workflows/ci.yml`, in the `test` job, insert a new step immediately after `- run:
cargo run --bin messgr-control -- migrate` and before the `Bootstrap Vault Transit fixture
mounts...` step:

```yaml
      - name: Create messgr_cold tablespace (T-014's partition_lifecycle tests need it)
        run: |
          docker exec ${{ job.services.postgres.id }} mkdir -p /var/lib/postgresql/tablespaces/messgr_cold
          docker exec ${{ job.services.postgres.id }} chown postgres:postgres /var/lib/postgresql/tablespaces/messgr_cold
          docker exec ${{ job.services.postgres.id }} psql -U messgr -d control -c \
            "CREATE TABLESPACE messgr_cold LOCATION '/var/lib/postgresql/tablespaces/messgr_cold'"
```

#### Task 2 — No `justfile`/local-dev change

Confirm (do not edit) that `just tablespace-init` and the local dev flow are untouched — the local
recipe already works; only CI was missing the equivalent step.

### Acceptance test

Cannot be fully verified locally (there is no local GitHub Actions runner in this environment),
so the acceptance test is the real thing:

1. `cargo fmt --all -- --check` and `cargo clippy --all-targets --all-features -- -D warnings`
   clean locally (the YAML change touches no Rust, so both should be unaffected — run anyway as
   a sanity check).
2. Push the branch, open the PR, and confirm the `test` job's new step succeeds and
   `tests/partition_lifecycle.rs`'s three previously-failing tests
   (`a_partition_inside_the_retention_window_moves_but_is_not_dropped`,
   `a_partition_past_the_retention_boundary_is_detached_and_dropped`,
   `no_tenant_config_means_no_drop_ever`) now pass, along with everything else in `cargo test`.
3. Re-run (or push a trivial follow-up commit) to confirm the fix holds on a second fresh
   `postgres` service container, not just the first one that happened to work.

### Docs update (mandatory when user-facing)

No user-facing surface — CI-internal only. No README/DESIGN.md change: this doesn't alter any
documented behavior, only makes CI actually cover what T-014's own docs already describe.

### Finish (mandatory)

1. Acceptance test green (`fmt`/`clippy` locally; the real CI run is the actual proof and can
   only be confirmed once pushed).
2. No docs to update.
3. Write a summary: file touched, decision honoured.
4. Suggested Conventional Commit message:

   ```
   ci: create messgr_cold tablespace before running tests (T-017)

   T-014's partition_lifecycle tests need the messgr_cold tablespace to
   exist, created locally by `just tablespace-init`. CI never ran an
   equivalent step, so every push to main since T-014 merged has had
   three tests fail with "tablespace \"messgr_cold\" does not exist".
   ```

5. Root-path child: single atomic commit is enough here (one file, one change) — no rebase
   needed, but check for stray WIP commits anyway before presenting.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the commit (root-path default), verify `git fetch origin main
   && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints nothing (in-tree layout,
   rules §0), then push and open the merge request. Merging is the human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-01 — created (TO DO). source: chat: user reported three failed GitHub Actions runs (33530977868, 33531032733, 33531095818); root-caused to T-014's tablespace-init step missing from ci.yml
- 2026-09-01 — TO DO → READY: plan complete
- 2026-09-01 — READY → IN DEVELOPMENT: picked up
- 2026-09-01 — IN DEVELOPMENT → IN REVIEW: acceptance green (fmt/clippy clean, YAML valid; real CI run pending push)
