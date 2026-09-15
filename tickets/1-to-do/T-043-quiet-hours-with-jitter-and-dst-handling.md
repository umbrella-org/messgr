---
id: T-043
title: Quiet hours with jitter and DST handling
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-043 — Quiet hours with jitter and DST handling

## Outcome

After this ships, marketing (and transactional) messages no longer dispatch during a customer's
configured quiet hours — a message due inside the window reschedules to window-end plus jitter
instead of sending or silently piling up; auth is unaffected.

## Description

Closes build-order step 8 (§6.1) — currently schema-only: `customer.timezone` exists (migration
0008, comment: "quiet hours + local-time scheduling") but no policy table, no window-resolution
logic, and no gate step exists anywhere in the dispatcher.

Per §6.1, exactly three failure modes the naive version hits, all must be handled:

1. **Thundering herd** — reschedule to `quiet_end + random_jitter(0, 30min)`, **plain uniform
   jitter only**. An earlier design draft weighted it (transactional early in the window,
   marketing late) and that was cut as over-engineering — "a knob nobody will tune and which
   duplicates the `ORDER BY priority` preemption already in the claim query (§4.2)." Do not
   reintroduce weighted jitter (AGENTS.md "Prefer cutting to adding" names this cut explicitly).
2. **Unknown timezone** — falls back to an explicit institution default (configured per
   deployment), never to server-local time, never to "send anyway."
3. **DST** — store everything UTC, resolve windows with a real tz database (`chrono-tz`, per
   §6.1 — not yet a dependency; `chrono` itself is already in `Cargo.toml` but not the `-tz`
   crate, so this ticket adds it), and on a non-existent local time (spring-forward gap), round
   forward to the next valid instant.

Resolution order: `customer tz -> segment policy -> institution default`. Auth class skips this
gate entirely (§3, same exemption pattern as verification/consent in T-036/T-037).

**Explicitly out of scope for this ticket** (§6.3's precedence table, which needs T-040 to exist
first): quiet-hours-vs-scheduled-time precedence ("quiet hours wins"), quiet-hours-vs-expiry
interaction ("expires before window end → dropped"), and `scheduled_local` (customer-local-time
scheduling, shares this ticket's tz machinery but is T-040's API surface, not this ticket's).
Note these couplings in the Implementation Plan when refined; building precedence logic against
gates that don't exist yet would be untestable.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — last of the next-batch-of-5 (T-039-T-043), lowest urgency since it's schema-only
  today rather than a live gap in an already-claimed control.
