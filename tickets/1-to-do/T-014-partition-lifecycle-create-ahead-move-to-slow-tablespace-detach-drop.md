---
id: T-014
title: Partition lifecycle: create-ahead, move to slow tablespace, detach + drop
project: messgr
depends-on: [T-009]
spawned-by: []
family: T-007
impact: medium
complexity: medium
cost: M
---

# T-014 — Partition lifecycle: create-ahead, move to slow tablespace, detach + drop

## Outcome

After this ships, `comms_request` partitions manage themselves: a create-ahead job keeps future
partitions ready, partitions older than 18 months move to a slower (still writable) tablespace,
and partitions past the tenant's retention boundary are detached and dropped automatically —
nobody manually manages ledger partitions.

## Description

Build the partition lifecycle per design §4.1, §7.2, §7.5: a create-ahead job, an 18-month move
to a slower (still writable) tablespace, and detach + drop at the tenant's retention boundary
(read from `tenant_config`, T-007). Part of the step-2 ticket family (`family: T-007`; see
T-007). Depends on T-009 for the partitioned `comms_request` table this operates on.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
