---
id: T-024
title: CI check that every PII-holding table is covered by erasure or a named exemption
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-024 — CI check that every PII-holding table is covered by erasure or a named exemption

## Outcome

CI fails if a table holding customer-linkable data exists that is neither in §7.2's physical-redaction statements nor on a named, reasoned exemption list checked in alongside the erasure code. This is DESIGN.md's own §14 CI requirement and PLAN.md's former T-045, scoped narrowly to the check itself — **not** a pull-forward of the full erasure feature (crypto-shred command, `erasure_request` table, physical-redaction job), which stays at build order step 15 behind gates and consent that don't exist yet.

## Description

This audit found `suppression` silently outside §7.2's erasure statements — not a bug, since `suppression` has no `customer_id` to erase by and is deliberately destination-scoped rather than customer-scoped (§5's suppression gate needs a recycled number to stay blocked regardless of who currently holds it) — but undocumented, which made it indistinguishable from the `comms_event` omission DESIGN.md already records as a past mistake (§7.2). DESIGN.md now names `suppression` as a stated exemption with its reason.

The mechanical check itself does not yet exist and doesn't need the full erasure feature built to be useful: it can run today, against whatever schema exists, and keep working as tables are added. Scope:

1. A CI-runnable check (likely a `just` recipe invoked from `docs-check` or its own target) that inspects the live tenant-database schema for columns holding customer-linkable data (by convention: any column referencing `customer_id`, or holding a `*_ciphertext`/`*_hmac` value) and confirms each such table appears in `src/erasure`'s redaction statements (once written) or on an explicit, reasoned exemption list committed alongside that code — not a bare table-name allowlist, which is what let `suppression`'s absence go unnoticed as an omission rather than a decision.
2. Since no `src/erasure` module exists yet, this ticket's first concrete target is `customer_address`, `customer_external_id`, `comms_request`, and `comms_event` — the four tables §7.2 already names in DESIGN.md — plus `suppression` as the one named exemption. The check must fail today if run against the current schema with an empty implementation, and pass once these five are correctly classified.
3. Building the actual crypto-shred and physical-redaction commands (`erasure_request` table, the throttled background job, `VACUUM` afterward) remains step 15 in the build order and is explicitly out of scope for this ticket — it depends on consent/suppression (step 5) and the customer projection being live in production data, neither of which changes here.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found suppression undocumented as an erasure exemption; re-specs PLAN.md's former T-045 (CI check against the live schema) narrowly, without pulling the full erasure feature forward from build step 15.
