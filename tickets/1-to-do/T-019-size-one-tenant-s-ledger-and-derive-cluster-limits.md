---
id: T-019
title: Size one tenant's ledger and derive cluster limits
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: low
cost: S
---

# T-019 — Size one tenant's ledger and derive cluster limits

## Outcome

A written sizing document states, for a tenant across the design's 100k–5M messages/day range: expected `comms_request`/`comms_event` storage per year, a recommended cluster size and tenants-per-cluster count, a quoted single-tenant restore RTO, and the point at which the 18-month slow-storage migration (§7.5) starts to matter. No code changes.

## Description

DESIGN.md's own Still-open #15 states the prerequisite this ticket exists to close: "no per-tenant storage estimate exists anywhere in this document." The ledger holds rendered message bodies (§7) for 7 years across a 50× volume range (100k–5M/day, §1) — an email-heavy tenant's `payload_ciphertext` dominates its own size and therefore the cluster's, so nothing about capacity planning can proceed until one tenant is sized.

This ticket is a document, not a migration or a code change. It must produce a concrete number (or a small table across the volume range) for:

- Average row size for `comms_request` and `comms_event` at realistic payload sizes per channel (SMS ~160 bytes, email/WhatsApp larger), including ciphertext overhead (AES-256-GCM nonce + tag) and the encrypted `provider_payload_ciphertext`.
- Annual storage growth per tenant across the 100k–5M/day range, and the resulting 7-year total.
- From that: how many tenants of what size profile reasonably share a Postgres cluster (§2.1's "not thousands of small tenants" assumption needs a number behind it).
- The 18-month cold-storage migration point (§7.5) restated in GB rather than months, so ops can plan the slower tablespace's capacity.
- A single-tenant restore RTO (§13's full-cluster-recovery-to-a-side-instance procedure), as a function of the cluster size this sizing produces.

Unblocks the deferred DESIGN.md commit recording this ticket's number (§2.1, §7.5, §13) and closes Still-open #14 (regional backup policy) and #15 (single-tenant restore RTO), which are both currently unanswerable without this.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit of DESIGN.md against the shipped code and ticket reviews; closes Still-open #15's stated prerequisite.
