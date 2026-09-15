---
id: T-042
title: Producer quotas and producer_usage enforcement
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-042 — Producer quotas and producer_usage enforcement

## Outcome

After this ships, a producer's marketing traffic that exceeds its configured per-minute or
per-day limit defers to the next window instead of spending unlimited budget; a transactional
producer over its limit still sends, loudly alerted; auth traffic is counted but never blocked.

## Description

Closes build-order step 6 (§5.1) — currently entirely unbuilt: no `producer_quota` or
`producer_usage` table exists anywhere in `migrations/`, no in-process counter, no enforcement
path. Only `tenant_config.quota_day_boundary_tz` exists (a config field with nothing reading it).

Per §5.1, this needs, precisely (do not build more than this without asking — §5.1 is unusually
prescriptive about what *not* to build):

1. **Two distinct limits, not one** — admission rate (ingest API, protects messgr's own DB,
   `429` immediate/retryable) vs. send quota (dispatcher, protects customers/provider spend,
   defer-or-alert per mode). Conflating them is explicitly called out as the usual mistake.
2. **Charged at dispatch, not ingest** — a message scheduled three weeks out consumes the quota
   of the day it sends, not the day it was submitted.
3. **In-process counter, not a hot DB row** — a plain in-memory map behind the single dispatcher
   enforcer per tenant (no coordination/contention, relies on T-039's single-active-dispatcher
   guarantee), flushed to `producer_usage` every few seconds for reporting, rebuilt from that
   table on dispatcher startup so a restart doesn't zero a producer's daily allowance.
4. **Fixed windows** (per-minute burst + per-day total resetting at `quota_day_boundary_tz`
   local midnight), not rolling — explicitly rejected as "fairer and harder to explain; not
   worth it here."
5. **Enforcement mode per (producer, channel, class)**, with these exact defaults (AGENTS.md
   invariant 5 is the `transactional`/`auth` rows — do not make quota able to block either):
   `marketing` → hard/defer; `transactional` → soft/send-and-alert; `auth` → exempt but counted.
6. **`producer_quota_override`** for time-boxed, approved, self-expiring campaign-day uplift.

**Explicitly cut, already recorded in DESIGN.md — do not reintroduce:** a `campaign_stats`
rollup and a marketing share-of-budget throttle (AGENTS.md "Prefer cutting to adding" names both
as removed for being unjustified).

Soft coupling: depends conceptually on T-039 (single active dispatcher) for the in-process
counter to be safe without coordination — not a hard `depends-on`, since quota logic can be built
and tested before T-039 lands, but the "no coordination needed" property only holds once T-039 is
real. Flag this explicitly if picked up before T-039 is done.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: pickle ticket new
