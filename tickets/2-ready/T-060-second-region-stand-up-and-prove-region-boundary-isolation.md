---
id: T-060
title: Second region: stand up and prove region-boundary isolation
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: high
cost: XL
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

None hard. Soft: if T-056 (`messgr-otp`) has not merged yet, its binary is simply left out of the
second-region compose stack and the per-binary region check (task 3) — added when T-056 lands,
not blocking this ticket.

### Confirmed design decisions (do not deviate without asking)

1. **The region assertion is added only where a long-lived service binary resolves its own
   tenant identity at startup — not at every `connect_tenant_pool` call site.** Eighteen files
   call it today (`grep -rln connect_tenant_pool src/`), most of them one-shot `messgr-control`
   admin operations (configure/register/etc.) run interactively against whichever control
   database the operator already pointed at — adding a hard region check there is disproportionate
   scope for what this ticket needs to prove. The six service binaries
   (`ingest`/`dispatcher`/`sms_sender`/`otp` once T-056 lands/`query_api`/`webhook`) are the ones
   with a persistent, deploy-time-configured identity that could be pointed at the wrong region's
   tenant by a wiring mistake — that is the failure mode decision 15 actually needs caught.
2. **`connect_tenant_pool` gains an `expected_region: Option<&str>` parameter, `None` for every
   existing (admin/CLI) call site, `Some(&config.region)` only from the six service binaries.**
   Mirrors the existing `expected_db`/`current_database()` assertion in shape
   (`src/db.rs::connect_with_expected_database`) — same "panic on mismatch, loudly, at connect
   time" precedent, not a silent log-and-continue.
3. **A placeholder second region, not a named launch jurisdiction** (Description, design-doc
   still-open item #10) — `region-a`/`region-b` string values are sufficient; do not block this
   ticket on legal/business naming real regions.
4. **Proof is a second Compose stack, not new IaC.** §13: "Run under systemd or Docker Compose...
   Kubernetes only if the operator already runs it for other workloads." This project has no
   Terraform/K8s manifests today (`compose.yml` is the only deployment artifact) — a second,
   parameterized Compose file is the proportionate proof, not a new deployment technology.

### Tasks

#### Task 1 — `src/config.rs`: `region` field
Add `pub region: String`, loaded from `MESSGR_REGION` (required, same `panic!` style as
`CONTROL_DATABASE_URL`).

#### Task 2 — `src/tenant/pool.rs`: region assertion
Add `expected_region: Option<&str>` to `connect_tenant_pool`'s signature. When `Some(region)`,
after resolving the `tenant` row (already fetched for the existing `tenant_id` lookup), assert
`tenant.region == region` or panic with a message naming both values (mirror
`assert_current_database`'s panic message shape, `src/db.rs:54`). Update every existing call
site to pass `None` (compiler-guided — eighteen files per the grep above; this is a mechanical
fixup, not a design decision per call site).

#### Task 3 — Wire `Some(&config.region)` from the six service binaries
`src/bin/ingest.rs`, `src/bin/dispatcher.rs`, `src/bin/sms_sender.rs`, `src/bin/otp.rs` (once
T-056 lands — otherwise skip, soft coupling), `src/bin/query_api.rs`, `src/webhook/mod.rs`
(wherever it calls `connect_tenant_pool` per-request) — each passes `Some(&config.region)` at its
own tenant-pool-opening call site(s).

#### Task 4 — Second Compose stack
`compose.region-b.yml` (a Compose override/second file, distinct container names/ports/volumes:
`messgr-postgres-b`/`5433`, `messgr-vault-b`/`8201`, distinct volume names) — a full second,
independent Postgres + dev-mode Vault, standing in for a second region's control DB/Vault
cluster. Add a `just up-region-b` / `just down-region-b` recipe pair mirroring whatever recipes
already bring up the default stack.

#### Task 5 — Integration test proving isolation
A new test (`tests/region_isolation.rs` or similar) that: provisions a tenant against region A's
control DB with `region = "region-a"`, provisions a second tenant against region B's control DB
with `region = "region-b"`, then attempts to open a tenant pool for the region-B tenant while
passing `expected_region = Some("region-a")` and asserts it panics (mutation test: this must go
red if task 2's assertion is removed). Also confirms the ordinary case — resolving each tenant
against its own region — succeeds without panicking.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including task 5's isolation test.
3. Manual: `just up-region-b`, provision a tenant against each stack
   (`CONTROL_DATABASE_URL`/`VAULT_ADDR` pointed at each in turn), start `messgr-ingest` for
   region A pointed at region B's tenant database by mistake (a deliberately broken env var
   combination) and confirm it panics on startup with the region-mismatch message rather than
   silently serving traffic.

### Docs update (mandatory when user-facing)

Add a "Regions" subsection under `docs/user-manual/introduction.adoc`'s existing "Tenancy"
section: `MESSGR_REGION`, what the assertion catches, and a pointer to `compose.region-b.yml` as
the local second-region proof stack. Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated and registered.
3. Write a summary (files touched — note the eighteen mechanical `None`-passing call sites as a
   single bucket rather than one-by-one — decisions made, anything deferred: real launch-region
   naming stays open per design-doc item #10) and hand back for review.
4. Suggested commit message: `feat(tenant): second region — region-boundary assertion and a
   second Compose stack (T-060)`.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #10: use a placeholder second region
  to prove isolation mechanics rather than holding this ticket for launch-jurisdiction input)
- 2026-09-22 — TO DO → READY: plan complete: found the region-boundary assertion the schema comment promises was never implemented; adds it (Config.region + connect_tenant_pool's expected_region) scoped to the six service binaries only, plus a second Compose stack as the isolation proof
- 2026-09-25 — impact sweep from T-058's review: T-058 landed platform kill switches, whose region-wide scope reaches every tenant in *one* control database — so this ticket's "per-region kill switch … independence" now has a concrete mechanism to prove (a `scope='platform'` switch engaged in region A's control DB must neither block nor `NOTIFY` a region-B tenant). Assumption still holds; plan unchanged — flagged for the implementer
