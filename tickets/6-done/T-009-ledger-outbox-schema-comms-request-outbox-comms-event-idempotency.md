---
id: T-009
title: Ledger + outbox schema: comms_request, outbox, comms_event, idempotency
project: messgr
depends-on: [T-005, T-007]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-009 — Ledger + outbox schema: comms_request, outbox, comms_event, idempotency

## Outcome

After this ships, the core data model of the system exists: a monthly-partitioned
`comms_request` ledger, an `outbox` a dispatcher can claim from, a `comms_event` append log,
and an `idempotency` table, all with the indexes the design specifies — the foundation every
later gate, dispatcher, and query surface builds on.

## Description

Build the ledger + outbox schema exactly as specified in design §4.1–§4.4: `comms_request` as
monthly RANGE partitions, `outbox`, `comms_event`, `idempotency`, and their indexes. The ledger
is self-contained by design (§4.1) — `customer_id` and the destination live on every row, and
the customer timeline must never join the customer projection later. Part of the step-2 ticket
family (`family: T-007`; see T-007). Consent (`T-020`), suppression (`T-021`), and the template
store (`T-010`) — also documented in §4.4 — are explicitly out of scope; this ticket covers only
the four tables its title names.

Depends on `T-005` (producer registry, merged) — `comms_request.producer_id` and
`outbox.producer_id` reference the identity it created, even though the design's own
`CREATE TABLE` snippets carry no `REFERENCES` clause to enforce it (§4.1/§4.2, honoured
verbatim below). `T-007` (tenant_config, merged) is a family/sequencing dependency, not a
functional one for this ticket specifically: on inspection, none of `comms_request`/`outbox`/
`comms_event`/`idempotency`'s columns read `tenant_config` (no timezone or retention column
appears in §4.1–§4.4's own schema) — the original filing note about "partition and schedule
columns" needing tenant_config defaults describes T-012/T-014, both later members of this same
family, not this ticket. Both dependencies are already satisfied either way.

Schema only — no Rust model, repository, or CLI surface. Nothing writes to these tables until
`T-011` (ingest) and `T-013` (dispatcher) exist; confirmed with the user during refinement that
a model struct or repo function here would sit unused for at least one more ticket, so each of
`T-010`/`T-011`/`T-013` adds its own typed structs scoped to what it actually needs. This
ticket's own acceptance test exercises the schema directly with raw `sqlx::query`.

`comms_request` and `comms_event` are RANGE-partitioned by month, but the create-ahead job that
keeps future partitions provisioned (`T-014`, `depends-on: [T-009]`) doesn't exist yet — a
migration that creates only the parent partitioned tables would reject every `INSERT` until
`T-014` lands. Confirmed with the user: this migration bootstraps the current and next calendar
month as real partitions, computed at migration-run time (i.e. whenever a tenant is
provisioned), just enough for `T-011`/`T-013`'s own acceptance tests to insert rows. `T-014`
takes over all ongoing partition creation from there.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-009-ledger-outbox-schema
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented
(rules §0). Do not push and do not open a merge request without explicit user approval. Ticket
and board bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-005` is in `6-done/` and merged to `main` — the `producer` table it created is the
  identity `comms_request.producer_id`/`outbox.producer_id` refer to (no DB-level FK, per
  decision 1 below).
- `T-007` is in `6-done/` and merged to `main` — family/sequencing dependency only; this
  ticket's migration does not read `tenant_config` (see Description).
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init` — the
  integration tests provision real tenants.

### Confirmed design decisions (do not deviate without asking)

1. **Schema matches DESIGN.md §4.1–§4.4 verbatim, including its omissions.** No `tenant_id`
   column on `outbox`/`comms_event`/`idempotency` — only `comms_request` carries one, exactly
   as the design's own `CREATE TABLE` statements show (a tripwire + consolidation column, §2.1;
   the other three tables' snippets have none). No `REFERENCES` clause on `producer_id`,
   `dek_id`, or `outbox.comms_request_id` — the design's own snippets declare none of these as
   enforced foreign keys, so none is added here even though the comment on
   `outbox.created_at` ("FK component into ledger partition") signals the *intent* to support
   one. Consent, suppression, and template tables (also in §4.4) are out of scope — see
   Description.
2. **Bootstrap two calendar-month partitions — current and next — for `comms_request` and
   `comms_event`, computed at migration-run time via a `DO` block.** `T-014` (create-ahead,
   `depends-on: [T-009]`) doesn't exist yet, so without a bootstrap step every `INSERT` would be
   rejected (`no partition of relation ... found for row`) until it does. Confirmed with the
   user. `T-014` takes over ongoing creation from here; this bootstrap only has to outlive this
   ticket's own acceptance test and unblock `T-011`/`T-013`'s.
3. **Partition naming: `<table>_<YYYY>_<MM>`** (e.g. `comms_request_2026_08`,
   `comms_event_2026_09`). No design section or existing migration names a convention; fixed
   here so `T-014` extends it consistently rather than inventing its own.
4. **`outbox` and `idempotency` are plain, unpartitioned tables** — exactly as §4.2/§4.3
   specify. No bootstrap step applies to them; they carry no `PARTITION BY` clause.
5. **No Rust model, repository, or CLI code — migration only.** Confirmed with the user:
   nothing consumes these tables yet (`T-011` ingest, `T-013` dispatcher are the first
   writers/readers), so a struct or repo function added now would sit unused for at least one
   more ticket and risk not matching what its first real consumer needs. The acceptance test
   below exercises the schema with raw `sqlx::query`/`query_as::<_, (...)>` tuples, not a typed
   model.

### Tasks

#### Task 1 — Tenant migration

Create `migrations/tenant/0004_ledger_outbox_schema.sql`:

```sql
-- Ledger + outbox schema (DESIGN.md §4.1-4.4, T-009): comms_request,
-- outbox, comms_event, idempotency, and their indexes, verbatim from the
-- design's own CREATE TABLE statements. No tenant_id column on
-- outbox/comms_event/idempotency and no REFERENCES clauses beyond what
-- the design itself declares -- see T-009 decision 1. Consent (T-020),
-- suppression (T-021), and template (T-010) -- also in §4.4 -- are
-- separate tickets.

CREATE TABLE comms_request (
    tenant_id              uuid        NOT NULL,   -- tripwire + consolidation path (§2.1)
    id                     uuid        NOT NULL,
    created_at             timestamptz NOT NULL,
    customer_id            uuid        NOT NULL,   -- always set; provisional shell if unresolvable (§4.6)
    channel                text        NOT NULL,   -- sms | email | whatsapp
    class                  text        NOT NULL,   -- auth | transactional | marketing
    template_id            text        NOT NULL,
    template_version       int         NOT NULL,   -- pinned; templates are immutable per version
    campaign_id            text,                   -- null for transactional
    destination_hmac       bytea       NOT NULL,   -- keyed HMAC, pepper in Vault; indexed lookup
    destination_ciphertext bytea       NOT NULL,   -- under customer DEK; the address as actually used
    payload_ciphertext     bytea,                  -- NULL for auth class; see §7
    dek_id                 uuid,
    producer_id            uuid        NOT NULL,   -- registered caller (§4.9)
    scheduled_for          timestamptz,            -- NULL = send immediately (§6.2)
    expires_at             timestamptz,            -- drop rather than send late (§6.2)
    final_status           text,                   -- NULL while in flight; see §4.1
    finalized_at           timestamptz,
    PRIMARY KEY (created_at, id)
) PARTITION BY RANGE (created_at);

CREATE INDEX ON comms_request (customer_id, created_at DESC);
CREATE INDEX ON comms_request (final_status, created_at DESC);
CREATE INDEX ON comms_request (campaign_id, created_at) WHERE campaign_id IS NOT NULL;

CREATE TABLE outbox (
    comms_request_id  uuid        PRIMARY KEY,
    created_at        timestamptz NOT NULL,     -- FK component into ledger partition
    channel           text        NOT NULL,
    class             text        NOT NULL,
    priority          smallint    NOT NULL,     -- 1 transactional, 2 marketing; stored, not computed
    customer_id       uuid        NOT NULL,
    address_id        uuid        NOT NULL,     -- resolved contact point (§4.6); consent keys on it
    producer_id       uuid        NOT NULL,     -- quota + kill-switch scoping (§5.1, §5.2)
    campaign_id       text,
    next_attempt_at   timestamptz NOT NULL,     -- future-dated for scheduled sends (§6.2)
    expires_at        timestamptz,
    cancelled_at      timestamptz,              -- set by cancellation; re-checked before send
    attempts          smallint    NOT NULL DEFAULT 0,
    leased_until      timestamptz
);

CREATE INDEX outbox_claim ON outbox (channel, priority, next_attempt_at)
    WHERE leased_until IS NULL;

CREATE TABLE idempotency (
    key               text        PRIMARY KEY,
    comms_request_id  uuid        NOT NULL,
    expires_at        timestamptz NOT NULL
);

CREATE TABLE comms_event (
    comms_request_id  uuid        NOT NULL,
    customer_id       uuid        NOT NULL,  -- denormalized so erasure can find these rows
    occurred_at       timestamptz NOT NULL,
    event_type        text        NOT NULL,
      -- queued | sent | delivered | failed | bounced | read | complaint
      -- | expired | cancelled | suppressed_consent | suppressed_list | unverified_address
    provider_ref      text,
    provider_status   text,                  -- normalized code, safe to keep in clear
    provider_payload_ciphertext bytea,       -- raw provider JSON, under customer DEK -- see §4.4
    UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)
) PARTITION BY RANGE (occurred_at);

CREATE INDEX ON comms_event (customer_id, occurred_at);

-- Bootstrap partitions (T-009 decision 2): T-014's create-ahead job
-- doesn't exist yet. Creates the current and next calendar month for
-- both partitioned tables, computed when this migration actually runs
-- (i.e. at tenant provisioning time). T-014 takes over from here.
DO $$
DECLARE
    i           int;
    month_start date;
    month_end   date;
    suffix      text;
BEGIN
    FOR i IN 0..1 LOOP
        month_start := (date_trunc('month', now()) + (i || ' months')::interval)::date;
        month_end   := (month_start + interval '1 month')::date;
        suffix      := to_char(month_start, 'YYYY_MM');

        EXECUTE format(
            'CREATE TABLE %I PARTITION OF comms_request FOR VALUES FROM (%L) TO (%L)',
            'comms_request_' || suffix, month_start, month_end
        );
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF comms_event FOR VALUES FROM (%L) TO (%L)',
            'comms_event_' || suffix, month_start, month_end
        );
    END LOOP;
END $$;
```

No new runner needed — `provision_tenant` already runs `sqlx::migrate!("./migrations/tenant")`
against every tenant; an existing dev tenant picks this up on its next (idempotent)
re-provision.

#### Task 2 — Integration tests

Add `tests/ledger_outbox_schema.rs`, following `tests/customer_dek.rs`'s conventions exactly
(`unique_name`, `control_database_url`, `vault_keystore`, real `provision_tenant` +
`connect_tenant_pool`, `drop_test_tenant`-style best-effort cleanup). No model/repo layer
(decision 5) — every query in this file is raw `sqlx::query`/`query_as`. Cover:

1. **Bootstrap partitions exist.** `SELECT count(*) FROM pg_inherits WHERE inhparent =
   'comms_request'::regclass` and the same for `comms_event` both return `2` immediately after
   provisioning.
2. **Ledger insert into a bootstrapped partition succeeds.** Insert a `comms_request` row with
   `created_at = now()`; a `SELECT` by `(created_at, id)` round-trips it.
3. **Ledger insert outside any bootstrapped partition fails.** Insert a `comms_request` row with
   `created_at = now() + interval '3 months'`; assert the `INSERT` errors (no partition covers
   that range) rather than silently landing somewhere.
4. **Idempotency key is globally unique.** Insert one `idempotency` row; a second `INSERT` with
   the same `key` (different `comms_request_id`) violates the primary key.
5. **Outbox claim query respects `SKIP LOCKED`.** Insert two `outbox` rows for the same
   `channel`, both `next_attempt_at <= now()` and `leased_until IS NULL`. In one connection,
   open a transaction and `SELECT ... FOR UPDATE` one row without committing. From a second
   connection, run the §4.2 claim query (`UPDATE ... WHERE channel = $1 AND next_attempt_at <=
   now() AND leased_until IS NULL ORDER BY priority, next_attempt_at LIMIT 1 FOR UPDATE SKIP
   LOCKED RETURNING *`) and assert it returns the *other* (unlocked) row, proving a row held by
   an in-flight dispatcher is skipped rather than blocked on or double-claimed.
6. **`comms_event` uniqueness.** Insert one event; a second insert with an identical
   `(occurred_at, comms_request_id, event_type, provider_ref)` tuple violates the `UNIQUE`
   constraint, but changing `provider_ref` succeeds.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/ledger_outbox_schema.rs
```

Then verify the bootstrap partitions directly against a freshly provisioned tenant:

```
just provision acme eu tenant_acme operator@example.com
psql postgres://messgr:messgr@localhost:5432/tenant_acme -c "\d+ comms_request"
psql postgres://messgr:messgr@localhost:5432/tenant_acme -c "\d+ comms_event"
```

Expected: both show two partitions attached (current and next calendar month, named
`<table>_<YYYY>_<MM>`); `outbox` and `idempotency` are plain (non-partitioned) tables.

### Docs update (mandatory when user-facing)

No user-facing surface — no CLI, no README/justfile change. No `DESIGN.md` change expected:
this implements §4.1–§4.4 as written, including its own tenant_id/FK omissions. The bootstrap
partition strategy (decision 2) is a migration-tooling detail, not a documented schema
property, so it is recorded only here and in the migration's own header comment. If
implementation forces a deviation from §4.1–§4.4's schema itself, stop and raise it rather than
editing the design to match the code.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. No docs to update per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(ledger): add comms_request/outbox/comms_event/idempotency schema (T-009)

   Adds the fourth tenant migration (DESIGN.md §4.1-4.4): the monthly
   RANGE-partitioned comms_request ledger and comms_event log, the
   unpartitioned outbox and idempotency tables, and their indexes,
   matching the design's own CREATE TABLE statements verbatim -- no
   tenant_id or FK beyond what the design itself declares. Bootstraps
   the current and next calendar month as real partitions since T-014's
   create-ahead job doesn't exist yet. Schema only: no model, repo, or
   CLI code, since nothing writes to these tables until ingest (T-011)
   and the dispatcher (T-013).
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration / tests is a natural split) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a) — no user-facing surface shipped; confirmed no README/justfile/DESIGN.md/CHANGELOG.md edit is needed or missing
- [x] Docs-readability pass — skipped: no `.md`/`.adoc` files changed by this ticket
- [x] Findings recorded with severity, class, and disposition; disposition summary + cost line below (step 5)
- [x] Ticket moved to `tickets/6-done/` or `tickets/5-rework/`; `## History` appended (step 6)
- [x] Remaining-tickets impact sweep done (step 8) — re-read T-010, T-011, T-014 (the only `1-to-do/` tickets depending on T-009); all three describe the schema only in generic terms ("the comms_request column", "ledger/outbox schema", "the partitioned comms_request table") that still hold exactly as shipped. No patch needed.

**Independent verification**, checked out `feat/T-009-ledger-outbox-schema` directly rather than trusting the implementation's self-report: re-ran `just fmt`/`just lint`/`just test` on the branch — clean, 6/6 in `tests/ledger_outbox_schema.rs`, 51 tests total, no regressions. Diffed the branch against `main` (`git diff main...feat/T-009-ledger-outbox-schema --stat`): exactly the two files the plan's two tasks name, nothing else. Line-by-line compared the migration's `CREATE TABLE` statements against DESIGN.md §4.1–§4.4 — verbatim match, including the deliberate omissions (no `tenant_id` on `outbox`/`comms_event`/`idempotency`, no `REFERENCES` clause anywhere). Independently provisioned a fresh tenant (`t009rvw2`) and confirmed via `psql`/`docker exec`: `comms_request` and `comms_event` each show exactly 2 partitions (`<table>_2026_08`, `<table>_2026_09`); `outbox` and `idempotency` are plain tables. Grepped `src/`/`tests/` project-wide: no Rust module references these tables outside the new test file, confirming decision 5 (no model/repo/CLI) was honoured. Cleaned up the review tenant afterward.

Project-wide search for `comms_request`/`outbox`/`comms_event`/`idempotency` found no stale reference this branch should have updated — every hit outside the new files is in `PLAN.md`/`AGENTS.md`/`DESIGN.md`, all still accurate.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | test-gap | noted | Task 2 item 1 verifies the next-month bootstrap partition *exists* (`pg_inherits` count), but no test actually inserts a row with `created_at` inside that second partition's range — only the current-month partition is exercised by a real `INSERT`. The month-boundary arithmetic in the migration's `DO` block (`month_start`/`month_end` for `i=1`) is therefore unverified by data, only by catalog metadata. | `tests/ledger_outbox_schema.rs` — `insert_into_a_bootstrapped_partition_round_trips` only inserts at `Utc::now()` | Not worth a dedicated ticket: the boundary math is identical for both loop iterations (same formula, `i` substituted), so a bug there would almost certainly show up as a wrong partition *count* or *range* too, both of which the existing `bootstrap_partitions_exist_for_current_and_next_month` test and the manual `\d+` walkthrough already cover. Fold into T-014's own test suite when it's refined, since T-014 owns ongoing partition creation and will need this coverage anyway. |

Disposition summary: 1 noted (F1). No blocking findings.

cost: estimated L, actual M — the confirmed decision to ship schema-only (no Rust model/repo/CLI, since nothing consumes these tables until T-011/T-013) cut this from the multi-session effort the original `L` grade assumed to about one session.

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-08-31 — TO DO → READY: plan complete
- 2026-08-31 — READY → IN DEVELOPMENT: picked up
- 2026-08-31 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-08-31 — IN REVIEW → DONE: review clean: 1 noted (F1); no blocking findings
- 2026-08-31 — merged to main (PR #11, 65c5e70)
