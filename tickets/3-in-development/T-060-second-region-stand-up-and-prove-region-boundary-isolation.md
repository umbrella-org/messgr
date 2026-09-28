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

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #10: use a placeholder second region
  to prove isolation mechanics rather than holding this ticket for launch-jurisdiction input)
- 2026-09-22 — TO DO → READY: plan complete: found the region-boundary assertion the schema comment promises was never implemented; adds it (Config.region + connect_tenant_pool's expected_region) scoped to the six service binaries only, plus a second Compose stack as the isolation proof
- 2026-09-25 — impact sweep from T-058's review: T-058 landed platform kill switches, whose region-wide scope reaches every tenant in *one* control database — so this ticket's "per-region kill switch … independence" now has a concrete mechanism to prove (a `scope='platform'` switch engaged in region A's control DB must neither block nor `NOTIFY` a region-B tenant). Assumption still holds; plan unchanged — flagged for the implementer
- 2026-09-28 — plan amended inline: applicability gate found the six service binaries mostly open tenant pools lazily per request (TenantRegistry/TenantPoolCache), so a per-pool `expected_region` check could not fire at boot; replaced with a boot-time `assert_region` over the control DB in all seven mains, a `provision --region` guard, region A = `eu`, a standalone second Compose file, a single-cluster two-control-DB test with T-058 cross-region assertions; `connect_tenant_pool` signature unchanged; cost XL → M (user approved routing)
- 2026-09-28 — READY → IN DEVELOPMENT: picked up
