# development/

Overarching docs that aren't code, tickets, or the board.

## Design

[`DESIGN.md`](../DESIGN.md) at the repo root is the front door and section→file index. The
design itself lives in [`design/`](design/), one file per top-level section of the original
document:

| File | Covers |
|---|---|
| [`01-overview-architecture.md`](design/01-overview-architecture.md) | §1 What this system is, §2 Architecture |
| [`02-otp.md`](design/02-otp.md) | §3 The OTP question |
| [`03-data-model.md`](design/03-data-model.md) | §4 Data model |
| [`04-gate-chain.md`](design/04-gate-chain.md) | §5 The gate chain |
| [`05-send-timing.md`](design/05-send-timing.md) | §6 Send timing |
| [`06-pii-retention.md`](design/06-pii-retention.md) | §7 PII, retention, and the erasure conflict |
| [`07-throughput.md`](design/07-throughput.md) | §8 Throughput and campaign traffic |
| [`08-rate-limiting-dispatcher.md`](design/08-rate-limiting-dispatcher.md) | §9 Rate limiting and dispatcher topology |
| [`09-delivery-receipts.md`](design/09-delivery-receipts.md) | §10 Delivery receipts |
| [`10-query-api-ui.md`](design/10-query-api-ui.md) | §11 Query API and UI |
| [`11-failure-modes.md`](design/11-failure-modes.md) | §12 Failure modes and degraded operation |
| [`12-deployment.md`](design/12-deployment.md) | §13 Deployment |
| [`13-build-order.md`](design/13-build-order.md) | §14 Build order |
| [`14-decisions-and-open-questions.md`](design/14-decisions-and-open-questions.md) | Decisions taken, Still open |

Content and `§N` numbering are unchanged from before the split — only the file each section lives
in changed. Existing `§N` citations across the repo (AGENTS.md's hard invariants, tickets,
`review-addendum.md`) still resolve; use the table above (or the same map in `DESIGN.md`) to find
the file.

**When editing:** a change to one section stays inside that section's file. A change that
renumbers or moves a `§N` boundary must be reflected in both `DESIGN.md`'s map and this table —
grep the repo for the old `§N` afterward (review-addendum.md's own consistency-audit step calls
this out).

## Review addendum

[`review-addendum.md`](review-addendum.md) — messgr-specific review rules layered on top of the
brine review protocol (`.agents/skills/brine/resources/review-protocol.md`). Wired by path in
`pickle.toml`'s `review_addendum` key. See `AGENTS.md`'s "Review addendum" section for how it's
invoked.
