---
id: T-040
title: Wire scheduled delivery and expiry into the outbox
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-040 — Wire scheduled delivery and expiry into the outbox

## Outcome

After this ships, a producer's `scheduled_for` and `expires_at` on a request actually take effect:
a future-dated message sits unclaimed until its time, an expired one is written `expired` instead
of sent, and a request beyond the tenant's scheduling horizon is rejected at ingest instead of
being silently accepted and immediately claimable.

## Description

Closes build-order step 9's core mechanism (§6.2), which turns out to be schema-complete but
functionally inert. Verified directly in code: `src/ingest/repo.rs`'s outbox INSERT hardcodes
`next_attempt_at` to `now` and `expires_at` to `NULL` on every row, regardless of what the
`comms_request` row carries — so a message sent with a future `scheduled_for` is claimable
immediately, and `expires_at` is never populated for the dispatcher to check. This also means the
gate chain's own **Expiry gate — listed first in §5's table as "checked first, cheapest"** — is
dead code today: `src/dispatcher/drain.rs` already has the `expires_at <= now` check written and
correct, but it can never fire because the column it reads is always `NULL`.

Also unenforced: `tenant_config.schedule_horizon_days` (default 90, T-007) is stored and
shown by `messgr-control tenant-config show`, but nothing reads it at ingest — a request can
schedule arbitrarily far out today with no rejection.

Per §6.2: "All gates run at dispatch, never at schedule time" (already true structurally, this
ticket doesn't touch the gate chain itself) and "Requests beyond the horizon are rejected unless
the producer holds an explicit override" — no override mechanism exists yet; if this ticket's
refinement finds the override is needed for launch, split it out rather than growing this ticket,
since horizon rejection with no override is still a correct, shippable state (nobody has an
override to lose).

**Explicitly out of scope:** `scheduled_local` (customer-local-time scheduling, §6.2's second
form) — only `scheduled_for` (absolute UTC) exists in the schema today; local-time scheduling
needs the same tz machinery as quiet hours (T-043) and should follow it, not duplicate it. Quiet
hours precedence (§6.3: quiet hours wins over a scheduled time) is also out of scope until T-043
exists — note the coupling in this ticket's Implementation Plan when refined.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — DESIGN.md §5's Expiry gate (checked first in the gate chain, alongside T-036-T-038's
  verification/consent/suppression gates) cannot fire until this ships, since `outbox.expires_at`
  is hardcoded NULL at ingest today.
