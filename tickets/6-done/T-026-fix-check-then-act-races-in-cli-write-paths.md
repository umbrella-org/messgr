---
id: T-026
title: Fix check-then-act races in CLI write paths
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-026 — Fix check-then-act races in CLI write paths

## Outcome

Three known check-then-act races in single-operator CLI write paths are closed: a rejected
template approval always writes its `platform_audit` row even under concurrent callers, a
concurrent producer registration never fails with a raw database error, and a concurrent
tenant-config set always audits an accurate `created`/`updated`/`idempotent` outcome instead of
letting both racers claim `created`. The tolerance that let these stand as `noted` —
"single-operator CLI, no concurrent-caller expectation" — is recorded as expired for these
three now that the admin panel and platform console (build steps 14, 19) will remove that
assumption. The fourth instance this ticket was filed against (T-006/F3, dev-PKI root
generation) is not fixed here — see the refinement note below — but stays batched with the
other three as a documented, accepted exception rather than being split into its own ticket.

## Description

Three reviews independently found the same shape of bug and each time judged it not worth a dedicated ticket on its own — correctly, at the time, since messgr had no concurrent-caller story yet. Batched together, four instances of the same pattern across three tickets passed the promotion test:

1. **T-010/F1** — `approve_template_inner`'s check-then-insert (`repo::find` then, only if `None`, `repo::insert`) is not atomic. Two concurrent `template approve` calls for the same `(template_id, version, locale)` can both pass the existence check; the loser's `INSERT` then fails on the primary-key constraint as a raw `ApproveError::Database`, skipping the `rejected`-audit branch entirely — contradicting decision 6's "every approve call, including a rejected one, writes exactly one row" (`src/template/approve.rs`).
2. **Same shape, `producer::register::register_producer_inner`** (`find_by_name` then `insert`) — named in T-010's own review as an existing instance, not previously flagged.
3. **Same shape, `tenant_config::configure::set_tenant_config_inner`** — also named in T-010's review as a pre-existing instance. Unlike #1 and #2, `repo::upsert` is already `ON CONFLICT (singleton) DO UPDATE`, so the DB write itself never fails or loses data under a race — the actual bug is that `set_tenant_config_inner` decides its `created`/`updated`/`idempotent` audit outcome from a `load` taken *before* the race, so two concurrent first-time callers can both read no row, both compute `created`, and both audit `created` even though only one of them actually created the row.
4. **T-006/F3** — the dev-PKI root-CA-generation race (`has_issuer` check, then `cert::ca::generate`) is not mitigated the way the equivalent mount-enable race is; two concurrent `bootstrap` callers that both observe no issuer yet can each generate a root, since Vault's PKI engine allows multiple issuers per mount with no "already exists" error to catch. `bootstrap`'s own doc comment claims a guarantee ("never mints a second root CA") that isn't held under concurrency (`src/producer/dev_pki.rs`).

**Correction made during this ticket's refinement (2026-09-07):** #4's premise was stale. Commit
`0c331de` ("fix(producer): serialize dev PKI root CA bootstrap to close TOCTOU race (T-006)",
PR #7) landed on main 2026-08-31 — the same day T-006 merged, off-ticket, with no ticket
recording it — and added `ROOT_CA_BOOTSTRAP_LOCK`, a `tokio::sync::Mutex` serializing
`has_issuer`+`generate` calls within one process. That closes the race this codebase's own test
suite can trigger (many `#[tokio::test]` tasks sharing one process and one dev Vault), but does
**not** close the race #4 is actually filed against: two separate `messgr-control dev-pki
bootstrap` **process** invocations, which do not share a `static` Mutex. That race is still
open.

Closing it for real needs a lock outside the process — e.g. a Postgres advisory lock — which
means passing `control_pool` into `dev_pki::bootstrap` for the first time; today it is
Vault-only tooling with no database dependency at all. Weighed against that cost: `dev-pki` is
guarded to `profile = dev` (panics otherwise, see `assert_dev_profile`), carries no
`platform_audit` guarantee the way #1-#3 do, and losing the race just leaves a harmless orphaned
extra root CA — Vault always issues leaf certs off whichever root is currently the mount's
default issuer, so a stray second root doesn't break `issue_cert`/`issue_server_cert`, it's just
inert clutter in a dev Vault. **Decided: accept this as-is.** #4 is closed by documentation, not
code — see Task 4 below. Re-open if a fifth instance of this pattern surfaces (per the original
filing's own re-open condition) or if `dev-pki bootstrap` ever grows a real reason to take a
database dependency.

Fix #1-#3 with the pattern `ensure_pki_mount` (`src/producer/dev_pki.rs`) already uses for its
own, already-closed mount-enable race: make the write itself idempotent (`ON CONFLICT DO
NOTHING`, matching `src/customer/resolve.rs`'s and `src/customer_dek/repo.rs`'s own
`insert_if_absent`/`insert_if_absent_tx` race-handling shape for provisional customers/DEKs),
and reclassify from what actually landed rather than from a stale pre-write read. #1's fix must
specifically preserve the "always writes exactly one `platform_audit` row" guarantee on the
losing side, not just avoid the panic.

This tolerance was accepted under a single-operator assumption. The admin panel (step 14) and
platform console (step 19) both introduce multiple concurrent human operators; re-open this
class of finding if either surfaces a fifth instance.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-026-check-then-act-races
```

### Prerequisite gate (hard)

None. `depends-on: []`, no unmerged branch this ticket builds on.

### Confirmed design decisions (do not deviate without asking)

1. **#1-#3 are fixed by making the write authoritative, not by re-checking before it.** Each
   `_inner` function stops trusting a pre-write `SELECT`/`load` to decide the outcome; it
   attempts the write first (`ON CONFLICT DO NOTHING`), and only falls back to a re-read when
   that write reports it changed nothing. This removes the TOCTOU window entirely instead of
   narrowing it.
2. **#4 (dev-PKI root race) is documented, not fixed, in this ticket.** See the Description's
   2026-09-07 refinement note for the full reasoning: the in-process race is already closed by
   `ROOT_CA_BOOTSTRAP_LOCK` (commit `0c331de`); the remaining cross-process race is accepted as
   a harmless orphaned-root risk given `dev-pki` is dev-only tooling with no audit guarantee and
   no existing database dependency worth adding solely for this. Task 4 records that decision in
   the code and in this ticket; no behaviour changes.
3. **`producer::repo::insert_if_absent` uses an untargeted `ON CONFLICT DO NOTHING`** (no
   `(column)` target) because a losing insert can violate either the `name` or the
   `cert_subject` `UNIQUE` constraint on `producer` (`migrations/tenant/0001_producer.sql`) and
   the caller cannot know which in advance. On a loss, `register_producer_inner` re-runs its
   existing classification checks (`find_by_name`, `find_by_cert_subject`,
   `cert_repo::find_producer_cert`) exactly once more — those checks are what already
   distinguish "idempotent re-registration" from every rejection case today, and a losing insert
   guarantees one of them will now find the winner's row. No retry loop: a second insert attempt
   is never made, since the reclassification always terminates in either the idempotent success
   path or one of the existing `Err(rejected(...))` returns.
4. **Tenant-config's race is closed with a transaction-scoped Postgres advisory lock, not
   `ON CONFLICT`.** `repo::upsert` is already `ON CONFLICT (singleton) DO UPDATE`, so the write
   itself is safe; the bug is that `load` (the outcome-classifying read) can run before a
   concurrent racer's write commits, on the very first configure of a tenant when no row yet
   exists for a `SELECT ... FOR UPDATE` to lock. `set_tenant_config_inner` therefore opens a
   transaction, calls `pg_advisory_xact_lock(hashtext('tenant_config'))` before `load`, then
   `load`+decide+`upsert` all inside that same transaction, then commits — serializing every
   concurrent `set` for this tenant regardless of whether a row exists yet. The lock is
   transaction-scoped (`_xact_lock`, not session-scoped) so it always releases on commit or
   rollback with no separate release call needed. Safe against cross-tenant collision because
   each tenant already has its own database (hard invariant 8) and Postgres advisory locks are
   scoped per-database, not per-cluster.

### Tasks

#### Task 1 — `src/template/approve.rs`, `src/template/repo.rs`

Add `repo::insert_if_absent` (`INSERT ... ON CONFLICT (template_id, version, locale) DO NOTHING`,
returning `Result<bool, sqlx::Error>` from `rows_affected() == 1`, matching
`customer_dek::repo::insert_if_absent`'s shape). In `approve_template_inner`, delete the
pre-insert `repo::find` existence check and call `repo::insert_if_absent` directly: `true` ->
audit `created`, return `Ok`; `false` -> audit `rejected` (identical message to today's
already-approved rejection), return `Err(ApproveError::Rejected(...))`. Leave `repo::find`
itself in place — `show_template` still uses it.

#### Task 2 — `src/producer/register.rs`, `src/producer/repo.rs`

Add `repo::insert_if_absent` (`INSERT ... ON CONFLICT DO NOTHING`, untargeted per confirmed
decision 3, returning `Result<bool, sqlx::Error>`). Factor `register_producer_inner`'s existing
classification block (the `find_by_name` idempotent/conflicting-inputs branches, the
`find_by_cert_subject` branch, the cross-tenant `find_producer_cert` branch) into a helper,
`classify_registration`, returning an enum/`Result` the caller matches on. Call it once at the
top as today; if it says "proceed to insert," call `repo::insert_if_absent`. If that returns
`true`, continue exactly as today (write the cert mapping, audit `created`). If it returns
`false` (lost the race between classification and insert), call `classify_registration` a
second time — it is now guaranteed to return one of the existing early-return branches — and
return whatever it yields, verbatim, including that branch's own audit call. Never call
`repo::insert_if_absent` more than once.

**Amended during implementation (2026-09-07):** `classify_registration`'s own two checks
(`find_by_name`, `find_by_cert_subject`) must run inside one `REPEATABLE READ` transaction, not
as two independent pool queries. The acceptance test below caught this: under the default `READ
COMMITTED`, each check is a separate statement with its own snapshot, so a concurrent
registration's commit landing between the two awaits let `find_by_name` miss a just-inserted row
that `find_by_cert_subject` then found — producing a false "cert_subject already bound to a
different producer" rejection for what was actually the same producer under identical inputs.
Added `repo::find_by_name_tx`/`repo::find_by_cert_subject_tx` (same `_tx` convention as Task 3)
so both checks share one snapshot.

#### Task 3 — `src/tenant_config/configure.rs`, `src/tenant_config/repo.rs`

Add `repo::load_tx`/`repo::upsert_tx` (`&mut PgTransaction<'_>`-taking variants of `load`/
`upsert`, same SQL, matching `customer_dek::repo::insert_if_absent_tx`'s convention). Rewrite
`set_tenant_config_inner` to open a transaction on `tenant_pool`, execute
`SELECT pg_advisory_xact_lock(hashtext('tenant_config'))` against it, then `repo::load_tx`,
decide the outcome exactly as today, conditionally `repo::upsert_tx`, commit the transaction,
then audit against `control_pool` exactly as today (the audit call stays outside the tenant
transaction — it already writes to a different database).

#### Task 4 — `src/producer/dev_pki.rs` (documentation only)

Update `bootstrap`'s doc comment: it currently claims "re-running never mints a second root CA
or duplicates the mount" unconditionally. Narrow that claim to same-process callers, and add a
paragraph (near `ROOT_CA_BOOTSTRAP_LOCK`'s existing doc comment) recording: two separate
`messgr-control dev-pki bootstrap` process invocations racing `has_issuer`+`generate` can still
each mint a root; this is accepted rather than fixed because `dev-pki` is dev-only tooling
(`assert_dev_profile`), carries no audit guarantee, has no existing database dependency worth
adding solely to close this, and a lost race only leaves a harmless orphaned extra root CA
(Vault always issues off the mount's current default issuer). Cross-reference this ticket by id.
No test — no behaviour changes.

### Acceptance test

Real-stack, `tokio::join!`-based races, matching `tests/customer.rs`'s
`concurrent_address_only_resolution_mints_exactly_one_provisional_customer` convention:

1. `tests/template.rs` — new test `concurrent_approve_calls_for_the_same_version_locale_write_exactly_one_success_and_one_rejected_audit_row`: race two identical `approve_template` calls for the same `(template_id, version, locale)`; assert exactly one `Ok`, one `Err(ApproveError::Rejected(_))`, exactly one row in `template`, and exactly two `platform_audit` rows for this call (`created` + `rejected`) — no raw `ApproveError::Database`.
2. `tests/producer.rs` — new test `concurrent_registration_with_identical_inputs_never_errors_and_writes_no_duplicate`: race two identical `register_producer` calls (same name/cert_subject/owner_team/contact) against a fresh tenant; assert both return `Ok`, exactly one row in `producer`, exactly one row in `producer_cert`, and the two audit rows' outcomes are one `created` and one `idempotent` (never both `created`).
3. `tests/tenant_config.rs` — new test `concurrent_first_time_set_calls_with_identical_input_audit_one_created_and_one_idempotent`: race two identical `set_tenant_config` calls against a tenant with no existing config; assert exactly one row in `tenant_config`, and the two audit rows' outcomes are one `created` and one `idempotent` (never both `created`).
4. `just build`, `just lint`, `just test`, `just docs-check` all clean.

### Docs update (mandatory when user-facing)

No user-facing surface — these are internal correctness/audit-accuracy fixes to existing CLI
commands with unchanged inputs, outputs, and exit codes. `docs/user-manual/control-plane-cli.adoc`
needs no change.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. No docs to update (see above).
3. Write a summary: files touched, decisions honoured (the four confirmed decisions above),
   anything deferred (item #4 — deliberately not fixed, see decision 2).
4. Suggest a Conventional Commit message, e.g.:

   ```
   fix(producer,template,tenant-config): close check-then-act races in CLI write paths (T-026)

   Make the write itself authoritative (ON CONFLICT DO NOTHING) instead of trusting a
   pre-write read, for template approval, producer registration, and tenant-config set.
   Serializes tenant-config's create/update classification with a transaction-scoped
   Postgres advisory lock. Documents, but does not fix, the dev-PKI cross-process root
   race (T-006/F3) as an accepted dev-only exception.
   ```

5. Tidy WIP commits into a small number of atomic commits (root-path child, `path = "."`).
6. Commit locally on the ticket branch. Publish only per the project's commit policy — do not
   push or open a merge request without user approval. Present the commit message; after
   approval, verify the remote base is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` prints nothing), then push and open the
   merge request. Hand back to the user.

## Review

- [x] Reviewer independence settled (step 0): the reviewing agent authored this branch in this
  session, so audits (steps 2-4a) were delegated to an independent, adversarially-briefed
  sub-agent with no memory of writing the code. Every delegated finding was re-verified by hand
  before recording (below) — one delegated claim (tenant-config's shipped test being merely
  "weak" rather than tautological) was independently confirmed by re-running the mutation test
  myself.
- [x] Implementation audit (steps 1, 2): re-ran the acceptance test verbatim —
  `just build`/`just lint`/`just docs-check` clean; `just test` full suite green (all suites,
  including the three new concurrent tests); each of the three new concurrent tests additionally
  run 15x in a loop by the independent reviewer, 45/45 green. All 4 Tasks and all 4 Confirmed
  design decisions verified against the actual diff, in the files named — no gaps.
- [x] Quality audit (step 3): review-addendum's mutation-testing rule applied to every
  concurrency-load-bearing mechanism in the diff — see findings F1-F2 below for what it caught.
- [x] Consistency audit (step 4): `migrations/tenant/0001_producer.sql` confirmed `name` and
  `cert_subject` are both `NOT NULL`, so `insert_if_absent`'s untargeted `ON CONFLICT DO NOTHING`
  is safe against review-addendum step 2 item 2's NULL-dedup hole. `disable_producer_inner`
  (unchanged) still compiles and behaves correctly against the now-dual-variant `find_by_name`.
  Two functions this branch's own refactor orphaned found dead (F3, F4) — `pub` visibility hid
  them from `dead_code` lint.
- [x] Documentation audit (step 4a): `just docs-check` clean. Grepped
  `docs/user-manual/control-plane-cli.adoc` for every behaviour this diff touches — all unchanged
  (error message text byte-for-byte preserved, no enum/error-shape changes), confirming the
  ticket's "no user-facing surface" claim. Task 4's `dev_pki.rs` doc update confirmed present,
  narrowed to same-process callers, cross-referencing T-026.
- [x] Docs-readability pass (step 4b): no docs-readability reviewer available in this
  environment — conscious skip. No `.adoc`/`.md` prose changed by this branch regardless.
- [x] Findings recorded with severity, class, and disposition; disposition summary and cost line
  below (step 5).
- [x] Ticket moved to `tickets/6-done/`; `## History` appended (step 6). Two rework rounds
  preceded this verdict (F2, then F5 found in F2's own scoped re-review) — see the rework fix
  records above and the History below.
- [x] Other references updated if needed; governing documents reconciled (step 7): grepped all
  14 `development/design/*.md` files, `DESIGN.md`, and AGENTS.md's ten invariants for any claim
  about the internal check-then-act vs. write-then-classify shape of `register_producer`/
  `approve_template`/`set_tenant_config` — none exists to reconcile. Confirmed explicitly, not by
  omission.
- [x] Remaining-tickets impact sweep (step 8): no ticket in `1-to-do/` or `2-ready/` references
  T-026 or depends on it — nothing to patch.
- [x] Summary + commit message & MR attributes presented for approval (step 9).

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | test-gap | fixed inline | `concurrent_first_time_set_calls_with_identical_input_audit_one_created_and_one_idempotent` raced only 2 callers. Removing `lock_tx` (the fix under test) still passed this test in 120 consecutive local runs — the interleaving window is too narrow to hit reliably at 2-way concurrency, giving very weak regression protection despite not being tautological (it does fail reliably at higher concurrency). | `tests/tenant_config.rs:234` (pre-fix); mutation test: 0/120 failures at 2-way without `lock_tx`, vs. 8/10 failures at 6-way | Widened to 6 concurrent callers via `tokio::task::JoinSet` |
| F2 | blocking | correctness | — | `register_producer_inner` re-introduces a panic path (`.expect(...)` on a lost-race reclassification returning `None`) one ticket after T-025 converted every `messgr-control` subcommand-body panic to a reported `Err` specifically so a CLI operator never sees a crash (T-025 decision 2: "everything after startup is in scope for panic→Err conversion... `messgr-control`'s subcommand bodies"). Reachable from `Command::Producer(ProducerCommand::Register)` (`src/bin/control.rs:484-497`), which T-025 made `Result`-returning precisely to eliminate this class of crash. Practically near-impossible to trigger (would need a `producer_id` UUID collision, since name/cert_subject collisions are otherwise always caught by reclassification) but contradicts a locked decision regardless. | `src/producer/register.rs:306-309` (pre-fix) | Return `Err(rejected(...))` instead of `.expect(...)`, matching this file's own error-construction convention |
| F3 | non-blocking | design | fixed inline | `producer::repo::find_by_cert_subject` (non-tx) orphaned by this branch's own switch to `find_by_cert_subject_tx` — zero remaining callers repo-wide, `pub` visibility hid it from `dead_code` lint. | `src/producer/repo.rs:43-57` (pre-fix) | Removed; `find_by_cert_subject_tx`'s doc comment updated to note it has no non-tx sibling |
| F4 | non-blocking | design | fixed inline | `tenant_config::repo::upsert` (non-tx) orphaned the same way by the switch to `upsert_tx` — zero remaining callers repo-wide. | `src/tenant_config/repo.rs:64-96` (pre-fix) | Removed; `upsert_tx`'s doc comment absorbed the explanation of the created/updated/idempotent contract |
| F5 | blocking | correctness | — | Round 1's scoped re-review of F2's fix found the new `Err(rejected(...))` branch it added returns with no preceding `audit(...)` call, contradicting this same function's own documented invariant, cited in its doc comment: "writes exactly one `platform_audit` row on every outcome (decision 4) — created, idempotent, or rejected." Every other `rejected(...)` return in this file audits first; this one, introduced by F2's own fix, didn't. | `src/producer/register.rs:320-324` (commit `6744ba6`, pre-F5-fix) | Add the same `audit(..., "rejected")` call every other rejection branch in this function makes, before returning `Err` |

Disposition summary: 2 blocking (F2, F5 — both routed to rework), 3 fixed inline (F1, F3, F4).

cost: estimated M, actual M

### Rework fix record — round 1 (commit 6744ba6)

F2 fixed: `register_producer_inner`'s lost-race reclassification now returns
`Err(rejected(...))` on a `None` instead of `.expect(...)`-ing, matching the file's own
`rejected()` error-construction convention used by every other domain rejection in this
function. `just build`/`just lint`/`just test`/`just docs-check` re-run clean on
`feat/T-026-check-then-act-races` at this commit (full suite green across every test binary,
including the 3 concurrent tests and the widened 6-way tenant-config one from F1's fix).

**Scoped re-review of round 1** (delegated, independent — reviewing agent authored the round-1
fix in this same session): confirmed F2 resolved, no panic path remains for this condition;
`rejected()`'s construction matches file convention; re-ran `just build`/`just lint`/`just test`/
`just docs-check`, all clean, no regression. Reading the fix's own replacement text (per
`review-protocol.md` §1) surfaced F5 above — a defect in round 1's fix itself, not the original
F2 finding.

### Rework fix record — round 2 (commit 6faad13)

F5 fixed: the `None` branch now calls `audit(control_pool, actor, "producer.register",
Some(tenant_id), name, Some(cert_subject), "rejected")` before returning `Err`, matching every
other rejection branch in this function. No test added — the branch requires a `producer_id`
UUID v4 collision to reach, which this integration harness cannot simulate (same precedent as
T-018/F3: "accepted as noted... isn't practically simulable in this integration-test harness").
`just build`/`just lint`/`just test`/`just docs-check` re-run clean on
`feat/T-026-check-then-act-races` at this commit.

**Scoped re-review of round 2** (delegated, independent — same-session-authorship rule applies
again): confirmed F5 resolved — the new `audit(...)` call's action, arg order, and values match
every other rejection site in this function exactly; audit fires before the `Err` return with
`?` present (no silent-swallow risk); no double-audit possible (`classify_registration`'s own
`None` return path doesn't audit). No new defect found. `just build`/`just lint`/`just test`/
`just docs-check` re-verified clean by hand afterward. **Verdict: no blocking findings remain.**

## History

- 2026-09-02 — created (TO DO). source: audit: batches four check-then-act races noted across prior reviews (T-010/F1 and its two named sibling instances in producer registration and tenant-config set; T-006/F3's dev-PKI root race), each individually accepted under a single-operator-CLI tolerance this audit records as expiring once the admin panel/platform console land.
- 2026-09-07 — refined: re-verified #4 against current `main` and found its premise partly stale (in-process race already mutex-guarded by commit `0c331de`, landed off-ticket 2026-08-31); decided with the user to accept the remaining cross-process dev-PKI race as documented-not-fixed rather than add a new database dependency to dev-only tooling for it. Scope for #1-#3 unchanged. Wrote the Implementation Plan.
- 2026-09-07 — TO DO → READY: plan complete
- 2026-09-07 — READY → IN DEVELOPMENT: picked up
- 2026-09-07 — plan amended inline: Task 2's `classify_registration` needed its two existence checks wrapped in one `REPEATABLE READ` transaction — the concurrent-registration acceptance test caught a second, narrower check-then-act race internal to the classification itself (a task-switch between `find_by_name` and `find_by_cert_subject` could straddle a concurrent registration's commit)
- 2026-09-07 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-07 — IN REVIEW → REWORK: F2 blocking: register_producer_inner reintroduces a panic path T-025 eliminated
- 2026-09-07 — REWORK → IN REVIEW: F2 fixed (commit 6744ba6)
- 2026-09-07 — IN REVIEW → REWORK: F5 blocking: F2's own fix skipped the platform_audit call on its new rejected path
- 2026-09-07 — REWORK → IN REVIEW: F5 fixed (commit 6faad13)
- 2026-09-07 — IN REVIEW → DONE: review clean after 2 rework rounds (F2, F5); 3 non-blocking findings fixed inline (F1, F3, F4)
