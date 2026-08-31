---
id: T-012
title: Sender trait + first SMS provider adapter + provider_config
project: messgr
depends-on: [T-007]
spawned-by: []
family: T-007
impact: high
complexity: medium
cost: M
---

# T-012 — Sender trait + first SMS provider adapter + provider_config

## Outcome

After this ships, the system can actually reach an SMS provider: a `Sender` trait abstracts the
provider call, a first concrete adapter implements it, and `provider_config` holds an ordered
provider list from day one — even at length 1 — so later multi-provider failover (T-046) has a
list to extend rather than a single hardcoded provider to refactor away.

## Description

Build the `Sender` trait, the first SMS provider adapter, and `provider_config` as an ordered
list from day one (design §4.10, §11.1 trait/mock pattern, §12.1). Part of the step-2 ticket
family (`family: T-007`; see T-007). Depends on T-007 for tenant-scoped provider configuration.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
