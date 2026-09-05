---
id: T-024
title: CI check that every PII-holding table is covered by erasure or a named exemption
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-024 — CI check that every PII-holding table is covered by erasure or a named exemption

## Outcome

CI fails if a table holding customer-linkable data exists that is neither in §7.2's physical-redaction statements nor on a named, reasoned exemption list checked in alongside the check itself. This is DESIGN.md's own §14/§7.2 CI requirement and PLAN.md's former T-045, scoped narrowly to the check itself — **not** a pull-forward of the full erasure feature (crypto-shred command, `erasure_request` table, physical-redaction job), which stays at build order step 15 behind gates and consent that don't exist yet.

## Description

This audit found `suppression` silently outside §7.2's erasure statements — not a bug, since `suppression` has no `customer_id` to erase by and is deliberately destination-scoped rather than customer-scoped (§5's suppression gate needs a recycled number to stay blocked regardless of who currently holds it) — but undocumented, which made it indistinguishable from the `comms_event` omission DESIGN.md already records as a past mistake (§7.2). DESIGN.md now names `suppression` as a stated exemption with its reason.

**Found at refinement: the same detection rule, actually applied to the live schema, flags three more tables neither covered nor exempted.** `customer_dek` (its PK is literally `customer_id`), `customer_alias` (holds a `customer_id` column), and `outbox` (also holds a `customer_id` column) all match "any column referencing `customer_id`" — none are in §7.2's statements, none are named exemptions, and `suppression` (the only exemption named before this refinement) doesn't even exist in the live schema yet (a future ticket, step 5). User-confirmed resolution: all three get their own named exemption in §7.2, matching `suppression`'s style — reasoned individually, not folded into one blanket rule:

- `customer_dek` — destroying `wrapped_dek` (setting `shredded_at`) is Mode 1's own crypto-shredding mechanism (§7.1); Mode 2's physical redaction doesn't also need to touch it, since the ciphertext columns it would unlock are already overwritten by that same redaction.
- `customer_alias` — holds only two opaque customer-id UUIDs and a merge timestamp; no PII-bearing value to redact (§7.2 already states a customer id itself "carries no personal information").
- `outbox` — `customer_id` here is routing/claim metadata; the actual message content lives in `comms_request`, not in `outbox`.

The mechanical check itself does not yet exist and doesn't need the full erasure feature built to be useful: it can run today, against whatever schema exists, and keep working as tables are added. Scope:

1. A CI-runnable check (a `just` recipe wrapping a Rust integration test, following this codebase's existing schema-truth-testing convention rather than a shell/psql script) that inspects the live tenant-database schema for columns holding customer-linkable data (by convention: any column literally named `customer_id`, or matching `*_ciphertext`/`*_hmac`/`*_raw`) and confirms each such table appears in a covered list (a stand-in for `src/erasure`'s eventual redaction statements — that module is not created by this ticket) or on an explicit, reasoned exemption list — not a bare table-name allowlist, which is what let `suppression`'s absence go unnoticed as an omission rather than a decision.
2. Since no `src/erasure` module exists yet, this ticket's concrete target is all nine tables the detection rule currently matches against the live schema: `customer_address`, `customer_external_id`, `comms_request`, and `comms_event` (covered — §7.2 already names these four) plus `suppression`, `customer_dek`, `customer_alias`, `outbox`, and `orphan_event` (exempt, each with its own stated reason — `suppression`'s already in DESIGN.md; the other four are added by this ticket, see above). The check must fail today if run against the current schema with an empty implementation, and pass once all nine are correctly classified.
3. Building the actual crypto-shred and physical-redaction commands (`erasure_request` table, the throttled background job, `VACUUM` afterward) remains step 15 in the build order and is explicitly out of scope for this ticket — it depends on consent/suppression (step 5) and the customer projection being live in production data, neither of which changes here.

**Correction, T-022 review (2026-09-04): `orphan_event` (T-022, DESIGN.md §4.4) is a fifth exempt table, and the detection rule needed widening to see it.** `orphan_event.provider_payload_raw` is unencrypted third-party webhook payload with no `customer_id` column — the same structural shape as `suppression`, but the original detection rule (`customer_id`/`*_ciphertext`/`*_hmac`) does not match its column name at all, so the table would silently never appear in the query's result set. Adding it to `EXEMPT` alone would have made Task 2's second assertion (every `EXEMPT`/`COVERED` name must actually appear in the schema-check's result) fail, since an unmatched exemption is indistinguishable from a stale one under that assertion. The rule now also matches `*_raw`, which today only newly catches this one column (verified: no other tenant-schema column matches `%_raw` or `%_payload%`).

- `orphan_event` — holds unmatched provider delivery-receipt payloads with no `customer_id` column yet (reconciliation, which would resolve one, is out of scope per T-022 decision 5); like `suppression`, it is structurally exempt rather than covered.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-024-erasure-tooling-with-a-mechanical-ci-check-for-pii-table-coverage
```

Local WIP commits as you go. Do not push or open a merge request without explicit user
approval (publish-gated, root-path child — tidy WIP into atomic commits before presenting,
rules §0/§4 item 7).

### Prerequisite gate (hard)

None. `depends-on: []`.

### Confirmed design decisions (do not deviate without asking)

1. **This ticket does not create `src/erasure`.** The check's "covered" list is a hardcoded
   stand-in for §7.2's real redaction statements, living inside the test file itself — not a
   new production module. Building the real erasure feature is build-order step 15, a separate,
   future ticket (Description item 3).
2. **The concrete target is 9 tables**, not 5 (Description items 1–2, user-confirmed at
   refinement; widened from 8 to 9 at the T-022 review, see Description's correction note):
   covered — `comms_request`, `comms_event`, `customer_address`,
   `customer_external_id`; exempt — `suppression`, `customer_dek`, `customer_alias`, `outbox`,
   `orphan_event`, each with its own stated reason.
3. **Detection rule:** any tenant-database table with a column named exactly `customer_id`, or
   matching `%_ciphertext` / `%_hmac` / `%_raw`, via `information_schema.columns` against a real
   provisioned tenant database — not static SQL-file parsing, matching this codebase's existing
   schema-truth-testing convention (`tests/ledger_outbox_schema.rs` et al.). The `%_raw` pattern
   was added at the T-022 review specifically to catch `orphan_event.provider_payload_raw`.
4. **Implemented as a Rust integration test** (`tests/erasure_coverage.rs`), reusing the
   `TestTenant` provisioning pattern each test file in this repo already defines locally (no
   shared test-helpers module exists to reuse instead — matches the established convention of
   `tests/ledger_outbox_schema.rs`, `tests/kill_switch.rs`, etc., each defining their own copy).
   A `just erasure-coverage-check` recipe wraps `cargo test --test erasure_coverage` for a
   documented, directly-runnable entry point. No `.github/workflows/ci.yml` change is needed —
   CI's one `test` job already runs a bare `cargo test`, which picks up any new integration test
   file automatically.
5. **`suppression` isn't in the live schema yet** (a future ticket, build step 5) — it stays on
   the exemption list regardless, so the check does not need touching again the day it ships;
   the test only evaluates tables that exist in the schema at run time, so `suppression`'s
   absence today is not itself a failure.

### Tasks

#### Task 1 — DESIGN.md §7.2 correction (`development/design/06-pii-retention.md`)

`orphan_event`'s exemption paragraph already exists (added by T-022's rework, currently line 47,
immediately after `suppression`'s at line 45) — only three new paragraphs are needed now.
Immediately after the existing `orphan_event` paragraph (currently line 47), add three new
paragraphs in the same style (bold lead sentence naming the table and the decision, then the
reasoning), for `customer_dek`, `customer_alias`, and `outbox` — using the bullet points already
drafted in this ticket's Description as the basis for the prose. Update the closing sentence
following the exemption paragraphs (or add one) so it reads as "these five tables" rather than
singling out `suppression` alone, since all five are now named exemptions the CI check must
carry.

#### Task 2 — `tests/erasure_coverage.rs` (new file)

- Add a `TestTenant` provisioning helper, copied from `tests/ledger_outbox_schema.rs`'s exact
  shape (`provision`/`teardown`, `unique_name`, `drop_test_tenant`).
- Define the manifest as plain consts:
  ```rust
  const COVERED: &[&str] = &["comms_request", "comms_event", "customer_address", "customer_external_id"];
  const EXEMPT: &[(&str, &str)] = &[
      ("suppression", "destination-scoped, not customer-scoped — a recycled number must stay blocked regardless of who currently holds it (DESIGN.md §5, §7.2)"),
      ("customer_dek", "destroying wrapped_dek is Mode 1's own crypto-shredding mechanism; Mode 2 doesn't also need to touch it (§7.1, §7.2)"),
      ("customer_alias", "holds only opaque customer-id UUIDs and a merge timestamp, no PII-bearing value to redact (§7.2)"),
      ("outbox", "customer_id here is routing/claim metadata; message content lives in comms_request, not here (§4.2, §7.2)"),
      ("orphan_event", "unmatched provider payload with no customer_id column yet; reconciliation, which would resolve one, is a separate ticket (§4.4, §7.2)"),
  ];
  ```
- Write `#[tokio::test] async fn every_customer_linkable_table_is_covered_or_exempt()`: provision
  a `TestTenant`, query
  `SELECT DISTINCT table_name FROM information_schema.columns WHERE table_schema = 'public' AND (column_name = 'customer_id' OR column_name LIKE '%\_ciphertext' ESCAPE '\' OR column_name LIKE '%\_hmac' ESCAPE '\' OR column_name LIKE '%\_raw' ESCAPE '\')`
  against `tenant.tenant_pool`, and assert every returned table name is in `COVERED` or the
  `EXEMPT` names — failing with the list of unclassified table names and a pointer to add them
  to one list or the other, if not. Tear down the tenant afterward.
- Add a second, cheap assertion in the same test (or a second test): every name in `COVERED`
  and every name in `EXEMPT` is actually a table the schema check returned — catches a stale
  manifest entry (a renamed/dropped table) drifting silently, the same class of problem this
  ticket exists to prevent in the other direction.

#### Task 3 — `justfile`

Add, near the other `[group('docs')]`/check recipes:
```just
# Fail if a customer-linkable table exists that is neither covered by an
# erasure redaction statement nor a named, reasoned exemption (DESIGN.md §7.2)
[group('docs')]
erasure-coverage-check:
    cargo test --test erasure_coverage
```

### Acceptance test

```
just build
just lint
just erasure-coverage-check
just test
just docs-check
```

Confirm the check actually catches an unclassified table before considering it done: temporarily
comment out one `EXEMPT` entry (e.g. `outbox`), re-run `just erasure-coverage-check`, confirm it
fails naming exactly that table, then restore it and confirm a clean pass — proving the negative
case Description item 2 requires, not just the positive one.

### Docs update (mandatory when user-facing)

`development/design/06-pii-retention.md` per Task 1 (not user-facing in the product sense, but
is the design document itself, which this ticket is directly correcting). No `docs/user-manual/`
change — this check has no CLI or operator-facing surface.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. Docs updated per Task 1.
3. Write a summary: files touched, decisions honoured (especially decision 2 — the target list
   grew from 5 to 8 tables during refinement, then to 9 at the T-022 review), anything deferred.
4. Suggest a Conventional Commit message, e.g.:
   ```
   test(erasure): add CI check for PII-table erasure coverage (T-024)
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

- [x] Reviewer independence settled (step 0): the implementing session authored the branch, so
  audits (steps 2–4a) were delegated to an independent sub-agent, briefed adversarially; every
  delegated finding below was re-verified by hand before recording.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2).
  `just build`/`lint`/`erasure-coverage-check`/`test`/`docs-check` all green on `main` at the
  merge commit. All 9 originally-targeted tables correctly classified; the negative-case
  mutation test (un-exempt `outbox`, confirm the exact failure, restore, confirm clean pass) was
  independently re-run and passed. The independent reviewer additionally mutation-tested the
  second (stale-manifest) assertion, not just the first — renaming `orphan_event`→`template` in
  `EXEMPT` correctly failed naming `["template"]` — confirming neither assertion is tautological.
  Two deviations from the plan's literal Task 2 text, both caught by the acceptance test itself
  before this review and already recorded inline in `## History` below (partition resolution via
  `pg_inherits`; the stale-manifest check distinguishing "not built yet" from "renamed/dropped").
- [x] Quality audit (step 3) — idiomatic, no dead code; both assertions proven capable of failing
  (above). CI/justfile command parity independently checked (review-addendum step 2.8): `ci.yml`'s
  `test`/`clippy`/`fmt` jobs invoke the same commands as `just test`/`just lint`/`just fmt-check`
  verbatim.
- [x] Consistency audit (step 4) — no caller/callee drift; `tenant_id`/`customer_id` naming
  consistent with §2.1; all `§N` citations this diff added (§5, §7.1, §4.2, §4.4) checked against
  the cited sections and correct.
- [x] Documentation audit (step 4a) — `just docs-check` clean; `docs/` grepped, confirmed no
  user-manual coverage is needed (no CLI/operator surface, as the ticket claims).
- [x] Docs-readability pass (step 4b) — no docs-readability reviewer available in this host
  environment; conscious skip.
- [x] Findings recorded below with severity, class, and disposition (step 5).
- [x] Ticket moved `4-in-review/` → `5-rework/` → `4-in-review/` after the scoped fix;
  `## History` appended at each move (step 6).
- [x] Other references reconciled: `development/design/13-build-order.md`'s §14 exemption count
  (F2, fixed inline, twice — once for this ticket's original three additions, again for F1's
  sixth). Governing docs otherwise consistent (step 7).
- [x] Remaining-tickets impact sweep (step 8): no ticket in `1-to-do/`/`2-ready/` references
  T-024 or depends on it; `T-030` (orphan_event reconciliation) doesn't assume anything this
  ticket changed.
- [x] Summary + commit message & MR attributes presented for approval; remote-base check applies
  under `layout = "in-tree"` (step 9).

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | plan-wrong | — | `customer` (the root customer entity: `locale`, `timezone`, `source_system`, `source_updated_at`) is invisible to decision 3's column-name detection rule — its own PK is `id`, not `customer_id`, and it has no `*_ciphertext`/`*_hmac`/`*_raw` column — so it shipped classified in neither `COVERED` nor `EXEMPT`, silently defeating the check's own purpose (AGENTS.md invariant 6). | `migrations/tenant/0008_customer_projection.sql:9-17`; `development/design/03-data-model.md:238-246`; pre-fix `tests/erasure_coverage.rs:181-196` matched no column on `customer` | Resolve the table via the `customer_id` foreign keys other tables declare against it, not by its own columns |
| F2 | non-blocking | stale-xref | fixed inline | `development/design/13-build-order.md`'s §14 checklist mirrored the exemption list and went stale twice: first when this ticket's original diff added 3 exemptions without updating it, again when F1's fix added a 6th. | `development/design/13-build-order.md:34` vs `development/design/06-pii-retention.md` §7.2 | Keep the mirrored count current at every future exemption addition |
| F3 | non-blocking | noted | noted | `TestTenant::teardown` only runs on the success path — a panicking `assert!` (the exact shape this ticket's own required mutation test produces) leaks a Vault mount, a tenant DB, and control-DB rows. Byte-identical to `tests/ledger_outbox_schema.rs`'s helper (decision 4's confirmed convention) — pre-existing and systemic across every integration test file using this pattern, not introduced by this ticket. | `tests/erasure_coverage.rs:32-72`, identical to `tests/ledger_outbox_schema.rs`'s helper | A future ticket scoped to the shared `TestTenant` pattern itself (not this one) could add panic-safe cleanup (e.g. a drop guard) |

Disposition summary: 1 blocking (F1, fixed — see rework record below), 1 fixed inline (F2), 1 noted (F3).

cost: estimated M, actual M

### Rework fix record — round 1 (commit d2583aa)

Fixed F1 by widening `customer_linkable_tables`'s query with a `UNION` resolving any table
referenced by a `customer_id`-named foreign key (surfaces `customer` via
`customer_address`/`customer_external_id`/`customer_alias`'s own declared constraints). Added
`customer` to `EXEMPT` — user-confirmed classification: locale/timezone are operational
preferences, source_system/source_updated_at are sync metadata, none of it personal information
— and a sixth named-exemption paragraph in `development/design/06-pii-retention.md` §7.2. Also
folds in F2's fix (`development/design/13-build-order.md`'s exemption count, bumped a second
time to six). Verified: mutation test (removing `customer` from `EXEMPT`) fails naming exactly
`["customer"]`; restored and confirmed clean pass. Full acceptance test
(`build`/`lint`/`erasure-coverage-check`/`test`/`docs-check`) green.

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found suppression undocumented as an erasure exemption; re-specs PLAN.md's former T-045 (CI check against the live schema) narrowly, without pulling the full erasure feature forward from build step 15.
- 2026-09-04 — TO DO → READY: implementation plan complete. Target list expanded from 5 to 8 tables during refinement, at the user's direction, after applying the check's own detection rule to the live schema found customer_dek/customer_alias/outbox also unclassified — each now gets its own named exemption in DESIGN.md §7.2, matching suppression's style.
- 2026-09-04 — TO DO → READY: plan complete
- 2026-09-04 — plan corrected (still READY): T-022's review (impact sweep, step 8) found T-022's own `orphan_event` table would go undetected by this ticket's detection rule (no `customer_id`/`*_ciphertext`/`*_hmac` column) and would fail Task 2's second assertion if merely added to `EXEMPT`. Widened the detection rule to also match `*_raw`, added `orphan_event` as a fifth named exemption (target now 9 tables, not 8), and updated Task 1/2 and decisions 2–3 accordingly. Folded per T-022's review finding F2 — no severity of its own on this ticket, since T-024 hasn't been picked up yet.
- 2026-09-05 — plan amended inline: pickup applicability audit found T-022's rework had already landed the `orphan_event` exemption paragraph directly in `development/design/06-pii-retention.md` (commit a5caad4), which Task 1's text hadn't caught up to. Task 1 now adds three new paragraphs (`customer_dek`, `customer_alias`, `outbox`), positioned after the existing `orphan_event` paragraph, not four. Non-blocking — schema, detection rule, 9-table target, and supporting test infra all confirmed still accurate.
- 2026-09-05 — READY → IN DEVELOPMENT: picked up
- 2026-09-05 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-05 — IN REVIEW → REWORK: F1 blocking: customer table invisible to detection rule
- 2026-09-05 — REWORK → IN REVIEW: F1 fixed (commit d2583aa): customer table now caught via customer_id FK resolution
