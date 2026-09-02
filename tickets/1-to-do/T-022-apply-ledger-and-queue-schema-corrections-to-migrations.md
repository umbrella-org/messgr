---
id: T-022
title: Apply ledger and queue schema corrections to migrations
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-022 — Apply ledger and queue schema corrections to migrations

## Outcome

The shipped schema matches DESIGN.md's corrected §4.1-§4.4 and §4.10: no dead `dek_id` column, an indexed `destination_hmac` and admin-panel-supporting outbox indexes exist, `idempotency` is scoped per-producer, `comms_event` dedup actually catches dispatch-internal events, `orphan_event` exists, and `staleness_max_age` is gone from both the schema and the `messgr-control tenant-config-set` CLI. A nightly `idempotency` sweep job exists (currently owned by nobody, deferred twice).

## Description

This ticket applies the schema corrections made to DESIGN.md during this audit (docs(design) commits "correct ledger and queue schema defects" and "correct gate-chain..."). Migrations are cheap here — there is no production data yet, so these land as edits to the existing migration files rather than new expand/contract migrations:

1. `migrations/tenant/0004_ledger_outbox_schema.sql`:
   - Drop `comms_request.dek_id` (never read, references nothing).
   - Add `CREATE INDEX ON comms_request (destination_hmac, created_at DESC)`.
   - Add `CREATE INDEX ON outbox (producer_id, next_attempt_at)` and `CREATE INDEX ON outbox (campaign_id, next_attempt_at) WHERE campaign_id IS NOT NULL`.
   - Rescope `idempotency` to `PRIMARY KEY (producer_id, key)`, adding the `producer_id uuid NOT NULL` column. Update `src/ingest` call sites that read/write idempotency rows to bind the caller's `producer_id`.
   - `comms_event.provider_ref` becomes `NOT NULL DEFAULT ''`; update `src/dispatcher/repo.rs::write_terminal` and any webhook-receipt insert path to write `''` rather than `NULL` for events with no provider reference (dispatch-internal events already always pass `None`/`NULL` today — becomes `Some("")` or equivalent).
   - Add the `orphan_event` table per DESIGN.md §4.4/§10 (not yet consumed by any code — T-034-equivalent reconciliation work is a separate, not-yet-filed ticket; this ticket only ships the schema).
2. `migrations/tenant/0002_tenant_config.sql`: drop `staleness_max_age`. This is a real, wired CLI surface, not a dead column — `src/bin/control.rs`'s `tenant-config-set` subcommand requires `--staleness-max-age-seconds` on every invocation, and `src/tenant_config/{repo,model,configure}.rs` all reference it. Removing the column means removing the CLI flag and every reference, and re-checking `docs/user-manual/` for the flag's documentation (`just docs-check`).
3. File the idempotency-sweep job as a follow-up if it doesn't already have an owner — `tickets/6-done/T-009` and `T-011` both deferred it without either claiming it; confirm during refinement whether a ticket already exists before filing a new one.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: applies the DESIGN.md ledger/queue schema corrections from the 2026-09-02 design/implementation audit to the shipped migrations.
