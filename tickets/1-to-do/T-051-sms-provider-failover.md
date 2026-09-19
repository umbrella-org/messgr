---
id: T-051
title: SMS provider failover
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-051 — SMS provider failover

## Outcome

After this ships, an SMS provider outage no longer stops customer login — the dispatcher (and,
via T-052, the OTP fast path) automatically fails over to the next provider in the tenant's
ordered `provider_config` list instead of just backing off and waiting for the down provider to
recover.

## Description

Build-order step 16 (§14), explicitly sequenced "before step 17, not after" — `11-failure-modes.md`
§12.1: "Recommend revisiting genuine SMS failover before the OTP path carries production auth
traffic ... It is the one place where 'single provider' translates directly into 'customers
locked out of their bank'."

What already exists (do not rebuild): `provider_config` is an ordered list per channel from
T-012 (`(channel, priority)` primary key), hot-reloadable per §12.1's launch mitigation, and
each dispatcher already runs a per-provider circuit breaker (open after N consecutive failures,
half-open probe, §9) plus exponential backoff with jitter. What's missing, per decision 4
(`14-decisions-and-open-questions.md`, "single provider acceptable for now ... flagged as a
tier-0 risk on the OTP path") and still-open item #4 (provider selection, unresolved) — the
actual failover logic: when the active provider's circuit breaker opens, route to the next
entry in `provider_config`'s priority order for that channel, rather than only queuing and
retrying the same provider. Includes recovery behaviour (does traffic return to the
higher-priority provider once its breaker closes, and how) and alerting on a failover event
itself, since an operator should know a fallback provider is now carrying live traffic.

Out of scope: `sms-sender`'s own duplicated failover logic for the synchronous OTP path (§3 —
"OTP rate limiting and provider failover are duplicated in `sms-sender` rather than
centralized," a deliberate trade) — that is part of T-052, reusing this ticket's
`provider_config`-ordering mechanism where practical rather than inventing a second one.

Soft coupling / recommended sequencing: build-order text puts this ticket immediately before
T-052 (OTP fast path) for the reason quoted above. Not set as a hard `depends-on:` here —
flagging for your call at refinement, same as the T-048/T-049 pattern.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 16, remaining gap identified when auditing unticketed steps against the board
