---
id: T-059
title: Tenant offboarding: terminate-and-destroy mode
project: messgr
depends-on: []
spawned-by: [T-054]
impact: critical
complexity: medium
cost: M
---

# T-059 — Tenant offboarding: terminate-and-destroy mode

## Outcome

After this ships, a departing tenant can be fully offboarded via terminate-and-destroy: its
Transit key destroyed, its database dropped, and its AppRoles revoked — O(1), minutes, immediate
unreadability subject only to the §7.3 backup window.

## Description

§7.7: `tenant.status` already carries `offboarding_archive` / `offboarding_destroy` in the schema
(T-001, done), but no tooling enforces either mode yet. This ticket builds **terminate and
destroy** only: destroy the Transit key, `DROP DATABASE`, revoke AppRoles.

**Terminate-and-archive is explicitly out of scope for this ticket.** §7.7 states it "needs
pricing rather than engineering" — design-doc still-open item #13 (is archive commercially
offered, and at what price? it carries multi-year key-custody obligations after the relationship
ends) is unresolved, and per user decision during T-054's refinement this ticket does not assume
an answer. Archive mode (database and Transit key retained for the tenant's remaining retention
period, producers/UI disabled, dispatchers stopped, restricted export-only read access, converts
to destroy after an explicit end date) is a separate, unticketed follow-up once that business
decision lands — do not fold it in here.

Applies hard invariant #7 (`AGENTS.md`, per-customer DEKs from the first write) and #6 (every
table holding customer data appears in erasure statements) at the tenant level rather than the
customer level: this is whole-tenant termination, distinct in scope from T-050's customer-level
crypto-shred/physical-redaction tooling — no duplication, but worth cross-referencing since both
touch the erasure/crypto-shred machinery.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #13: destroy mode only, defer
  terminate-and-archive until pricing is decided)
