---
id: T-060
title: Second region: stand up and prove region-boundary isolation
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: high
cost: M
---

# T-060 — Second region: stand up and prove region-boundary isolation

## Outcome

After this ships, a second region is standing up its own independent control DB, Vault cluster,
Postgres, and binaries, with tenants pinned to one region and no cross-region dependency —
proving the region-boundary isolation claims hold at N=2 rather than being asserted at N=1.

## Description

Decision 15 (`14-decisions-and-open-questions.md`): independent control DB, Vault, Postgres,
binaries per region; tenants pinned to one region; no cross-region dependency. Everything before
this point runs multi-tenant at N=1 only — this ticket is the first proof the region-boundary
design actually holds when a second instance exists.

**Placeholder region, not final launch geography.** Design-doc still-open item #10 (launch
regions and jurisdictions) is unresolved — legal/business has not named where messgr actually
launches. Per user decision during T-054's refinement, this ticket stands up a second region as
an engineering exercise (e.g. a second same-jurisdiction region) to prove the isolation mechanics
mechanically: independent stack provisioning, no cross-region reads/writes, per-region kill
switch/Vault/control-DB independence. The exact jurisdiction and keyholder set for a real launch
region stay open and are not this ticket's concern — swapping the placeholder for a named
jurisdiction later should not require re-engineering the isolation boundary itself.

Soft coupling: `messgr-otp` (T-056) must already be deployable per-region — this ticket is what
proves that deployability at N=2, not what builds it.

**Finding from refinement: the region-boundary assertion the schema comment promises does not
exist yet.** `migrations/control/0001_control_schema.sql`'s `tenant.region` column is documented
"must match this control DB's region; asserted on boot" — but no code anywhere (`grep -rn
region src/`) reads a configured region or checks it against `tenant.region`; the column is
free text, set only by whichever value an operator happens to pass to `messgr-control provision
--region`. There is also no `REGION`/region field in `src/config.rs::Config` today. This is not
a documentation error to correct — it is exactly the gap decision 15's isolation claim rests on,
and this ticket is where it gets built, not deferred further: without it, "tenants pinned to one
region" is an operational convention, not something the system enforces.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-060-second-region-stand-up-and-prove-region-boundary-isolation
```

### Prerequisite gate (hard)

None. T-056 (`messgr-otp`) has merged (PR #66), so `otp` is in scope like every other binary.

### Confirmed design decisions (do not deviate without asking)

1. **The region is asserted once per process, at boot, against the whole control database, and
   not per tenant pool.** Only `messgr-dispatcher` opens its tenant pool at startup. `ingest`,
   `sms_sender` and `otp` open pools lazily through `TenantRegistry::get_or_open`, and `webhook`
   and `query_api` through `TenantPoolCache::get_or_open`. A per-pool check in those five would
   fire only on the first request, inside a connection task, and the process would keep serving.
   The schema comment (`migrations/control/0001_control_schema.sql:9`) already says "asserted on
   boot". One helper, `tenant::repo::assert_region(control_pool, region)`, runs
   `SELECT slug, region FROM tenant WHERE region <> $1 LIMIT 1` and panics naming both values on
   a hit, mirroring `db.rs::assert_current_database`. It is called right after the control pool
   connects in all seven mains: `ingest`, `dispatcher`, `sms_sender`, `otp`, `query_api`,
   `webhook` and `control`. The `control` call skips `migrate` and `version`, since `migrate`
   runs before the `tenant` table exists. It does cover the long-lived `messgr-control serve`
   console (T-057).
2. **Write-time guard.** `messgr-control provision` refuses a `--region` that differs from
   `MESSGR_REGION`, so a mislabelled row never enters a control database.
3. **`connect_tenant_pool`'s signature is unchanged.** With decisions 1 and 2 an
   `expected_region` parameter would be redundant, and it would have touched 31 src and 72 test
   call sites.
4. **A placeholder second region, not a named launch jurisdiction** (design-doc still-open item
   #10). Region A is `eu`, the value every test, `scripts/e2e.sh` and the existing dev data
   already use; region B is `region-b`.
5. **The proof is a second, standalone Compose stack, not new IaC** (§13).
6. **T-058 platform switches cannot cross regions by construction.** A binary holds exactly one
   `CONTROL_DATABASE_URL`, and switch reads, fan-out and `NOTIFY` all go through it. The
   isolation test pins this with two assertions (task 5) so it doesn't depend on the argument
   alone.

### Tasks

#### Task 1: `src/config.rs`, the `region` field
Add `pub region: String` from `MESSGR_REGION` (required, same `panic!` style as
`CONTROL_DATABASE_URL`). Add `MESSGR_REGION=eu` to `.env.example`, the CI `test` job env
(`.github/workflows/ci.yml`; `just control-migrate` constructs `Config`), and local `.env`.

#### Task 2: `src/tenant/repo.rs::assert_region`
Returns `Result<(), sqlx::Error>` and panics on a mismatch with a message naming the configured
region, the offending tenant's slug and its region.

#### Task 3: Wire it into the seven mains
After each `db::connect` of the control pool: `.await.expect(...)` on
`assert_region(&control_pool, &config.region)`. In `control.rs`, skip it for `Migrate` and add
the `Provision` guard (decision 2).

#### Task 4: Second Compose stack
`compose.region-b.yml` is a standalone file with top-level `name: messgr-region-b`:
`messgr-postgres-b` on `5433`, `messgr-vault-b` on `8201`, and volume `messgr-region-b-pgdata`.
Add `just db-up-region-b` and `just db-down-region-b` recipes, mirroring `db-up`/`db-down`.

#### Task 5: `tests/region_isolation.rs`
Runs against one Postgres cluster, which is all CI has. It creates two throwaway control databases
(`CREATE DATABASE` plus `sqlx::migrate!("./migrations/control")`) and inserts one tenant row in
each (`eu` in A, `region-b` in B). It then asserts:
- `assert_region(A, "eu")` and `assert_region(B, "region-b")` succeed.
- `assert_region(B, "eu")` panics with the region-mismatch message, using the `tokio::spawn` +
  `into_panic` pattern from `db.rs`'s test. This is the mutation test: it must go red if the
  assertion is removed.
- After a platform switch is engaged in A, `list_active_for_tenant(B, tenant_B)` is empty, and A's
  `notify_targets` does not include tenant B.

The test drops both databases afterwards.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including `tests/region_isolation.rs`.
3. Manual: `just db-up-region-b`, run `messgr-control migrate` then `provision --region region-b`
   against the region-B stack (`CONTROL_DATABASE_URL=…:5433/control MESSGR_REGION=region-b`), then
   start `messgr-ingest` with `MESSGR_REGION=eu` and region B's `CONTROL_DATABASE_URL`, a
   deliberate wiring mistake. Confirm it panics on startup with the region-mismatch message. Also
   confirm `provision --region region-b` with `MESSGR_REGION=eu` is refused.

### Docs update (mandatory when user-facing)

- Add a "Regions" subsection under `docs/user-manual/introduction.adoc`'s "Tenancy" section:
  `MESSGR_REGION`, the boot assertion, the provision guard, and a pointer to
  `compose.region-b.yml`.
- Update the wording of `docs/user-manual/otp-api.adoc` (~95-97, "region … not something its own
  code branches on") so it mentions the boot assertion.
- Check `development/design/12-deployment.md:7` ("Six binaries") for staleness.
- Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` and `just docs-check` clean.
2. Docs updated.
3. Write a summary (files touched, decisions, and what was deferred: real launch-region naming
   stays open per design-doc item #10) and hand back for review.
4. Suggested commit message: `feat(tenant): boot-time region assertion and a second-region
   Compose stack (T-060)`.

## Review

### Round 1 — 2026-09-28 (branch `feat/T-060-…` at `d39f1ad` after rebase onto `main`)

- [x] Reviewer independence settled (step 0): **independent** — a fresh session with no memory of writing the branch
- [x] In-tree stale-branch check (step 0a): `pickle doctor` warned the branch had T-060 in `3-in-development`; the branch was rebased onto `main` (local-only, no upstream), re-run clean apart from the unrelated payload-version warning
- [x] Implementation audit (steps 1, 2): all five tasks present in the files named. `just build`, `just lint` clean; `just test` green (every test binary `0 failed`, `region_isolation` 1 passed). Mutation check: forcing `assert_region`'s mismatch branch off turns `region_isolation` red at `tests/region_isolation.rs:111`. Manual acceptance step 3: the provision guard refuses `--region region-b` under `MESSGR_REGION=eu`, and `messgr-ingest`/`messgr-control` panic with the region-mismatch message against region B's control DB. **But `provision` against the region-B stack fails as written (F1)** and only passed after AppRole was enabled on `:8201` by hand
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4): addendum items 1–8 checked. No new table or column, `tenant.region` is `NOT NULL` (no NULL hole in `region <> $1`), no secrets from env, and the `ci.yml`/`justfile` edits are env and new recipes only (no command-parity drift). `Version` in `messgr-control` returns before `Config::from_env`, so decision 1's "skips `version`" holds. The only `tenant` insert path is `provision_tenant`, called only from `control.rs`, which is behind the guard
- [x] Documentation audit (step 4a): `just docs-check` clean; the "Regions" subsection and the `otp-api.adoc` wording are present. The Regions instruction for running region B is incomplete (part of F1)
- [x] Docs-readability pass (step 4b): skipped. No reviewer configured (`opencode` not installed), 0 suggestions discarded
- [x] Findings recorded (step 5)
- [x] Ticket moved to `tickets/5-rework/` (step 6a)
- [x] Governing documents (step 7): `03-data-model.md`'s "asserted on boot" is now true, and decision 15's row still holds. `12-deployment.md`'s "Six binaries" was already stale before this branch (F2). No DESIGN.md amendment made in this review
- [x] Impact sweep (step 8): no ticket in `1-to-do/` or `2-ready/` references T-060
- [x] Summary presented; no publish while in rework (step 9)

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | The region-B stack cannot provision a tenant. Its dev Vault never gets AppRole auth enabled, because `vault-dev-init` hardcodes `http://localhost:8200` and nothing targets `:8201`. Acceptance step 3 and `introduction.adoc`'s "Point `CONTROL_DATABASE_URL`, `VAULT_ADDR` and `MESSGR_REGION` at it" both fail at `provision`. The only way the step could have passed is with region A's Vault, which is exactly the cross-region dependency decision 15 rules out | `MESSGR_REGION=region-b messgr-control provision … ` with `VAULT_ADDR=http://localhost:8201` → `provisioning failed (vault): … status code 404`. It succeeds after `POST :8201/v1/sys/auth/approle` | Let `vault-dev-init` take the Vault address (default `http://localhost:8200`) and add a region-B call, either in `db-up-region-b` or as its own recipe. Say so in the Regions subsection, then re-run acceptance step 3 against `:8201` only |
| F2 | non-blocking | other | noted | `development/design/12-deployment.md:7` says "Six binaries" and the table leaves out `messgr-sms-sender`, while `Cargo.toml` declares seven `[[bin]]`s. The plan's docs step asked for this check, but the line has been stale since T-052 (`39271f4`), not made false by this branch | `grep -c '^\[\[bin\]\]' Cargo.toml` → 7 | Fix it on the next DESIGN.md pass (version bump) |
| F3 | non-blocking | test-gap | noted | The provision guard (decision 2) has no automated test, only manual acceptance step 3. The project has no binary-level test harness, and the guard is a single `!=` | `src/bin/control.rs:851`; no test references `does not match this deployment's MESSGR_REGION` | None now. Worth covering if `run()` is ever extracted into something testable |

Dispositions: 1 blocking (F1 → rework); 2 noted (F2, F3); 0 fixed inline, 0 folded, 0 new tickets.
cost: estimated M, actual M

### Rework fix record — round 1 (commit 4cb235c)

Branch rebased onto `main` first (local-only, no upstream) to clear `pickle doctor`'s stale-ticket
warning; the pre-fix tip is `c1ce8e6`, so the re-review diff is `git diff c1ce8e6..4cb235c`.

- **F1 — fixed.** `vault-dev-init` now takes `addr` (default `http://localhost:8200`, so CI,
  `scripts/e2e.sh` and the README call are unchanged) and uses it for every call. `db-up-region-b`
  runs `docker compose … up -d --wait`, so both health checks pass before it continues, and then
  `just vault-dev-init http://localhost:8201`. That enables AppRole on region B's own Vault and
  never touches region A's. The Regions subsection of `introduction.adoc` now says the recipe
  does this. Re-ran acceptance step 3 with only `:5433` and `:8201`, starting from
  `docker compose -f compose.region-b.yml down -v`: `migrate` and
  `provision --slug acme-b --region region-b` both succeed, returning a role id and wrapped
  secret id from `:8201`. `messgr-ingest` with `MESSGR_REGION=eu` against region B's control DB
  panics with `region mismatch: … tenant "acme-b" in region "region-b"`. Against region B, `provision --region region-b` under `MESSGR_REGION=eu` now trips the boot
  assertion before the guard runs, so the guard was checked against region A's control DB, whose
  `eu` tenants pass the boot check: the provision is refused with `does not match this
  deployment's MESSGR_REGION`, and nothing is written. Region A's control DB holds no
  `acme-b*` row. `just build`, `just lint` and `just docs-check` are clean, and `just test` is
  green: 0 test binaries report a failure, and `region_isolation` passes 1 of 1.

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #10: use a placeholder second region
  to prove isolation mechanics rather than holding this ticket for launch-jurisdiction input)
- 2026-09-22 — TO DO → READY: plan complete: found the region-boundary assertion the schema comment promises was never implemented; adds it (Config.region + connect_tenant_pool's expected_region) scoped to the six service binaries only, plus a second Compose stack as the isolation proof
- 2026-09-25 — impact sweep from T-058's review: T-058 landed platform kill switches, whose region-wide scope reaches every tenant in *one* control database — so this ticket's "per-region kill switch … independence" now has a concrete mechanism to prove (a `scope='platform'` switch engaged in region A's control DB must neither block nor `NOTIFY` a region-B tenant). Assumption still holds; plan unchanged — flagged for the implementer
- 2026-09-28 — plan amended inline: applicability gate found the six service binaries mostly open tenant pools lazily per request (TenantRegistry/TenantPoolCache), so a per-pool `expected_region` check could not fire at boot; replaced with a boot-time `assert_region` over the control DB in all seven mains, a `provision --region` guard, region A = `eu`, a standalone second Compose file, a single-cluster two-control-DB test with T-058 cross-region assertions; `connect_tenant_pool` signature unchanged; cost XL → M (user approved routing)
- 2026-09-28 — READY → IN DEVELOPMENT: picked up
- 2026-09-28 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-28 — IN REVIEW → REWORK: review round 1: 1 blocking (F1 region-B Vault never gets AppRole, provision fails), 2 noted (F2, F3)
- 2026-09-28 — REWORK → IN REVIEW: findings fixed
