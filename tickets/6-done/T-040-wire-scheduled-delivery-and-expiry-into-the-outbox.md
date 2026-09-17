---
id: T-040
title: Wire scheduled delivery and expiry into the outbox
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: high
cost: L
---

# T-040 — Wire scheduled delivery and expiry into the outbox

## Outcome

After this ships, a producer's `scheduled_for` and `expires_at` on a request actually take effect:
a future-dated message sits unclaimed until its time, an expired one is written `expired` instead
of sent, and a request beyond the tenant's scheduling horizon is rejected at ingest instead of
being silently accepted and immediately claimable.

## Description

Closes build-order step 9's core mechanism (§6.2), which turns out to be schema-complete but
functionally inert. Verified directly in code: `src/ingest/repo.rs`'s outbox INSERT hardcodes
`next_attempt_at` to `now` and `expires_at` to `NULL` on every row, regardless of what the
`comms_request` row carries — so a message sent with a future `scheduled_for` is claimable
immediately, and `expires_at` is never populated for the dispatcher to check. This also means the
gate chain's own **Expiry gate — listed first in §5's table as "checked first, cheapest"** — is
dead code today: `src/dispatcher/drain.rs` already has the `expires_at <= now` check written and
correct, but it can never fire because the column it reads is always `NULL`.

Also unenforced: `tenant_config.schedule_horizon_days` (default 90, T-007) is stored and
shown by `messgr-control tenant-config show`, but nothing reads it at ingest — a request can
schedule arbitrarily far out today with no rejection.

Per §6.2: "Requests beyond the horizon are rejected unless the producer holds an explicit
override" — no override mechanism exists yet; if this ticket's refinement finds the override is
needed for launch, split it out rather than growing this ticket, since horizon rejection with no
override is still a correct, shippable state (nobody has an override to lose).

**Refinement correction (2026-09-17):** two assumptions above didn't survive re-verification
against the current code, and both are needed for the Outcome above to actually hold, not
optional extras:

1. `CreateCommsRequest` (`src/ingest/model.rs`) has **no `scheduled_for` or `expires_at` fields
   at all** — the DB columns exist (`migrations/tenant/0004_ledger_outbox_schema.sql`) but a
   producer has no way to set either today. "Wire ... into the outbox" undersold this: it isn't
   just a dead DB write, the ingest API itself needs the two fields added.
2. "All gates run at dispatch, never at schedule time (already true structurally, this ticket
   doesn't touch the gate chain itself)" was wrong. The Expiry gate (§5, "checked first —
   cheapest") only exists in the kill-switch release-drain path (`src/dispatcher/drain.rs`'s
   `drain_released_scope`/`write_expired`). The normal claim loop
   (`src/dispatcher/worker.rs::try_process`) has no expiry check at all — it starts at the
   Suppression gate. Populating `outbox.expires_at` without adding the check there would leave
   an expired message dispatched through the ordinary (non-kill-switch) path still sending,
   which contradicts this ticket's own Outcome. This ticket now adds the Expiry gate to
   `try_process`, as the first check, ahead of Suppression, per §5's ordering.
   `drain.rs`'s own pre-check stays as-is — it's the F1/F2-rework-hardened retry path for the
   drain case specifically, and duplicating vs. touching it are two different tickets' worth of
   risk; see the Implementation Plan's confirmed decisions.

Re-graded accordingly below (impact unchanged; complexity/cost up to reflect the added
gate-chain and API-surface work).

**Explicitly out of scope:** `scheduled_local` (customer-local-time scheduling, §6.2's second
form) — only `scheduled_for` (absolute UTC) exists in the schema today; local-time scheduling
needs the same tz machinery as quiet hours (T-043) and should follow it, not duplicate it. Quiet
hours precedence (§6.3: quiet hours wins over a scheduled time) is also out of scope until T-043
exists — note the coupling in this ticket's Implementation Plan when refined.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .   # messgr is the root-path child (path = "." in pickle.toml)
git checkout main
git checkout -b feat/T-040-wire-scheduled-delivery-and-expiry
```

WIP commits locally as you go. Publish (push / MR) only after user approval, per the project's
commit policy — see Finish.

### Prerequisite gate (hard)

None. `depends-on: []`; branch cuts clean from `main`.

### Confirmed design decisions (do not deviate without asking)

1. **`scheduled_for` beyond the horizon is rejected at ingest with `422`; a past `scheduled_for`
   is not rejected — it collapses to "send immediately"** (§6.2, user-confirmed during
   refinement). `next_attempt_at` is a claim-eligibility timestamp; a past one is simply
   immediately claimable, the same as the existing `NULL` case, so no special-casing is needed.
2. **A past or otherwise "doomed" `expires_at` is never rejected at ingest** (user-confirmed
   during refinement). It flows through unchanged and is caught by the Expiry gate the first
   time the row is claimed (task 4) — one code path handles "expired before dispatch" and
   "expired while queued" instead of duplicating the check at ingest too.
3. **The horizon (`tenant_config.schedule_horizon_days`) bounds `scheduled_for` only, not
   `expires_at`.** §6.2's "Maximum horizon" paragraph is specifically about a message sitting
   unclaimed for years against a template/product/customer that may no longer be current;
   `expires_at` carries no such risk — an expired row terminal-writes `expired` and leaves the
   outbox, it doesn't sit inert.
4. **`class = "auth"` needs no new rejection for `scheduled_for`.** §6.3 says auth "never
   scheduled... the API rejects `scheduled_for` on auth-class requests", but
   `handler::validate_class` already rejects `class = "auth"` outright for `POST /comms` (it's
   never a legal value here at all, per `model.rs`'s own doc comment on `class::AUTH` and
   AGENTS.md hard invariant 1) — so this is already true, structurally, with no code change.
5. **`src/dispatcher/drain.rs`'s existing expiry pre-check (`write_expired`, 5-attempt retry) is
   left untouched, not merged into `try_process`'s new gate.** It's already correct and
   review-hardened (F1/F2 rework in that module) for the kill-switch drain case specifically;
   once `outbox.expires_at` is populated (task 3) it starts working as designed. Removing it in
   favor of relying solely on `try_process`'s simpler, non-retrying gate would drop that
   hardening as a side effect of this ticket, not a deliberate call — out of scope. The two
   checks overlapping is harmless: during a drain, `write_expired` catches an expired row before
   `process_one`/`try_process` is ever reached for it.
6. **No scheduling override mechanism.** §6.2 mentions one for horizon rejection; none exists
   today and none is built here — horizon rejection with no override is still a correct,
   shippable state (Description).

### Tasks

#### Task 1 — Accept `scheduled_for`/`expires_at` on the ingest API

`src/ingest/model.rs`: add to `CreateCommsRequest` (after `campaign_id`, same style as the
existing `Option<String>` fields — no `#[serde(default)]` needed, `serde` already treats a
missing `Option<T>` key as `None`):

```rust
pub scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
```

Add `IngestError::ScheduleHorizonExceeded { horizon_days: i32 }`, its `Display` arm ("scheduled_for
exceeds the tenant's N-day scheduling horizon"), and route it to `StatusCode::UNPROCESSABLE_ENTITY`
in `IntoResponse`, in the same group as `InvalidClass`/`InvalidChannel`/`InvalidResolutionInput`.

#### Task 2 — Validate the scheduling horizon at ingest

`src/ingest/handler.rs`: add `use chrono::{DateTime, Duration, Utc};` and a
`fn validate_schedule(scheduled_for: Option<DateTime<Utc>>, horizon_days: i32) ->
Result<(), IngestError>` next to `validate_channel`/`validate_class`, rejecting only when
`scheduled_for > Utc::now() + Duration::days(horizon_days.into())` (decision 1). Call it in
`create_comms` right after `build_resolution_input(&body)?` — before any DB/Vault work, same
placement as the other cheap validations — using `producer.tenant.config.schedule_horizon_days`
(reading that one `i32` field doesn't move `producer.tenant`, so the later
`let tenant = producer.tenant;` still holds):

```rust
validate_schedule(body.scheduled_for, producer.tenant.config.schedule_horizon_days)?;
```

#### Task 3 — Thread the values into the ledger + outbox write

`src/ingest/repo.rs::insert_transactional`: add two parameters,
`scheduled_for: Option<DateTime<Utc>>, expires_at: Option<DateTime<Utc>>` (keep
`#[allow(clippy::too_many_arguments)]`, already present). Bind them into the `comms_request`
INSERT in place of its two hardcoded `NULL`s (`scheduled_for, expires_at` columns). Compute
`let next_attempt_at = scheduled_for.unwrap_or(now);` and bind it in place of the current
`.bind(now)` for the outbox row's `next_attempt_at`; bind `expires_at` into the outbox INSERT in
place of its hardcoded `NULL`.

Update the one production call site, `src/ingest/handler.rs::create_comms`'s call to
`insert_transactional`, passing `body.scheduled_for, body.expires_at`.

Update the two existing test call sites so they keep compiling (pass `None, None` — none of
them are testing scheduling behavior):
- `tests/dispatcher.rs::write_outbox_row_with_verification`
- `tests/kill_switch.rs` (the direct `insert_transactional` call, ~line 231)

#### Task 4 — Add the Expiry gate to the main dispatch loop

`src/dispatcher/worker.rs::try_process`: at the very top, before the existing Suppression gate,
add:

```rust
// Expiry gate (DESIGN.md §5, T-040) — checked first: cheapest, and avoids
// spending any other gate's work on a dead message. `src/dispatcher/drain.rs`
// runs the equivalent check ahead of this for the kill-switch drain path
// specifically (decision 5) — this covers the ordinary claim loop, which had
// no expiry check at all before this ticket.
if row.expires_at.is_some_and(|expires_at| expires_at <= Utc::now()) {
    repo::write_terminal(
        &ctx.pool,
        row.created_at,
        row.comms_request_id,
        row.customer_id,
        "expired",
        None,
        None,
        "expired",
    )
    .await?;
    return Ok(());
}
```

`Utc` is already imported in this file. Do not touch `src/dispatcher/drain.rs` (decision 5).

#### Task 5 — Tests

`tests/ingest.rs` (follow `sample_body`/existing DB-assertion conventions, e.g.
`create_comms_writes_ledger_and_outbox_and_idempotency_replays`):
- `create_comms_honours_scheduled_for`: POST with `scheduled_for` a few days in the future;
  assert `201`; assert `comms_request.scheduled_for` and `outbox.next_attempt_at` both equal it
  (not `now`).
- `create_comms_rejects_scheduled_for_beyond_horizon`: POST with `scheduled_for` past the
  tenant's `schedule_horizon_days` (90, per `sample_tenant_config`); assert `422`.
- `create_comms_writes_expires_at_to_outbox`: POST with `expires_at` set (no `scheduled_for`);
  assert both `comms_request.expires_at` and `outbox.expires_at` equal it.
- `create_comms_omits_scheduling_fields_by_default`: POST with neither field (plain
  `sample_body`); assert `outbox.expires_at IS NULL` and `outbox.next_attempt_at` is within a
  few seconds of `now` — locks in "absent = send immediately", unchanged from today.

`tests/dispatcher.rs` (follow `write_ready_outbox_row`/`enforce_blocks_unverified_address_...`
conventions — write a row via `write_outbox_row_with_verification`-style helper, claim it, call
`try_process`, assert the terminal write):
- `expired_row_is_terminal_written_and_not_sent`: write an outbox row via
  `insert_transactional` with `expires_at` in the past, claim it, call `try_process`, assert the
  written `comms_event`/`comms_request.final_status` is `"expired"` and the `Sender` was never
  invoked (mirrors `enforce_blocks_unverified_address_with_terminal_event_and_no_send`'s shape).

#### Task 6 — Docs

Covered in the mandatory Docs update step below.

### Acceptance test

From the `messgr` root:

```
just build
just test      # includes the 5 new tests above; full suite must stay green
just lint       # cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings
just docs-check # snowball check — validates docs/user-manual/*.adoc
```

Manual smoke check (mirrors `docs/user-manual/ingest.adoc`'s existing curl examples): with a
tenant/producer provisioned and `messgr-ingest` running locally, POST a request with
`"scheduled_for":"<a few minutes from now, RFC3339>"` and confirm (`SELECT next_attempt_at,
expires_at FROM outbox WHERE comms_request_id = ...`) the row isn't claimable until then; POST
one with `"expires_at":"<a time already past>"` and no `scheduled_for`, start
`messgr-dispatcher`, and confirm `comms_request.final_status` becomes `expired`, not `sent`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/ingest.adoc`: document the new optional `scheduled_for`/`expires_at`
  request fields (near the existing field-combination paragraph, lines 7-14), and the new `422`
  for a `scheduled_for` beyond `tenant_config.schedule_horizon_days`, alongside the existing
  `422` for an invalid resolution-input combination.
- `docs/user-manual/dispatcher.adoc`: add a paragraph introducing the Expiry gate (T-040,
  DESIGN.md §5) as the first in-line gate check the claim loop performs. Update line 36's "the
  first in-line gate check this send path runs" (currently said of Suppression, `T-038`) to
  "next" — it's no longer first once Expiry is checked ahead of it.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated per above.
3. Write a summary: files touched, decisions made (the six above), anything deferred (the
   scheduling override mechanism, `scheduled_local`, quiet-hours precedence — all already
   out of scope per the Description).
4. Suggest a Conventional Commit message, e.g.:

   ```
   feat(ingest,dispatcher): wire scheduled delivery and expiry into the outbox (T-040)

   <body — what and why>
   ```

5. Tidy WIP commits into a small number of atomic ones (root-path child).
6. Commit locally; do not push or open an MR without user approval. Before pushing, verify
   `origin/main...HEAD` carries no `tickets/` path (in-tree layout). Present the commit message,
   then hand back.

## Review

- [x] Reviewer independence settled (step 0): **independent** — fresh session (post-`/clear`),
  no memory of authoring this branch. Audits (steps 2-4a) additionally delegated to a spawned,
  briefed-adversarially sub-agent for extra rigor; every delegated finding re-verified by hand
  before being recorded here.
- [x] Implementation audit — all 6 tasks verified met, in the files named, against the actual
  tree (not the plan prose); acceptance test re-run verbatim: `just build`, `just test` (full
  suite green, incl. the 5 new named tests), `just lint`, `just docs-check` — all clean.
- [x] Quality audit — idiomatic; mutation-tested the Task 4 gate reasoning (deleting it would
  flip `expired_row_is_terminal_written_and_not_sent` red two ways: wrong `final_status` and the
  wiremock `.expect(0)` firing) confirming it isn't tautological. One test-gap found (F1).
- [x] Consistency audit — all 6 confirmed decisions verified against shipped code. One
  governing-document staleness found and fixed inline (F2, per the review-addendum's step 5:
  design corrections are not deferred).
- [x] Documentation audit — `just docs-check` clean; `ingest.adoc`/`dispatcher.adoc` coverage
  spot-checked sentence-by-sentence against shipped behaviour; whole-tree grep for stale
  gate-order/scheduling claims found nothing else new.
- [x] Docs-readability pass — conscious skip: no docs-readability reviewer configured in this
  host/session.
- [x] Findings recorded below with severity, class, and disposition; disposition summary and
  cost line present.
- [x] Ticket moved to `tickets/6-done/`; `## History` appended.
- [x] Other references updated; governing documents reconciled — `DESIGN.md` bumped to Version 8
  and `development/design/14-decisions-and-open-questions.md` item 8 narrowed (F2's fix, commit
  `6b460bf` on the ticket branch).
- [x] Remaining-tickets impact sweep done: `T-041` and `T-043` (the only `1-to-do/` tickets
  mentioning T-040, both soft couplings, no `depends-on:`) re-read — both descriptions remain
  accurate to what shipped; no patch needed.
- [ ] Summary + commit message & MR attributes presented for approval; overarching bookkeeping
  committed per policy; next-ticket suggestion — pending, below.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | test-gap | noted | No ingest-level (`POST /comms`) test asserts decision 2 — a past `expires_at` is accepted, not rejected, at ingest. The only past-`expires_at` coverage (`expired_row_is_terminal_written_and_not_sent`) writes the row via `insert_transactional` directly, bypassing `validate_schedule`/the handler entirely. | `tests/ingest.rs` has no such case; handler has no `expires_at` validation at all (by design, decision 2) | Add `create_comms_accepts_past_expires_at`: POST with `expires_at` in the past, assert `201` and the value lands unchanged in `comms_request`/`outbox`. Small enough to fold into the next ticket touching `tests/ingest.rs`'s scheduling cases, not worth its own ticket. |
| F2 | non-blocking | stale-xref | fixed inline | `development/design/14-decisions-and-open-questions.md` Still-open item 8 said the 90-day scheduling horizon default needed "confirming" — T-040 ships it as the enforced, hard-rejecting default (decision 6), resolving that half of the question; only the override sub-question remains open. | `development/design/14-decisions-and-open-questions.md:49` (pre-fix) | Fixed inline (review-addendum step 5): item 8 reworded, `DESIGN.md` version bumped 7→8, commit `6b460bf` on `feat/T-040-wire-scheduled-delivery-and-expiry`. |
| F3 | non-blocking | stale-xref | noted | `development/design/05-send-timing.md`'s §6.2 "two forms" table still describes `scheduled_local` as accepted alongside `scheduled_for`; only `scheduled_for` exists in the schema/API. Pre-existing (predates this branch) and explicitly out of scope per this ticket's own Description — not this branch's defect, so not eligible for `fixed inline`, but surfaced here since the consistency audit's whole-tree sweep found it in the exact section this ticket touches. | `development/design/05-send-timing.md:26-31` | Leave for whichever ticket implements `scheduled_local` (flagged in T-043's Description as sharing tz machinery) to correct the doc alongside the code, or a design-scoped audit pass (review-addendum step 8) to pick up independently. |

Disposition summary: 3 findings — 1 fixed inline (F2), 2 noted (F1, F3). 0 folded, 0 new tickets.

cost: estimated L, actual L

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — DESIGN.md §5's Expiry gate (checked first in the gate chain, alongside T-036-T-038's
  verification/consent/suppression gates) cannot fire until this ships, since `outbox.expires_at`
  is hardcoded NULL at ingest today.
- 2026-09-17 — TO DO → READY: plan complete
- 2026-09-17 — READY → IN DEVELOPMENT: picked up
- 2026-09-17 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-17 — IN REVIEW → DONE: no blocking findings; 1 fixed inline, 2 noted
