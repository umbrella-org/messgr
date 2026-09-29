---
id: T-062
title: Send SMS through each tenant's porth over its REST API
project: messgr
depends-on: []
spawned-by: [T-051]
impact: critical
complexity: high
cost: L
---

# T-062 — Send SMS through each tenant's porth over its REST API

## Outcome

After this ships, every SMS messgr sends reaches the customer through the tenant's own porth.
OTP is marked high priority, short-lived and not to be kept. Queued messages carry the time left
before `expires_at` and an idempotency key, and the dispatcher hands them to porth no faster than
the handoff cap. A marketing backlog then waits in messgr's outbox, where kill switches and
cancellation still reach it, instead of in porth.

## Description

DESIGN.md §2.5 and decision 34 (2026-09-29): SMS goes through porth's REST API, one porth
per tenant, and messgr builds no porth functionality. Today SMS goes through `HttpSender`
(`src/sender/http.rs`), which speaks this codebase's own mock contract (`POST {base}/messages`,
bearer token, `{to, body}`). That contract is not porth's. porth's REST submit is
`POST /api/v1/sms/send` with `from_number`, `to_number` and `message`, it is unauthenticated,
and it returns a `message_id`.

Gaps found while filing, each in DESIGN.md §2.5 and §4.10:

- **One porth per tenant needs a URL per tenant.** The base URL is one environment variable per
  binary (`DISPATCHER_SMS_BASE_URL` in `src/bin/dispatcher.rs`, `SMS_SENDER_BASE_URL` in
  `src/bin/sms_sender.rs`, `OTP_BASE_URL` in `src/bin/otp.rs`). The dispatcher runs one
  process per tenant, so an environment variable works there. `messgr-sms-sender` and
  `messgr-otp` serve many tenants and cannot. `provider_config` needs the URL (a column; a
  nullable one with the old variable as fallback keeps the migration backward-compatible).
- **Sender ID.** porth requires a `from` (or a configured default). messgr has none today.
- **`credential_path`** is `NOT NULL` but porth's REST API has no credential to point at.
- **Submit fields** (porth design 1.30 §4.1): high priority for auth; a validity period, short
  for OTP and the time left before `expires_at` for a queued message (§6.2); "do not keep the
  text" for auth (§7.4); `comms_request.id` as the idempotency key; and a callback URL carrying
  the tenant's opaque webhook token (§10, T-063).
- **The handoff cap** (§5, replacing the rate-limit gate). The dispatcher hands messages to
  porth no faster than `provider_config.rate_limit_per_sec`, which nothing reads today
  (`src/bin/control.rs` says so). It is a limit on the dispatcher, not pacing to the operator.
- **The OTP paths' walk over `provider_config` rows** (`src/sms_sender/provider.rs`,
  `src/otp/provider.rs`) cannot fail over from an operator outage, because porth accepts and
  queues (§3's correction). With one porth per tenant the SMS list has one row. Refinement
  decides whether the walk goes or stays as harmless.

Out of scope: anything porth does (routing, failover between operators, pacing to the operator,
SMSC retries, encoding). A gap there is a porth ticket (§2.5). Also out of scope: SMS receipts
(T-063), and email and WhatsApp, which stay on `HttpSender`.

**porth prerequisites, not expressible as `depends-on:` across umbrellas.** POR-012 (a production
porth can bind), POR-024 (a `throughput` cap safe to set the handoff cap under), POR-025
(priority), POR-027 (validity and not keeping the text) and POR-028 (idempotency key) must be
merged in porth first. Refinement checks their state. POR-003 bounds porth's plaintext copy
(§7.2), which matters before production traffic but not for building this.

Docs: `docs/user-manual/control-plane-cli.adoc`'s `provider-config` section describes
`HttpSender`, "failover order" and an unread `rate_limit_per_sec`, and all three change here.

Soft couplings: T-063 (the callback this ticket's URL points at), T-053 (bulk sends go through the
handoff cap), T-061 (a region check could cover the porth URL).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-29 — created (TO DO). source: review: T-051 dropped because SMS goes through porth (decision 34, DESIGN.md §2.5); this is the messgr side of that decision
