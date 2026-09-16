---
id: T-037
title: Consent gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium-high
cost: M-L
---

# T-037 — Consent gate at dispatch

## Outcome

After this ships, marketing no longer sends to an address without a recorded opt-in: the
dispatcher checks consent for (`address_id`, `class`) at send time, blocks unconsented marketing
with a terminal `suppressed_consent` event, and an operator has a way to record an opt-in/opt-out
(no automated consent feed exists yet — see scope note below).

## Description

Second of the three regulatory gates from DESIGN.md §5 (build-order step 5; see T-036 for the
family context). Unlike verification, **no schema exists for this yet** — needs a new `consent`
table keyed on `(address_id, class)` per AGENTS.md hard invariant 4 (keys on `customer_address.id`,
never `customer_id` or the raw address value, so a recycled number's new address row starts with
no consent record — absence of consent defaults to opted-out for marketing, by design, not a bug
to fix). Transactional does not require opt-in (§5); auth skips this gate entirely (AGENTS.md
invariant 1).

**Scope note — the design's real consent source doesn't exist yet.** §5 says the master system
normally publishes consent via the customer event feed, but that feed consumer is still a stub
(build-order step 10, unbuilt). Building this gate against a data source that doesn't exist yet
would ship a gate nothing can ever satisfy. This ticket therefore also needs a minimal, explicit
way to write consent records — most likely a `messgr-control consent set` subcommand, mirroring
the existing `tenant-config set` / `provider-config set` CLI pattern — scoped only to manual/CLI
consent recording. Wiring the real event-feed consent source is out of scope here and belongs to
whatever ticket eventually builds step 10; note that coupling explicitly when this is refined.
§5 also mentions a **consent pre-filter at bulk-campaign ingestion** — that depends on the bulk
campaign path (step 18, unbuilt) and is out of scope for this ticket; the dispatch-time gate is
authoritative regardless.

Runs after verification (T-036), before suppression (T-038), per §5's ordering table. No hard
dependency on T-036/T-038 — each gate is independently observable and buildable; WIP=1 serializes
pickup anyway. **Note for refinement:** T-038 landed first (its own Description says the same —
no hard dependency, pickup order decides), so `try_process` already has the suppression check as
its very first statement, before decrypt. Whoever refines this ticket decides where the consent
check goes relative to it — §5's prose ordering is not enforced in code, and nothing requires
restoring it.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed alongside T-036/T-038 from a
  build-order-vs-shipped-tickets gap analysis — step 5 (the gate chain) is unbuilt despite steps
  0-4 and 17 later hardening tickets being done.
- 2026-09-16 — Description amended (T-038 review, impact sweep): T-038 shipped first, so the
  suppression check is already `try_process`'s first statement — this ticket's own consent-check
  placement relative to it is now this ticket's own call at refinement, not §5's prose ordering.
