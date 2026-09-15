# messgr — Design

**Version 6** · 2026-09-15 · T-033 review: §4.10's `tenant_config` DDL and field-count prose were missing `kill_switch_release_rate` (T-016) and `reconcile_attempts_cap` (T-033)

Centralized communications orchestration and audit ledger for customer messaging across SMS, email, and WhatsApp.

**Constraints:** Rust · 100k–5M messages/day per tenant · 7-year retention · customer service + compliance users.

**Two deployments, one codebase:** on-premise for a single institution, and a regional cloud service for up to ~20 large institutions. On-prem is the N=1 case of the same multi-tenant system (§2.1) — not a fork, not a build flag.

**Design principle:** boring technology — the fewest moving parts that satisfy the requirements, biased toward components the ops team can reason about at 3am.

---

This file is the index. The design itself lives in [`development/design/`](development/design/),
split one file per top-level section — content and section numbers (`§N`) are unchanged from
before the split, only the file they live in changed. Every `§N` citation anywhere in this repo
(AGENTS.md, tickets, review-addendum.md) still means what it always meant; use the map below to
find which file holds it. See [`development/README.md`](development/README.md) for the fuller
navigation aid.

| Section | File |
|---|---|
| §1 What this system is, §2 Architecture (incl. 2.1–2.4) | [`01-overview-architecture.md`](development/design/01-overview-architecture.md) |
| §3 The OTP question | [`02-otp.md`](development/design/02-otp.md) |
| §4 Data model (4.1–4.11) | [`03-data-model.md`](development/design/03-data-model.md) |
| §5 The gate chain | [`04-gate-chain.md`](development/design/04-gate-chain.md) |
| §6 Send timing | [`05-send-timing.md`](development/design/05-send-timing.md) |
| §7 PII, retention, and the erasure conflict | [`06-pii-retention.md`](development/design/06-pii-retention.md) |
| §8 Throughput and campaign traffic | [`07-throughput.md`](development/design/07-throughput.md) |
| §9 Rate limiting and dispatcher topology | [`08-rate-limiting-dispatcher.md`](development/design/08-rate-limiting-dispatcher.md) |
| §10 Delivery receipts | [`09-delivery-receipts.md`](development/design/09-delivery-receipts.md) |
| §11 Query API and UI | [`10-query-api-ui.md`](development/design/10-query-api-ui.md) |
| §12 Failure modes and degraded operation | [`11-failure-modes.md`](development/design/11-failure-modes.md) |
| §13 Deployment | [`12-deployment.md`](development/design/12-deployment.md) |
| §14 Build order | [`13-build-order.md`](development/design/13-build-order.md) |
| Decisions taken, Still open | [`14-decisions-and-open-questions.md`](development/design/14-decisions-and-open-questions.md) |
