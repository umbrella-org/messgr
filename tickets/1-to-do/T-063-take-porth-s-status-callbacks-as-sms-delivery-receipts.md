---
id: T-063
title: Take porth's status callbacks as SMS delivery receipts
project: messgr
depends-on: []
spawned-by: [T-051]
impact: high
complexity: medium
cost: M
---

# T-063 — Take porth's status callbacks as SMS delivery receipts

## Outcome

After this ships, an SMS's `comms_event` history ends in the `delivered`, `failed` or
`expired` status porth reports, pushed by porth to messgr, instead of stopping at `sent`.

## Description

DESIGN.md §10 and decision 34 (2026-09-29): porth `POST`s each message's final status as
JSON to the callback URL messgr gave on submit (porth POR-026, which settles the body). The URL
carries the tenant's opaque webhook token. porth does not sign anything, and it is internal.
So `messgr-webhook` takes porth's callbacks on a **second, internal-only listener**, never on
the internet-facing one (`src/bin/webhook.rs` binds one TLS listener today, `WEBHOOK_LISTEN_ADDR`,
with route `/webhook/{webhook_token}/{provider}`). The token routes the tenant, and signature
verification is skipped for this route only. A forged receipt then needs a foothold on the
internal network, the same boundary porth's unauthenticated API already relies on.

The receipt enters the existing staging-and-promote path (T-047: `webhook_receipt_staging`,
`messgr-control webhook-promote`, orphans to `orphan_event`), so dedup, ordering and orphan
handling are unchanged. `src/webhook/handler.rs` parses a `GenericReceipt`
(`provider_ref`, `event_type`, `provider_status`). Mapping porth's body onto it, porth's
`message_id` being `provider_ref`, is this ticket's. porth reports no intermediate states, so an
SMS gets exactly one receipt event after `sent`. Whether an SMS `failed` receipt should feed
suppression the way a bounce does (§5) is a refinement question.

**porth prerequisite, not expressible as `depends-on:`:** POR-026 merged, and its JSON body
settled. POR-003 produces the `expired` status.

Soft coupling: T-062 sends the callback URL this receives. It can be built first, but it
receives nothing until T-062 lands.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-29 — created (TO DO). source: review: T-051 dropped because SMS goes through porth (decision 34, DESIGN.md §2.5); this is the messgr side of that decision
