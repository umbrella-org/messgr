---
id: T-010
title: Template store: immutable versioned templates, approval metadata, render path
project: messgr
depends-on: [T-009]
spawned-by: []
family: T-007
impact: medium
complexity: medium
cost: M
---

# T-010 — Template store: immutable versioned templates, approval metadata, render path

## Outcome

After this ships, a producer sends against an approved, versioned template rather than raw
freeform content: the template store holds immutable `(template_id, version, locale)` rows with
approval metadata, and every ledger row pins the exact version it rendered.

## Description

Build the template store per design §4.4: immutable `(template_id, version, locale)` rows,
approval metadata, a render path, and version pinning onto the ledger row created in T-009.
Part of the step-2 ticket family (`family: T-007`; see T-007). Depends on T-009 for the
`comms_request` column that records the pinned template version.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
