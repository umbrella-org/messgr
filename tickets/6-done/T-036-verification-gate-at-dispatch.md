---
id: T-036
title: Verification gate at dispatch
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-036 — Verification gate at dispatch

## Outcome

After this ships, a message no longer dispatches to an unverified address unnoticed: the
dispatcher checks `customer_address.verified_at` at send time and, per the tenant's
`verification_mode`, either blocks it (`enforce`, terminal `unverified_address` event) or lets it
through while recording and counting the outcome (`observe`).

## Description

Wires the verification gate from DESIGN.md §5 into the dispatcher's send path — the first of the
three regulatory gates (verification, consent [T-037], suppression [T-038]) named in build-order
step 5, none of which are enforced yet despite steps 0-4 and later hardening tickets being done.

Schema already exists: `customer_address.verified_at` (migration 0008) and
`tenant_config.verification_mode` (`enforce`|`observe`, migration 0002, default `observe`) — this
ticket is enforcement wiring, not new schema. Rule (§5): for `transactional` and `marketing`
class messages, `verified_at` must be set; `enforce` blocks with terminal event `unverified_address`
on `comms_event`, `observe` allows the send through but still records the outcome and exposes a
count (§5's own reasoning: a gate whose input might always be NULL must never silently no-op).
Auth class skips this gate entirely (§5, AGENTS.md hard invariant 1) — must not touch the OTP
path, which does not go through the dispatcher.

**Correction, found during refinement: the Description previously claimed this gate "runs after
kill switch and quota gates," implying both already exist as in-line checks in the dispatcher's
send path. Neither does.** `src/dispatcher/worker.rs::try_process` — the function every claimed
row runs through — currently does nothing but decrypt and send; it has no gate-chain evaluation
step at all. What's actually built: the kill-switch check (T-016) runs **before claiming**, as an
exclusion on `repo::claim`'s candidate query (`DispatcherContext::claim_exclusion`), not as an
in-line check inside `try_process`. The expiry check (§6.2) exists only inside the kill-switch
release-drain/discard tasks (`src/dispatcher/drain.rs`), not in the normal claim loop — wiring it
there is T-040 (TO DO). The producer quota gate (§5.1) doesn't exist anywhere yet — that's T-042
(TO DO). Quiet hours (§6) is T-043 (TO DO). So this ticket isn't inserting into an existing chain
in the stated order; it is adding the **first** in-line gate check `try_process` gets. The soft
coupling to T-037/T-038 stands — they'll each add their own check to the same function — but
"runs after kill switch and quota" is struck from the spec. §5's ordering table remains the
long-run target once T-040/T-042/T-043 land; nothing here contradicts it.

Soft coupling: shares the same dispatcher gate-chain evaluation point that T-037 and T-038 will
land in — sequencing among the three is left to pickup order (WIP limit is 1 anyway), not a hard
dependency, since each is independently observable once built.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-036-verification-gate-at-dispatch
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1.

### Confirmed design decisions (do not deviate without asking)

1. **The check lives inline in `try_process` (`src/dispatcher/worker.rs`), before
   `repo::load_ciphertexts`/DEK fetch.** No gate-chain abstraction is introduced — `try_process`
   currently has zero gate checks, so a plain sequential check at the top of the function is the
   entire "chain" there is. Placing it before decryption means a blocked send under `enforce`
   never touches Vault or spends a decrypt on a message that won't go out.
2. **Skip the gate unless `row.class` is `transactional` or `marketing`.** Reuse
   `crate::ingest::model::class::{TRANSACTIONAL, MARKETING}` rather than new string constants.
   `auth` is matched implicitly by the else-branch (falls through untouched) — defense in depth
   per §5, even though T-011 decision 3 means `class = "auth"` never actually reaches the outbox
   today.
3. **A missing `customer_address` row is treated identically to one with `verified_at = NULL`
   — both mean "unverified."** No FK ties `outbox.address_id` to `customer_address.id` (T-009
   decision 1), so a missing row is possible in principle; collapsing it to "unverified" is the
   safe direction, matching §5's own reasoning for consent's "absence defaults to opted-out."
4. **`tenant_config.verification_mode` is loaded once at `messgr-dispatcher` startup and carried
   on `DispatcherContext` as a plain `String` field, not hot-reloaded.** Matches the existing
   `kill_switch_release_rate` precedent in `src/bin/dispatcher.rs` — every other `tenant_config`
   field is load-once already, and unlike kill switches (§5.2's own seconds-not-minutes
   requirement, engaged during live incidents), nothing in §5 states a latency bound for
   `verification_mode` — it's an infrequent admin setting, not an incident-response control. A
   change takes effect on the next dispatcher restart.
5. **`enforce` writes the terminal event/status `unverified_address` via the existing
   `repo::write_terminal` (same shape as `expired`/`discarded`: event_type and final_status are
   the same string) and returns early — no send attempted.** `observe` writes a **non-terminal**
   `comms_event` row (new `repo::record_event`, event_type `unverified_address`, no
   `final_status`/outbox change) and falls through to the normal decrypt/send path. "Exposes a
   count" (Outcome) is satisfied by the event being ledger-queryable like any other
   `comms_event` row (§11's query API) — no new counter/metric is added.

### Tasks

#### Task 1 — `customer_address.verified_at` lookup

`src/dispatcher/repo.rs`: add

```rust
pub async fn load_verified_at(
    pool: &PgPool,
    address_id: Uuid,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let result: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT verified_at FROM customer_address WHERE id = $1",
    )
    .bind(address_id)
    .fetch_optional(pool)
    .await?;
    Ok(result.flatten())
}
```

(`Uuid`/`DateTime<Utc>`/`PgPool` already imported in this file.)

#### Task 2 — non-terminal event write for `observe`

`src/dispatcher/repo.rs`: add

```rust
pub async fn record_event(
    pool: &PgPool,
    comms_request_id: Uuid,
    customer_id: Uuid,
    event_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO comms_event (
            comms_request_id, customer_id, occurred_at, event_type, provider_ref,
            provider_status, provider_payload_ciphertext
        ) VALUES ($1, $2, $3, $4, '', NULL, NULL)
        ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING
        "#,
    )
    .bind(comms_request_id)
    .bind(customer_id)
    .bind(Utc::now())
    .bind(event_type)
    .execute(pool)
    .await?;
    Ok(())
}
```

`provider_ref` bound to `''` (not `NULL`), matching `write_terminal`'s own existing convention —
the addendum's NULL-in-unique-index item (review addendum step 2) is exactly why that convention
exists; don't regress it. Left as its own function rather than factored into `write_terminal`
(which runs inside a transaction against `&mut *tx`, not `&PgPool`) — no existing
generic-`Executor` pattern in this codebase to extend, and one function shared by one new call
site isn't worth introducing one now.

#### Task 3 — thread `verification_mode` onto `DispatcherContext`

`src/dispatcher/worker.rs`: add `pub verification_mode: String` to the `DispatcherContext`
struct (alongside `mount: String`, same shape).

`src/bin/dispatcher.rs`: add `const DEFAULT_VERIFICATION_MODE: &str = "observe";` (matches
`tenant_config` migration 0002's own column default). Change the existing

```rust
let release_rate = tenant_config_repo::load(&tenant_pool)
    .await
    .expect("loading tenant_config failed")
    .map(|c| c.kill_switch_release_rate)
    .unwrap_or(DEFAULT_KILL_SWITCH_RELEASE_RATE) as i64;
```

to load once and derive both fields:

```rust
let tenant_config = tenant_config_repo::load(&tenant_pool)
    .await
    .expect("loading tenant_config failed");
let release_rate = tenant_config
    .as_ref()
    .map(|c| c.kill_switch_release_rate)
    .unwrap_or(DEFAULT_KILL_SWITCH_RELEASE_RATE) as i64;
let verification_mode = tenant_config
    .as_ref()
    .map(|c| c.verification_mode.clone())
    .unwrap_or_else(|| DEFAULT_VERIFICATION_MODE.to_string());
```

Pass `verification_mode: verification_mode.clone()` into each `DispatcherContext { .. }` literal
in the `for (channel, base_url, api_key) in channel_senders` loop (one clone per channel, `String`
isn't `Copy`).

#### Task 4 — the gate check in `try_process`

`src/dispatcher/worker.rs`: add near the top of `try_process`, before the existing
`repo::load_ciphertexts` call:

```rust
use crate::ingest::model::class;
use crate::tenant_config::model::verification_mode;

if matches!(row.class.as_str(), class::TRANSACTIONAL | class::MARKETING) {
    let verified = repo::load_verified_at(&ctx.pool, row.address_id).await?;
    if verified.is_none() {
        if ctx.verification_mode == verification_mode::ENFORCE {
            repo::write_terminal(
                &ctx.pool,
                row.created_at,
                row.comms_request_id,
                row.customer_id,
                "unverified_address",
                None,
                None,
                "unverified_address",
            )
            .await?;
            return Ok(());
        }
        repo::record_event(
            &ctx.pool,
            row.comms_request_id,
            row.customer_id,
            "unverified_address",
        )
        .await?;
    }
}
```

(Imports go at the file's existing `use` block, not inline — shown inline here only to mark
where they're new.)

#### Task 5 — every existing `DispatcherContext { .. }` literal

Add `verification_mode: "observe".to_string()` (or the scenario's own mode) to every existing
`DispatcherContext { .. }` literal so the crate still compiles —
`grep -rn "DispatcherContext {" tests/` to find them all: **both** `tests/dispatcher.rs` (7
literals) **and** `tests/kill_switch.rs` (1 literal, in
`drain_sends_every_row_and_marks_an_already_expired_one_expired_instead`).

Also: `write_ready_outbox_row`'s pre-existing rows (`tests/dispatcher.rs`) never had a
`customer_address` row at all, so under `observe` the new gate would insert an extra
`unverified_address` event ahead of `sent` on every one of them — silently breaking the three
tests that assert an *exact* single-event `comms_event` list
(`successful_send_writes_sent_event_and_final_status_and_deletes_the_outbox_row`,
`terminal_provider_rejection_writes_failed_event_and_final_status_with_no_requeue`,
`retries_exhausted_after_max_attempts_terminal_fails`). Fix at the source, not by patching three
assertions: give `write_ready_outbox_row` a real, already-verified `customer_address` row by
default (real FK to `customer(id)`, so a parent `customer` row is needed too — neither table had
ever been touched by this suite before). Concretely: extract a lower-level
`write_outbox_row_with_verification(tenant, vault, cache, destination, body, class,
verified_at)` that inserts both rows and calls `insert_transactional` with the real
`address_id`; make `write_ready_outbox_row` a thin wrapper calling it with `class =
"transactional"`, `verified_at = Some(Utc::now())`. The four new acceptance tests below call the
lower-level function directly to control `class`/`verified_at` themselves.
`tests/kill_switch.rs`'s own row-writing helper does not need the same treatment — none of its
assertions inspect `comms_event` contents, so an extra `observe`-mode event is harmless there.

### Acceptance test

Add to `tests/dispatcher.rs`, following `write_ready_outbox_row`'s existing pattern (real
provisioning, real DEK, `ingest::repo::insert_transactional`) plus a directly-inserted
`customer_address` row for the `address_id` it's called with:

1. **`enforce` blocks an unverified address: terminal `unverified_address` event, matching
   `final_status`, no provider call, outbox row removed.** Insert a `customer_address` row with
   `verified_at = NULL`; set `DispatcherContext.verification_mode = "enforce"`; call
   `try_process`. Assert: `comms_event` has exactly one row, `event_type = 'unverified_address'`;
   `comms_request.final_status = Some("unverified_address")`; `outbox` row count 0; the wiremock
   mock (mounted with an expectation of zero calls, e.g. `.expect(0)`) never receives a request.
2. **`observe` records and still sends.** Same unverified `customer_address` row,
   `verification_mode = "observe"`. Assert: `comms_event` has **two** rows for this
   `comms_request_id` — `unverified_address` (non-terminal) and `sent`; `final_status =
   Some("sent")`; outbox row removed; the mock provider was called once.
3. **A verified address sends normally under `enforce` (negative control / mutation test).**
   `customer_address.verified_at = Some(Utc::now())`, `verification_mode = "enforce"`. Assert:
   only a `sent` event, `final_status = Some("sent")`, mock called once — proves the gate isn't
   blocking unconditionally.
4. **`auth`-class rows skip the gate even when unverified.** Class `"auth"`, unverified address,
   `verification_mode = "enforce"`. Assert the send still reaches the mock (this exercises
   decision 2's skip directly; `class = "auth"` never reaching the outbox in production per
   T-011 decision 3 is a separate invariant this test doesn't need to re-prove).

Run: `just build && just test && just lint`.

### Docs update (mandatory when user-facing)

`docs/user-manual/dispatcher.adoc`: add a paragraph after the existing `T-016` kill-switch
paragraph, in the same style (see that file's `T-021`/`T-016` paragraphs), stating: the
verification gate now runs before decrypt/send for `transactional`/`marketing` rows; per-tenant
`enforce` blocks an address with no `verified_at` and writes a terminal `unverified_address`
event; `observe` (the default) records the same event non-terminally and still sends; `auth`
rows are exempt. No new CLI flag or HTTP route, so `just docs-check` (`snowball check`) needs no
separate registration — this is a content addition to an existing file.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint` clean.
2. `docs/user-manual/dispatcher.adoc` updated per above.
3. Write a summary (files touched, decisions made, anything deferred) and hand back.
4. Suggested commit message:

   ```
   feat(dispatcher): enforce the verification gate at send time (T-036)

   Checks customer_address.verified_at for transactional/marketing sends
   before decrypt; enforce blocks with a terminal unverified_address
   event, observe records it and sends anyway. First in-line gate check
   in try_process — T-037/T-038 will add their own alongside it.
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on `feat/T-036-verification-gate-at-dispatch`. Do not push or open an MR
   without user approval. Present the commit message; after approval, verify
   `origin/main...HEAD` carries no `tickets/` path, then push and open the MR. Merging is the
   human's.

## Review

- [x] Reviewer independence settled (step 0): **delegated**. The implementing agent authored
  `feat/T-036-verification-gate-at-dispatch` in this same session, so steps 2-4a (implementation,
  quality, consistency, documentation audits) were run by a fresh sub-agent with no memory of
  writing the code, briefed adversarially and pointed at the branch, `main`'s copy of this
  ticket, `AGENTS.md`, the review addendum, and DESIGN.md §5. Every delegated finding below was
  independently re-verified by hand before being recorded here, per step 0's "delegation buys
  independence, not accuracy."
- [x] Implementation audit — all 5 confirmed design decisions and all 5 tasks verified done, in
  the files named, by the independent reviewer (file:line evidence for each — see its full
  report, condensed below). **Acceptance test**: initially blocked — the independent reviewer's
  first run hit a Postgres auth failure (`28P01`, `password authentication failed for user
  "messgr"`) that also broke an unrelated pre-existing unit test; re-verified by hand
  (reproduced the identical error independently). Root cause: the local dev stack (Postgres role
  password + dev-mode Vault, both external to this branch — the diff touches no `.env`,
  `compose.yml`, migration, or credential file) was reset mid-session, unrelated to this ticket.
  After the user restarted the stack: ran `messgr-control migrate` (fresh `control` DB had no
  schema), `just vault-dev-init` (dev-mode Vault doesn't persist the AppRole auth mount across a
  restart), `just tablespace-init` (`messgr_cold` tablespace lost with the volume — needed by
  `tests/partition_lifecycle.rs`, unrelated to this ticket but part of getting `just test` fully
  green). Final clean run, by hand: `cargo test --test dispatcher --test kill_switch` → **21/21
  pass** (all 4 new acceptance tests plus the 17 pre-existing ones this branch touches);
  `just test` → **27/27 binaries pass** (full repo suite, no regressions); `just build` → clean;
  `just lint` (`cargo fmt --check` + `cargo clippy -D warnings`) → clean; `just docs-check`
  (`snowball check`) → clean.
- [x] Quality audit (step 3) — SQL in both new `repo.rs` functions verified correct by hand
  (parameter bind order, the `Option<Option<DateTime<Utc>>>` → `.flatten()` logic for
  "missing row" vs "`verified_at IS NULL`", the `ON CONFLICT` target matching the table's actual
  `UNIQUE` constraint verbatim). Mutation-testability of the 4 new acceptance tests confirmed,
  not assumed: traced `wiremock` 0.6's `MockServer::drop` → `verify()` to confirm `.expect(0)`/
  `.expect(1)` genuinely panics the test on a call-count mismatch, and 3 of the 4 tests assert an
  explicit negative control. No `unwrap()`/`expect()` on a production code path; clippy clean
  under `-D warnings`; no new secret handling; every bind parameterized.
- [x] Consistency audit (step 4) — grepped against AGENTS.md hard invariants 1 and 3: invariant 1
  (auth/OTP never through the queue) not violated — the gate's `matches!` allowlist excludes
  `class::AUTH`, and independently re-verified against `tickets/6-done/T-011-*.md`'s own text
  that `class = "auth"` is rejected at ingest (`422`) and never reaches the outbox, so the
  in-gate skip is defense-in-depth for an already-unreachable state, not a workaround for a
  reachable one. Invariant 3 (gates run at dispatch, not ingest) not violated — the check reads
  `customer_address.verified_at` fresh inside `try_process`, no ingest-time caching. No dead
  imports (clippy would have caught them under `-D warnings`), no stale `§`-references, no scope
  creep (diff touches exactly the files the plan named).
- [x] Documentation audit (step 4a) — `docs/user-manual/dispatcher.adoc`'s new paragraph checked
  sentence-by-sentence against the code; no inaccuracies found. `just docs-check` clean (covered
  above). No other doc needed a T-036 mention (no new CLI flag or HTTP route shipped).
- [x] Docs-readability pass (step 4b) — **conscious skip**: no docs-readability reviewer/tool is
  configured in this environment.
- [x] Findings recorded below with severity, class, and disposition; disposition summary + cost
  line present (step 5).
- [x] Ticket moved to `tickets/6-done/` (step 6b — no blocking findings).
- [x] Other references / governing documents reconciled (step 7): checked
  `development/design/14-decisions-and-open-questions.md` — item 6 of "Still open" (whether the
  master system publishes per-address verification state) is **not** resolved by this ticket
  (it wires the enforcement mechanics for whichever mode is configured; it doesn't answer whether
  the feed exists) and correctly remains open, no edit needed. No decisions-table row claims the
  gate chain is or isn't built, so nothing there was made false. `AGENTS.md`'s "Step 5, the gate
  chain... is not yet built" line remains accurate as written — it names the three-gate chain
  collectively, and T-037/T-038 are still unbuilt, so the sentence doesn't go false until all
  three ship; left as-is rather than edited into an intermediate state that would need editing
  again twice more.
- [x] Remaining-tickets impact sweep done (step 8): re-read `T-037` and `T-038` (both
  `tickets/1-to-do/`, both reference T-036 by id, neither has a hard `depends-on:` on it). Both
  already describe T-036 correctly — "runs after verification (T-036)... per §5's ordering
  table" — which matches what actually got built (T-036's check is first, inline, in
  `try_process`); no correction needed to either.
- [x] Summary + commit message & MR attributes presented for approval; overarching-repo
  bookkeeping committed per policy; remote-base check to run before push (step 9) — see below.

### Findings

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | docs-gap | fixed inline | Comment on `record_event` framed `provider_ref: ''` as avoiding a live NULL-in-`UNIQUE`/`ON CONFLICT` dedup hole; `comms_event.provider_ref` has been `NOT NULL DEFAULT ''` since migration 0004 (predates this ticket), so `NULL` was never actually bindable there — the comment overstated the risk it was avoiding. No behavior impact. | `src/dispatcher/repo.rs:186-192` (pre-fix) | Reword to state the column is `NOT NULL`, not implying an active NULL hole. |
| F2 | non-blocking | test-gap | noted | `tests/kill_switch.rs`'s own `write_outbox_row` helper never creates a `customer_address` row and hardcodes `class = "transactional"`, so under the new gate every row that suite creates is "unverified" by construction (harmless today — no assertion in that file inspects `comms_event` contents — but untested kill-switch × verification-gate interaction, and a footgun if that file's `verification_mode` is ever set to `"enforce"`). | `tests/kill_switch.rs:196-249` | If T-037/T-038 end up touching this helper, consider giving it the same treatment `write_ready_outbox_row` got in `tests/dispatcher.rs` (T-036 plan Task 5). Not promoted to a ticket now — doesn't clear the promotion test on its own. |

Disposition summary: 2 non-blocking findings — 1 `fixed inline` (F1, committed `a31e7a9` on
`feat/T-036-verification-gate-at-dispatch` during this review — not a rework round, since no
blocking finding sent the ticket to `5-rework/`), 1 `noted` (F2). No blocking findings. No new
tickets spawned.

cost: estimated M, actual M

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — step 5 (the gate chain) is unbuilt despite steps 0-4 and 17 later hardening tickets
  being done; split into three independently-schedulable gates (this one, T-037, T-038).
- 2026-09-16 — TO DO → READY: plan complete
- 2026-09-16 — plan amended inline: pickup applicability audit (independent sub-agent) confirmed
  every plan assumption against current code except Task 5's literal count — `tests/dispatcher.rs`
  has 7 `DispatcherContext { .. }` construction sites, not 8; corrected in place, no other change.
- 2026-09-16 — READY → IN DEVELOPMENT: picked up
- 2026-09-16 — plan amended inline: Task 5 missed a second `DispatcherContext { .. }` literal in
  `tests/kill_switch.rs` (the applicability audit and the original plan both only checked
  `tests/dispatcher.rs`) — found via `cargo build`'s own missing-field error; added the field
  there too. Also found mid-implementation: `write_ready_outbox_row`'s rows had no
  `customer_address` row at all, so the new `observe`-mode gate injected an extra
  `unverified_address` event into three tests asserting an exact single-event list, and
  `customer_address.customer_id`'s real FK to `customer(id)` meant a parent row was needed too.
  Fixed by giving `write_ready_outbox_row` a real, already-verified `customer_address`/`customer`
  row by default (via a new lower-level `write_outbox_row_with_verification` helper the new
  acceptance tests also use) rather than patching the three assertions. Task 5's plan text
  updated to match; no scope change beyond it.
- 2026-09-16 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-16 — IN REVIEW → DONE: review clean; 2 non-blocking (1 fixed inline, 1 noted)
