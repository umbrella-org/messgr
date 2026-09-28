---
id: T-061
title: Region boundary hardening: control-DB and Vault region identity
project: messgr
depends-on: []
spawned-by: [T-060]
impact: medium
complexity: medium
cost: M
---

# T-061 — Region boundary hardening: control-DB and Vault region identity

## Outcome

A binary wired to another region's control database or Vault refuses to start, even when that
control database has no tenants yet. A region-A process can no longer write a tenant row into region
B's empty control database, or region-B keys into region A's Vault.

## Description

T-060 enforces decision 15 by checking that every tenant row in the control database carries the
process's `MESSGR_REGION` (`tenant::repo::assert_region`). A post-merge `/code-review` of PR 73
found two gaps that check cannot close:

1. **An empty control database has no region.** `assert_region` only looks for tenant rows in the
   wrong region, so a freshly migrated control DB passes for any `MESSGR_REGION`. If an operator
   sets `MESSGR_REGION=eu` but points `CONTROL_DATABASE_URL` at region B's empty control DB, the
   boot check and the `provision --region` guard both pass. An `eu` tenant then lands in region B,
   and every region-B binary refuses to boot afterwards. The fix needs the control DB to record
   its own region once (for example a singleton row written by `migrate` or the first
   `provision`) and to assert against that. This is a schema change, so §4 and §7.2 need
   checking.
2. **Vault is never checked.** With region B's control DB and region A's `VAULT_ADDR`, both checks
   pass and `provision` creates region B's Transit mount, key, AppRole and pepper in region A's
   Vault, so region-B customer DEKs are wrapped by a region-A key. This is the same wiring mistake
   T-060's round-1 F1 hit, and it is what decision 15 rules out. It needs a region marker in Vault
   that each binary checks at boot, alongside the control-DB check.

Soft coupling: T-060 (`assert_region`, `refuse_foreign_region`). Open question for refinement:
where the Vault region marker lives, and whether `messgr-otp`'s Vault use needs the same check.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-28 — created (TO DO). source: pickle ticket new
