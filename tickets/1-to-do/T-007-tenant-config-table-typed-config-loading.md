---
id: T-007
title: tenant_config table + typed config loading
project: messgr
depends-on: [T-001]
spawned-by: []
impact: medium
complexity: low
cost: S
---

# T-007 — tenant_config table + typed config loading

## Outcome

After this ships, every tenant has a typed configuration record — retention window,
timezone/locale defaults, schedule horizon, verification mode, staleness bound, quota day
boundary — that later gates, the ingest path, and the dispatcher read instead of hardcoded
values.

## Description

Add the `tenant_config` table (one row per tenant, in the tenant database) plus typed config
loading on top of it: retention, timezone/locale defaults, schedule horizon, verification mode
(`enforce`/`observe`), staleness bound, and quota day boundary (design §4.10). This is the first
ticket of build step 2 (ledger, outbox, ingest, one channel, encryption from the first write —
`PLAN.md`) and the umbrella of that step's ticket family (`family: T-007`): T-008 through T-014
all belong to it. Several downstream tickets read fields this table defines — T-009's schema
work, T-012's provider selection, T-026's quiet-hours resolution — so the column set should
anticipate those without inventing fields no ticket yet needs.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; umbrella of the step-2 ticket family (T-007–T-014)
