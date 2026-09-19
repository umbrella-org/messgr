---
id: T-047
title: Webhook receiver and delivery-receipt ingestion (messgr-webhook)
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-047 — Webhook receiver and delivery-receipt ingestion (messgr-webhook)

## Outcome

After this ships, message status in the ledger and UI reflects what the provider actually did
(`delivered`, `bounced`, ...), not just "accepted by the provider" at send time, and bounces/
complaints feed suppression automatically instead of needing a manual loop.

## Description

Build-order step 12 (§14): `messgr-webhook`, the one internet-facing binary in an otherwise
internal system (`09-delivery-receipts.md` §10). Scope per that section:

- Separate minimal binary in the DMZ; signature verification and nothing else; a narrowly-scoped
  DB write path.
- Routes on an opaque per-tenant `webhook_token` (already provisioned in the control-DB `tenant`
  table, `03-data-model.md` §4.11 — "unread until messgr-webhook ships (step 12)"), never the
  tenant slug, so a public callback path leaks nothing.
- Tolerates duplicate, out-of-order, and early-arriving receipts: `comms_event`'s natural-key
  unique constraint plus `ON CONFLICT DO NOTHING` handles duplicates; the UI renders by
  `occurred_at`; a receipt with no matching `provider_ref` goes to `orphan_event`, whose
  reconciliation loop already shipped (T-030) and needs no rework here.

**Blocking open design question — must be answered before refinement, not assumed:**
`09-delivery-receipts.md` §10 states explicitly the encryption-placement mechanism is
"not yet chosen": the DMZ binary must stay narrowly-scoped (no tenant-wide Vault
decrypt/encrypt AppRole in the DMZ), but the raw provider payload is stored encrypted under the
customer's DEK (§4.4), which needs *some* process to do that encryption. Deferring it to an
internal process means the raw payload crosses the DMZ boundary and sits briefly unencrypted on
the internal side before that process picks it up — a real, bounded exposure window, not a
detail to paper over. Refinement must get this decided (with the user) before writing the
Implementation Plan; it is not something this ticket can default its way past.

Soft coupling: shares network-segmentation and firewall groundwork with whatever infrastructure
conversation on-prem deployment already requires (flagged in the same design section as usually
the longest-lead item).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 12, remaining gap identified when auditing unticketed steps against the board
