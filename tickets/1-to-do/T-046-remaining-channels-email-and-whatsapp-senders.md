---
id: T-046
title: Remaining channels: email and WhatsApp senders
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: L
---

# T-046 — Remaining channels: email and WhatsApp senders

## Outcome

After this ships, a producer can send email and WhatsApp through the same `POST /comms`, the
same gate chain, and the same ledger as SMS — "every channel under one API, one ledger, one
gate chain" (build-order §14, step 11) stops being SMS-only.

## Description

Build-order step 11 (§14): concrete `Sender` implementations for email and WhatsApp behind the
channel-agnostic trait T-012 already shipped (`03-data-model.md` §4.10 — `provider_config`'s
primary key is already `(channel, priority)`, not SMS-specific). No new gate, ledger, or
dispatcher mechanism — the dispatcher, kill switches, quotas, and quiet-hours logic are already
channel-parametric; this ticket is provider adapters plus whatever is genuinely
channel-specific:

- Address format/validation differs per channel (email address vs. E.164 vs. WhatsApp's
  own identifier), which likely touches `customer_address.kind` handling and the HMAC-lookup
  path from T-015/T-018.
- Provider selection per channel is Still-open item #4 (`14-decisions-and-open-questions.md`,
  §12) — unresolved for SMS too, so this ticket inherits rather than newly creates that
  question. Needs a decision at refinement, not before.
- Template rendering (T-010) is channel-agnostic already; confirm during refinement whether
  email's richer format (subject line, possibly HTML) needs any template-store change or fits
  the existing render path as-is.

Out of scope: provider failover (§12.1, build-order step 16, SMS-specific and already a
separate concern), bulk/campaign sending (step 18).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 11, remaining gap identified when auditing unticketed steps against the board
