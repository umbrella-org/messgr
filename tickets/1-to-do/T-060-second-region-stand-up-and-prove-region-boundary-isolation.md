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

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #10: use a placeholder second region
  to prove isolation mechanics rather than holding this ticket for launch-jurisdiction input)
