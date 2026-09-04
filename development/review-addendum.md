# Review addendum — messgr project-specific rules

**Version 2** · written 2026-09-02 against `main` at `be36441` (design/implementation audit,
2026-09-02)

Applies **on top of** the brine review protocol
(`.agents/skills/brine/resources/review-protocol.md`), keyed to that procedure's step numbers.
It never replaces it.

There is no overarching layer above this one. messgr is `pickle.toml`'s only registered
child-project (`path = "."`, `layout = "in-tree"`); a second, overarching file at this same root
would run alongside this one on every review with nothing left for it to say that this file
doesn't already say. Do not "fix" that by adding one — it would be a distinction with no reader.

All audits run against the repository root.

## Step 1 — Load context (additions)

Read the sections of `DESIGN.md` the ticket's Description cites, plus `AGENTS.md`'s ten hard
invariants in full — not just the ones the ticket looks like it touches. A review **may amend
`DESIGN.md`**; it is not read-only input to be treated as correct by construction. That
assumption is exactly how findings A4–A8 (in `tickets/NOTES.md`'s 2026-09-02 audit) shipped: a
schema was copied out of the design and the copy was the acceptance test.

## Step 2 — Implementation audit (additions)

Configured commands: `just build`, `just test`, `just lint`, and `just docs-check` when the
ticket touched docs or the CLI/HTTP surface.

1. **"Verbatim from the design" is not a defence. Blocking.** If the ticket's diff or a comment
   in it says the schema or logic came straight from `DESIGN.md`, name the one claim you checked
   independently rather than transcribed. A transcribed `CREATE TABLE` whose Postgres semantics
   nobody verified is the defect class that produced findings A4 through A8.
2. **NULL semantics in every unique index and `ON CONFLICT`. Blocking.** Postgres treats NULLs
   as distinct values, so a `UNIQUE` index or `ON CONFLICT` target containing a nullable column
   does not dedupe rows where that column is NULL. Earned by `comms_event`'s inert
   `UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)` (provider_ref is NULL for
   gate-outcome and failed events) and by `kill_switch`'s two-concurrent-global-switches hole.
3. **Every claimed, leased, or locked row has a release path, and a test exercises it.
   Blocking.** Earned by the outbox lease: nothing in the codebase ever sets `leased_until` back
   to NULL, so any error between claim and terminal write stranded the row permanently.
4. **Secrets come from Vault (DESIGN.md §13). Blocking.** A provider API key, credential, or
   token read from an environment variable or a config file fails this check. Earned by
   `HttpSender` reading `DISPATCHER_<CH>_API_KEY` directly.
5. **A new table holding customer data is added to §7.2's erasure statements in the same
   ticket. Blocking** (AGENTS.md hard invariant 6). Earned by `suppression`, `customer`, and
   `idempotency.key` all landing without an erasure statement.
6. **Every new column names its reader and the ticket that adds it. Blocking if neither
   exists yet.** Earned by `comms_request.dek_id`, which is always written NULL and referenced
   by nothing.
7. *Advisory:* grep the diff against hard invariants 1 (auth/OTP never enters the queue) and 3
   (all gates run at dispatch, never at ingest).
8. **The ticket's configured local commands must be the literal commands CI runs, not an
   approximation. Blocking if a diff touches `justfile` or `.github/workflows/*.yml`.** Grep the
   two against each other; a `justfile` recipe and a workflow step doing the same job (fmt,
   lint, test) must invoke the same command, ideally by the workflow calling the recipe rather
   than re-deriving it. Earned by `just lint` running plain `cargo clippy -- -D warnings` while
   `ci.yml` ran `cargo clippy --all-targets --all-features -- -D warnings`, and `just fmt`
   mutating instead of checking while CI ran `cargo fmt --all -- --check` — both meant a clean
   local run proved nothing about CI.

## Step 3 — Quality audit (additions)

*Advisory:* **an assertion must be able to fail.** `result.is_err()` alone is not a
certification that a mechanism works — it also passes if the mechanism never ran. Mutation-test
anything load-bearing: delete or bypass the thing under test and confirm the test goes red. This
is messgr's most-repeated defect class (T-001/F13, T-003/F1, T-004/F2), and it is why the
tenant-pool isolation assertion read as tested for sixteen tickets while the code path that
would trip it was provably unreachable (`src/db.rs`'s own doc comment admits it).

## Step 4 — Consistency audit (additions)

*Advisory:* after any section renumber in `DESIGN.md`, grep the whole repo for stale `§N`
cross-references — they appear in code comments, migration headers, and other doc files, not
just in the design itself. The decisions table and the "Still open" list must reflect the
current review, not the one two tickets ago. `tenant_id` naming (column vs. no column vs.
implicit-via-database) has exactly one answer per DESIGN.md §2.1 — flag any table that
reintroduces the question.

## Step 4a — Documentation audit (additions)

Run `just docs-check`. A new CLI subcommand or HTTP route absent from `docs/user-manual/` is
**blocking** coverage. A behaviour the manual states as a property of the system — e.g.
`dispatcher.adoc`'s description of retry behaviour — must be re-checked against the code every
time that code changes, not assumed still true. The manual has previously documented a
message-loss behaviour as if it were a designed feature.

## Step 5 — Findings (additions)

If the review changes what `DESIGN.md` asserts, amend `DESIGN.md` in the same review and bump
its version stamp. Do not defer a design correction to a follow-up ticket — that is exactly the
gap this addendum exists to close.

## Step 8 — Impact sweep (additions)

Re-audit `DESIGN.md` itself at every build-order step boundary (AGENTS.md, "Build order"), not
only inside each ticket's own review. Evidence for why: sixteen ticket-scoped reviews, run
diligently, found zero DESIGN.md defects between them; one design-scoped pass found roughly
fifty. A ticket review's scope is naturally the diff in front of it — the design needs its own
periodic look from outside any single ticket's lens.

## Revision history

- **v1** (2026-09-02) — Written, following the 2026-09-02 design/implementation audit.
- **v2** (2026-09-04) — Added Step 2 item 8 (local/CI command parity), after `just lint`/`just
  fmt` were found to run different flags than `ci.yml`'s `cargo clippy`/`cargo fmt` steps.
