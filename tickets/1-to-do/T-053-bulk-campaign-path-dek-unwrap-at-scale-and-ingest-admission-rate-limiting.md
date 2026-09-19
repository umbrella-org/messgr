---
id: T-053
title: Bulk campaign path: DEK-unwrap-at-scale and ingest admission rate limiting
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-053 — Bulk campaign path: DEK-unwrap-at-scale and ingest admission rate limiting

## Outcome

After this ships, a producer can submit a multi-million-recipient campaign via `POST
/comms/bulk` and have it land correctly encrypted without melting the Vault mount, and a
runaway producer gets rejected at ingest (`429`) before touching the database, instead of
degrading it for everyone else.

## Description

Build-order step 18 (§14), bundling two related gaps `07-throughput.md` §8 and T-042 both
already named without building:

**1. Bulk ingestion path.** `POST /comms/bulk` accepts an NDJSON stream, lands it via `COPY`
into a staging table, then a single set-based INSERT into ledger and outbox (§8) — not yet
built; today only single-message `POST /comms` (T-011) exists.

**Blocking open design question — §8 is explicit this is "mechanism not yet chosen," not
assumed.** `COPY` writes `payload_ciphertext`/`destination_ciphertext` directly, so both must
already be encrypted under the right per-customer DEK before a row reaches staging (hard
invariant #7, no volume exception). A 2M-recipient campaign can span up to 2M distinct
customers — up to 2M distinct DEK unwraps, an order of magnitude past what §7.6's steady-state
cache was sized and pre-provisioned for. §8's own text: "Whatever the bulk path does —
pre-provision DEKs ahead of the batch, warm the cache from the campaign's recipient list before
`COPY` starts, or something else — it must be decided with this number in view, not discovered
when the first real campaign is slow or a Vault mount starts throttling." Refinement must settle
this mechanism with the user before writing the Implementation Plan.

**2. Admission rate limiting.** §5.1/§4 (gate-chain doc) defines it — "ingest API, protects
messgr's own database from a runaway producer, `429`, immediate, retryable" — and T-042 shipped
the *send*-quota half only, stating explicitly "admission-rate limiting on this API is still
unbuilt." This ticket builds the ingest-side half: reject before any DB write, distinct from
T-042's per-producer send quotas which are enforced at dispatch. `11-failure-modes.md`'s
"Producer floods with a runaway loop" row already assumes this exists ("Admission rate limit
rejects at ingest... before the DB is touched") — it does not yet.

Also in scope per `04-gate-chain.md` line 30: the **consent pre-filter on bulk campaign
submissions** — not authoritative (the send-time gate still is, per hard invariant #3), but
run at ingest to avoid enqueueing hundreds of thousands of rows that will be discarded by
consent at dispatch anyway.

Soft coupling: shares `provider_config`/DEK-cache machinery with T-051/T-052; the DEK
pre-provisioning mechanism this ticket picks should stay consistent with §7.6's existing
pre-provisioning batch job (T-008) rather than inventing a parallel one.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 18, remaining gap identified when auditing unticketed steps against the board
