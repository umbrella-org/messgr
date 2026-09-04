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

The shipped schema matches DESIGN.md's corrected §4.1-§4.4 and §4.10: no dead `dek_id` column, an indexed `destination_hmac` and admin-panel-supporting outbox indexes exist, `idempotency` is scoped per-producer, `comms_event` dedup actually catches dispatch-internal events, `orphan_event` exists, and `staleness_max_age` is gone from both the schema and the `messgr-control tenant-config-set` CLI. `development/design/04-gate-chain.md`'s gate table no longer lists the already-cut staleness gate. The unowned nightly `idempotency` sweep job is filed as its own ticket (T-029) rather than left to drift a third time.

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
3. The idempotency-sweep job — deferred by both `tickets/6-done/T-009` and `T-011` without either claiming it — has no existing ticket (checked during refinement). Filed as T-029, `spawned-by: [T-022]`.
4. **`development/design/04-gate-chain.md` §5 still lists a live "Staleness" gate row**, even though decision 27 in the decisions table and §4.10's own correction note both claim it was "removed from the gate chain" — that removal was never actually made. Drop the row while this ticket is already touching `tenant_config.staleness_max_age`'s removal, and fix `docs/user-manual/control-plane-cli.adoc`'s matching "not built yet" framing (the gate was cut, not deferred).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-022-apply-ledger-and-queue-schema-corrections-to-migrations
```

Local WIP commits as you go. Do not push or open a merge request without explicit user
approval (publish-gated, root-path child — tidy WIP into atomic commits before presenting,
rules §0/§4 item 7).

### Prerequisite gate (hard)

None. `depends-on: []`.

### Confirmed design decisions (do not deviate without asking)

1. **Migrations are edited in place, not expanded/contracted.** `migrations/tenant/0002_tenant_config.sql` and `0004_ledger_outbox_schema.sql` are rewritten directly — there is no production data yet, and `development/design/03-data-model.md`'s own correction note calls the `staleness_max_age` drop "a schema migration... tracked as part of the ledger/queue schema remediation ticket," i.e. this one, done this way.
2. **`dek_id` is dropped with no replacement.** DESIGN.md's correction (`development/design/03-data-model.md`, "Correction: a `dek_id uuid` column...") is authoritative: encryption keys are per-customer via `customer_dek`, never per-message. Do not add a replacement column.
3. **`write_terminal` (`src/dispatcher/repo.rs`) converts `None` to `""` at the SQL-bind site only** — `.bind(provider_ref.unwrap_or(""))` — keeping its `Option<&str>` signature and every call site (`worker.rs`'s two calls, `drain.rs`'s `write_expired`/`write_discarded`) unchanged. There is no webhook-receipt insert path yet (grepped `src/` — only `write_terminal` inserts into `comms_event`), so that half of the ticket's Description item 1 needs no code change.
4. **Idempotency scoping touches both the write and read paths.** `insert_transactional`'s own claim insert, `find_idempotent_reply` (called from `ingest/handler.rs`'s fast pre-check *and* from `insert_transactional`'s post-conflict replay lookup), all bind `producer_id` — otherwise a second producer reusing another producer's key can still read back that producer's row even after the write side is fixed.
5. **`orphan_event` ships schema-only** — the table and its `provider_ref` index, verbatim from DESIGN.md §4.4/§10. No Rust model, repo, or CLI surface; reconciliation is out of scope (a separate, not-yet-filed ticket, per this ticket's Description item 1).
6. **The gate-chain doc fix (Description item 4) is documentation-only** — no code or schema reads `04-gate-chain.md`'s table; deleting the stale row and fixing the adoc wording carries no migration or code risk.

### Tasks

#### Task 1 — Ledger/outbox schema + ingest/dispatcher code

In `migrations/tenant/0004_ledger_outbox_schema.sql`:
- Drop the `dek_id uuid,` line from `comms_request`.
- Add `CREATE INDEX ON comms_request (destination_hmac, created_at DESC);`.
- Add `CREATE INDEX ON outbox (producer_id, next_attempt_at);` and
  `CREATE INDEX ON outbox (campaign_id, next_attempt_at) WHERE campaign_id IS NOT NULL;`.
- Change `idempotency` to:
  ```sql
  CREATE TABLE idempotency (
      producer_id       uuid        NOT NULL,
      key               text        NOT NULL,
      comms_request_id  uuid        NOT NULL,
      expires_at        timestamptz NOT NULL,
      PRIMARY KEY (producer_id, key)
  );
  ```
- Change `comms_event.provider_ref` to `text NOT NULL DEFAULT ''` and update its comment.
- Add, verbatim from DESIGN.md §4.4:
  ```sql
  CREATE TABLE orphan_event (
      id                          uuid        PRIMARY KEY,
      received_at                 timestamptz NOT NULL,
      provider                    text        NOT NULL,
      provider_ref                text        NOT NULL,
      occurred_at                 timestamptz NOT NULL,
      event_type                  text        NOT NULL,
      provider_status             text,
      provider_payload_raw        jsonb,
      reconcile_attempts          smallint    NOT NULL DEFAULT 0
  );
  CREATE INDEX ON orphan_event (provider_ref);
  ```

In `src/ingest/repo.rs`:
- Remove `dek_id` from the `comms_request` `INSERT`'s column list and its `NULL` placeholder; renumber the remaining `$n` placeholders.
- `find_idempotent_reply`: add a `producer_id: Uuid` parameter; change the query to
  `SELECT comms_request_id FROM idempotency WHERE producer_id = $1 AND key = $2`.
- `insert_transactional`: bind `producer_id` into the `idempotency` INSERT's column list and
  values; change `ON CONFLICT (key)` to `ON CONFLICT (producer_id, key)`; pass `producer_id`
  into the post-conflict `find_idempotent_reply(pool, producer_id, idempotency_key)` call.

In `src/ingest/handler.rs`: pass `producer.producer_id` into the fast pre-check call to
`find_idempotent_reply` (currently `find_idempotent_reply(&tenant.pool, &idempotency_key)`).

In `src/dispatcher/repo.rs::write_terminal`: change `.bind(provider_ref)` to
`.bind(provider_ref.unwrap_or(""))` (decision 3). No other call site changes.

#### Task 2 — tenant_config schema + code + CLI

In `migrations/tenant/0002_tenant_config.sql`: drop the `staleness_max_age interval NOT NULL,`
line.

- `src/tenant_config/model.rs`: remove `staleness_max_age` from `TenantConfig` and
  `TenantConfigInput`, remove `staleness_max_age_duration`, and remove the field from
  `TenantConfigInput::matches`.
- `src/tenant_config/repo.rs`: remove `staleness_max_age` from `load`'s `SELECT` list and from
  `upsert`'s `INSERT`/`ON CONFLICT DO UPDATE` column lists and bind.
- `src/tenant_config/configure.rs`: remove `"staleness_max_age_seconds"` from `audit`'s JSON
  payload.
- `src/bin/control.rs`: remove the `--staleness-max-age-seconds` arg
  (`TenantConfigCommand::Set::staleness_max_age_seconds`), its use in building
  `TenantConfigInput`, and `staleness_max_age_seconds={}` from the `Show` `println!`.

#### Task 3 — Update tests for the schema/code changes

- `tests/ledger_outbox_schema.rs`: replace `idempotency_key_is_globally_unique` (its assertion
  is now false) with two tests: same `producer_id` + same `key` still conflicts on the PK; two
  different `producer_id`s with the same `key` both succeed. Add a `comms_event` dedup test:
  two inserts sharing `(occurred_at, comms_request_id, event_type)` and `provider_ref = ''`
  must conflict (proves the dispatch-internal dedup fix). Add a minimal `orphan_event`
  insert-then-select round-trip test, in this file's existing style (no model/repo layer,
  matching decision 5 above).
- `tests/tenant_config.rs`, `tests/ingest.rs`, `tests/kill_switch.rs`,
  `tests/partition_lifecycle.rs`: remove the `staleness_max_age: PgInterval { .. }` field from
  every `TenantConfigInput` literal.
- `src/bin/control.rs`'s own `#[cfg(test)]` module: remove `--staleness-max-age-seconds` (and
  its value) from the two `Cli::try_parse_from` fixtures that currently pass it.

#### Task 4 — Docs

- `docs/user-manual/control-plane-cli.adoc`: remove `--staleness-max-age-seconds 7200` from the
  `tenant-config set` example command; remove "(the staleness bound, the quota day-boundary
  timezone)" down to just the quota day-boundary timezone; remove the **staleness gating**
  bullet from "Two pieces of the design are explicitly not built yet" (it was cut, not
  deferred — DESIGN.md decision 27).
- `development/design/04-gate-chain.md` §5: delete the
  `| Staleness | ... | Defer + alert |` row from the gate table (Description item 4).

### Acceptance test

```
just build
just lint
cargo test --test ledger_outbox_schema
cargo test --test tenant_config
cargo test --test ingest
cargo test --test kill_switch
cargo test --test partition_lifecycle
just test
just docs-check
```

All must pass clean, including the new/updated `ledger_outbox_schema.rs` tests named in Task 3.

### Docs update (mandatory when user-facing)

`docs/user-manual/control-plane-cli.adoc` per Task 4 — the `--staleness-max-age-seconds` flag is
a real, documented CLI surface being removed.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. Docs updated per Task 4.
3. Write a summary: files touched, decisions honoured, anything deferred (the idempotency-sweep
   follow-up ticket, filed separately during refinement — see History).
4. Suggest a Conventional Commit message, e.g.:
   ```
   fix(schema): apply ledger and queue schema corrections (T-022)
   ```
5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits before
   presenting (root-path child, rules §0/§4 item 7) — default to preserving them on merge
   (rebase/keep-history) rather than squashing.
6. Commit locally on the ticket branch. Publish only per commit policy (no push/MR without
   explicit user approval). Under `layout = "in-tree"`, before pushing verify the remote base is
   not behind (`git fetch origin main && git diff --name-only origin/main...HEAD | grep
   '^tickets/'` must print nothing) — then push and open the merge request. Hand back to the
   user.

## Review

- [x] Reviewer independence settled (step 0): **delegated** — the reviewing agent authored this branch in the same session, so audits (steps 2–4a) were run by a fresh, adversarially-briefed sub-agent with no memory of writing the code. Every delegated finding was re-verified by hand before being recorded here.
- [x] Implementation audit — acceptance test re-run (`just build`, `just lint`, `cargo test --test {ledger_outbox_schema,tenant_config,ingest,kill_switch,partition_lifecycle}`, `just test`, `just docs-check`): all pass clean, independently confirmed both by the delegated reviewer and by this session's own earlier run. All four Tasks and all 6 confirmed design decisions verified done, in the files named.
- [x] Quality audit (step 3) — new tests (`idempotency_key_is_unique_per_producer`/`_may_be_reused_across_producers`, `comms_event_dedup_catches_dispatch_internal_events_with_no_provider_ref`, `orphan_event_insert_then_select_round_trips`) are mutation-testable, not `is_err()`-alone shells — confirmed the dedup test would go red if the `NOT NULL DEFAULT ''` fix were reverted.
- [x] Consistency audit (step 4) — repo-wide grep for "staleness" clean except F3 below.
- [x] Documentation audit (step 4a) — `just docs-check` clean; CLI flag removal fully reflected in `docs/user-manual/`.
- [x] Docs-readability pass (step 4b) — skipped: no docs-readability reviewer available in this host session.
- [x] Findings recorded below with severity, class, and disposition (step 5).
- [x] Ticket moved (step 6) — see History.
- [x] Other references updated / governing documents reconciled (step 7) — F1 below is the one gap found; it goes to rework rather than being fixed inline, since it contradicts AGENTS.md hard invariant 6.
- [x] Remaining-tickets impact sweep done (step 8) — T-024 patched directly (F2); no other ticket in `1-to-do/`/`2-ready/` references T-022 or `orphan_event`.
- [x] Summary + commit message & MR attributes presented for approval (step 9) — scoped re-review below confirms F1 resolved; presenting now.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | docs-gap | — | `orphan_event` (new table, holds unencrypted third-party `provider_payload_raw`) is missing from DESIGN.md §7.2's erasure statements and its named-exemption list — AGENTS.md hard invariant 6, violated by omission exactly as invariant 6 predicts ("third-party payloads are the classic miss") | `development/design/06-pii-retention.md:45` (exemption paragraph ends after `suppression`, no `orphan_event` mention); `development/design/13-build-order.md:34` (exemption-tracking list: "currently: `suppression`, §7.2") | Add an `orphan_event` exemption paragraph to §7.2 alongside `suppression` (no `customer_id` column; reconciliation, which would resolve one, is out of scope per this ticket's own decision 5 — cite T-030), and add it to build-order.md's exemption-tracking list |
| F2 | non-blocking | plan-wrong | folded (T-024) | T-024's detection rule (`customer_id`/`*_ciphertext`/`*_hmac` only) can't see `orphan_event.provider_payload_raw`, and its own second assertion would fail if `orphan_event` were added to `EXEMPT` without widening the rule | `tickets/2-ready/T-024-*.md` decisions 2–3, Task 2's manifest/query (pre-patch) | Patched directly: widened detection rule to also match `%_raw`, added `orphan_event` as a 5th exemption (target 8→9 tables), updated Task 1/2 and decisions 2–3, recorded in T-024's own History |
| F3 | non-blocking | stale-xref | fixed inline | `src/bin/control.rs`'s `TenantConfig` subcommand doc comment still listed "staleness bound" after Task 2 removed the flag | `src/bin/control.rs:69` (pre-fix) | Fixed inline: dropped the phrase, committed on the ticket branch (commit `f2dd6b3`) |
| F4 | non-blocking | design | new ticket | `orphan_event` reconciliation is named in design (`development/design/09-delivery-receipts.md:11`) but no ticket owns building it — the same class of drift this ticket itself caught for the idempotency sweep (filed as T-029) | grep for "orphan_event"/"reconcil" across `tickets/`: only this ticket references it before now | Filed as T-030 (`spawned-by: [T-022]`), graded to match T-029's shape (impact low, complexity low, cost S) |

Disposition summary: 1 blocking (F1, routes to rework), 1 folded (F2 → T-024), 1 fixed inline (F3), 1 new ticket (F4 → T-030).

cost: estimated M, actual M

### Rework fix record — round 1 (commit a5caad4)

Fixed F1 only, per scope. Added an `orphan_event` named-exemption paragraph to
`development/design/06-pii-retention.md` §7.2, matching `suppression`'s style — no `customer_id`
column, third-party payload kept unencrypted, temporary until T-030's reconciliation resolves a
row into an ordinary `comms_event`. Added `orphan_event` to `development/design/13-build-order.md`
§14's exemption-tracking list alongside `suppression`. Docs-only change; full acceptance test
(`just build`/`lint`/`test`/`docs-check`) re-run clean.

### Scoped re-review verdict

Reviewer independence: **delegated** (I authored the rework fix commit this session) — a fresh
adversarial sub-agent audited commit `a5caad4` in isolation, scoped strictly to F1 and its own
diff, per this pass's re-review rule. Findings re-verified by hand: `orphan_event`'s schema
(`migrations/tenant/0004_ledger_outbox_schema.sql:80-91`) confirmed to have no `customer_id`
column, matching the new paragraph's structural claim; the §4.4/§10 citations check out against
the actual section headers; T-030 exists on `main` with an Outcome matching what the paragraph
claims it will do; `just docs-check` re-run clean independently.

**F1: resolved.** No new findings from the fix's own diff — it is exactly the two files the fix
record claims, docs-only, no scope creep, no new stale cross-references. (One pre-existing gap
noted but correctly out of scope: `development/design/14-decisions-and-open-questions.md` has no
row for either `suppression`'s or `orphan_event`'s exemption — predates this diff, not introduced
by it.)

**Verdict: no blocking findings remain.** Ticket proceeds to `tickets/6-done/`.

## History

- 2026-09-02 — created (TO DO). source: audit: applies the DESIGN.md ledger/queue schema corrections from the 2026-09-02 design/implementation audit to the shipped migrations.
- 2026-09-04 — TO DO → READY: implementation plan complete; also folds in a fix for `development/design/04-gate-chain.md`'s stale Staleness gate row (decision 27 claimed removal that never happened), added to the plan at the user's direction during refinement.
- 2026-09-04 — TO DO → READY: plan complete
- 2026-09-04 — READY → IN DEVELOPMENT: picked up
- 2026-09-04 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-04 — IN REVIEW → REWORK: review: F1 blocking (orphan_event missing from DESIGN.md §7.2 erasure statements)
- 2026-09-04 — REWORK → IN REVIEW: findings fixed
- 2026-09-04 — IN REVIEW → DONE: scoped re-review clean; F1 resolved, no blocking findings remain
- 2026-09-04 — pushed `feat/T-022-apply-ledger-and-queue-schema-corrections-to-migrations`; opened https://github.com/umbrella-org/messgr/pull/34 (base `main`). Not yet merged.
- 2026-09-04 — PR #34's `fmt` CI job failed (not caught locally — `just build`/`just lint` don't run `cargo fmt --check`). Fixed with two commits on the same branch: `8976a37` (ran `cargo fmt` on this ticket's own `src/ingest/{handler,repo}.rs` changes) and `d33d472` (folded `fmt-check` into `just lint` — user-directed, bundled here rather than filed separately — so no ticket's Acceptance Test misses this again; CI's `clippy` job now also installs the `rustfmt` component). Pushed; awaiting a green CI run.
- 2026-09-04 — MERGED: PR #34 merged to `main` (`2239233`).
