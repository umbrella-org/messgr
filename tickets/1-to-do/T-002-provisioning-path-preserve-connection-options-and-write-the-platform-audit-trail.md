---
id: T-002
title: Provisioning path: preserve connection options and write the platform_audit trail
project: messgr
depends-on: []
spawned-by: [T-001]
impact: medium
complexity: low
cost: S
---

# T-002 — Provisioning path: preserve connection options and write the platform_audit trail

## Outcome

After this ships, a tenant pool connects with the same TLS and connection options as the
control pool it was derived from rather than silently dropping them, and every provisioning
run leaves a row in `platform_audit` — so the platform console (§11.4) has a trail from the
first tenant onward instead of one backfilled later.

## Description

Two gaps in the provisioning path shipped by T-001, both recorded as non-blocking findings in
that ticket's `## Review` (F5 and F6). Batched because they live in the same two files
(`src/db.rs`, `src/tenant/provision.rs`) and are one sitting of work together.

**1. Connection options are silently discarded (F5, `correctness`).** `db::with_database_name`
derives a tenant URL by string-splitting the base URL on its last `/`, so any query string is
lost: `postgres://…/control?sslmode=require` becomes `postgres://…/tenant_acme`. Today every
URL is a local-dev one with no query string, so nothing is broken — but the first staging or
production deployment would connect every tenant pool without TLS and without any other
connect option, and it would do so silently. The fix is to stop treating the URL as a string:
`base_url.parse::<PgConnectOptions>()?.database(database_name)` produces the same value with
every other option preserved, and `PgConnectOptions` is already the type
`db::connect_with_expected_database` accepts two functions away. `db::connect` (the control
pool) should be checked for the same treatment.

**2. `platform_audit` is never written (F6, `design`).** §4.11 introduces the table with the
comment "provisioning, suspension, break-glass" and §11.4 lists the audit trail as a platform
console surface. T-001 created the table and the provisioning command, and wired neither to
the other. Provisioning is currently the only auditable platform action that exists. One row
per run: actor (the invoking operator — how that identity is obtained on a CLI with no auth
realm yet is the open question this ticket must settle, and the honest interim answer may be
an explicit `--actor` flag rather than an inferred one), action `tenant.provision`, `tenant_id`,
and a `detail` payload carrying slug, region, database_name, and whether the database was
created or already existed. The row belongs on the idempotent re-run path too — a second
provision of the same tenant is an operator action worth seeing.

Soft coupling, no hard dependency: this edits code T-001 introduced, so it wants T-001 merged
first to avoid a conflict, but it encodes no assumption T-001 could invalidate. Note that
T-001's rework pass (blocking findings F1–F4) also touches `src/tenant/provision.rs`.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-21 — created (TO DO). source: review: T-001 review findings F5 (tenant URLs silently drop query-string connection options, e.g. `sslmode`) and F6 (`platform_audit` created but never written), batched by theme — both are provisioning-path completeness in `src/db.rs` / `src/tenant/provision.rs`.
