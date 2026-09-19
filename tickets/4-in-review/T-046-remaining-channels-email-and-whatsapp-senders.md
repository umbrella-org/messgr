---
id: T-046
title: Remaining channels: email and WhatsApp senders
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: low
cost: S
---

# T-046 — Remaining channels: email and WhatsApp senders

## Outcome

After this ships, `channel=email` and `channel=whatsapp` are proven — by a runnable test, not
just by reading the code — to flow through the same `POST /comms`, gate chain, and dispatcher
as SMS, and a developer can run `messgr-dispatcher` locally against all three channels from a
documented `.env.example`. "Every channel under one API, one ledger, one gate chain"
(build-order §14, step 11) stops being an SMS-only claim resting on inspection.

## Description

**Refinement found this ticket's premise stale: there is no `Sender` left to build.** Every
piece build-order step 11 originally scoped is already shipped, generically, by earlier
tickets:

- `ingest::model::channel` already declares `EMAIL`/`WHATSAPP`, and `validate_channel`
  (`src/ingest/handler.rs`) already accepts all three.
- `customer::model::kind_for_channel` already maps `sms→msisdn`, `email→email`,
  `whatsapp→whatsapp`, and `customer::resolve` calls it generically — the HMAC-lookup path
  (T-015/T-018) has no SMS-only assumption to fix.
- `provider_config` and its `messgr-control` CLI already validate `channel` against all three
  values (`src/bin/control.rs`), as does `template approve`.
- `Sender`/`HttpSender` (T-012) were never SMS-specific: the trait is
  `send(destination: &str, body: &str)` against this codebase's own generic mock-vendor HTTP
  contract, not a named vendor's API. No real vendor was ever picked for any channel (Still
  Open #4, `14-decisions-and-open-questions.md` §12, stays open — this ticket does not resolve
  it, for SMS or the other two).
- `messgr-dispatcher` (`src/bin/dispatcher.rs`) already loops over `DISPATCHER_CHANNELS`,
  resolves each channel's own `provider_config` row + Vault credential, and spawns one
  `DispatcherContext`/`run_channel_loop` per channel. `dispatcher::worker::try_process` has no
  channel-specific branch anywhere in the gate chain.
- No channel — including SMS today — validates destination *format* server-side (§4.8: the
  caller always supplies a fresh `destination`), so there is nothing to add here either.

**What is genuinely missing, and what this ticket actually does:**

1. **No test exercises `channel=email` or `channel=whatsapp` end to end.**
   `tests/ingest.rs` and `tests/dispatcher.rs` hardcode `"sms"` throughout. The claim above is
   true by code reading, not by a runnable check — this ticket adds that check.
2. **Email's richer format is out of scope, by decision (confirmed at refinement).**
   `template.body` stays the only content field; email ships body-only, same render path as
   SMS/WhatsApp. No `subject` column, no HTML. Revisit only if a real email vendor (still an
   open question, Still Open #4) actually requires one.
3. **No local dev wiring** documents running the dispatcher against more than one channel
   (`.env.example` only shows `DISPATCHER_CHANNELS=sms` / `DISPATCHER_SMS_BASE_URL`).

Out of scope, unchanged: provider failover (§12.1, T-051, SMS-specific), bulk/campaign sending
(T-053).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-046-remaining-channels-email-and-whatsapp-senders
```

Local WIP commits as you go; publish only per the project's commit policy (no push/MR without
explicit user approval — `AGENTS.md`).

### Prerequisite gate (hard)

None. No `depends-on:`, clean working tree on `main`.

### Confirmed design decisions (do not deviate without asking)

1. **Email ships body-only for this ticket — no subject-line or HTML support.**
   User-confirmed at refinement (2026-09-19). `template.body` stays the only content column;
   email renders through the identical path SMS and WhatsApp already use. Revisit only when a
   real email vendor is chosen (Still Open #4) and actually requires a subject.
2. **No new `Sender` implementation and no per-channel adapter.** Email and WhatsApp reuse
   `sender::http::HttpSender` exactly as SMS does — same generic mock-vendor HTTP contract, a
   different `base_url`/credential per channel via the existing `provider_config` row. Do not
   write an `EmailSender`/`WhatsAppSender` type.
3. **No destination-format validation is added**, for any channel. Matches the existing,
   deliberate absence of one (§4.8) — do not add an email-regex or E.164 check as part of this
   ticket.
4. **Provider selection remains open** (Still Open #4) for all three channels equally; this
   ticket does not pick a real vendor for email or WhatsApp any more than T-012 picked one for
   SMS.

### Tasks

#### Task 1 — Ingest-level proof for email and WhatsApp (`tests/ingest.rs`)

Add two new `#[tokio::test]` functions (mirroring
`create_comms_writes_ledger_and_outbox_and_idempotency_replays`'s shape, trimmed to what's
being proven here), one for `channel = "email"` and one for `channel = "whatsapp"`:

- `setup(...)` reuses the `template_id = "balance-alert"` / `version = 1` / `locale = "en-US"`
  template `setup()` already approves for `"sms"` — do **not** call `approve_template` again for
  `"email"`/`"whatsapp"`. `template` is primary-keyed on `(template_id, version, locale)` only
  (`migrations/tenant/0005_template.sql:17`); `channel` is a stored column but is not part of
  the key and `template_repo::find` (`src/template/repo.rs:6-24`) never filters on it, so a
  second approval under the same id/version/locale is rejected by `approve_template`'s own
  immutability check (T-010 decision 5), and would be redundant even if it succeeded — the
  request's `channel` field is what drives `validate_channel`/`kind_for_channel`, not the
  template row.
- Register a producer, build the request body from `sample_body(customer_id)` with
  `["channel"]` and `["destination"]` overridden (an email-shaped and a whatsapp-shaped
  destination string respectively — these are opaque strings to the system, per decision 3
  above, so any plausible value is fine).
- POST it, assert `201`, then assert the `comms_request.channel` column and the resolved
  `customer_address.kind` row (`email` / `whatsapp` respectively) — proving
  `kind_for_channel` and `validate_channel` actually work end to end for these two values, not
  just by inspection.

#### Task 2 — Dispatch-level proof for email and WhatsApp (`tests/dispatcher.rs`)

Add two new `#[tokio::test]` functions mirroring
`successful_send_writes_sent_event_and_final_status_and_deletes_the_outbox_row`, one per
channel:

- Write an outbox row with `channel = "email"` / `"whatsapp"` (either thread a `channel`
  parameter through a small new helper, or write a short dedicated one — do not retrofit
  `channel` onto `write_outbox_row_with_verification` itself, it has ~15 existing call sites
  that all mean `"sms"`).
- Claim it with `repo::claim(&tenant.tenant_pool, "email"/"whatsapp", ...)`, run `try_process`
  against an `HttpSender` pointed at a `wiremock` mock server (same pattern as the existing SMS
  test), and assert `final_status = "sent"` and the resulting `comms_event` row — proving the
  claim query and `try_process`'s gate chain have no hidden SMS-only assumption.

#### Task 3 — Local dev wiring (`.env.example`)

Update the `messgr-dispatcher` block (currently lines 22–24) to show all three channels:
`DISPATCHER_CHANNELS=sms,email,whatsapp` plus `DISPATCHER_EMAIL_BASE_URL` and
`DISPATCHER_WHATSAPP_BASE_URL` example values alongside the existing
`DISPATCHER_SMS_BASE_URL` — each still needs its own `provider_config` row and Vault
credential via the existing `messgr-control provider-config set`, which already accepts all
three channel names.

#### Task 4 — Record the decision (`development/design/14-decisions-and-open-questions.md`)

Add decision **#31**: email and WhatsApp reuse SMS's generic `Sender` contract and
channel-parametric gate chain as-is, with body-only template rendering — no per-channel
adapter, no subject-line field — so this isn't relitigated the next time someone reads step 11
as "unbuilt." Cite T-046. Leave Still Open #4 unchanged — it already covers all three channels.

### Acceptance test

- `just test` green, including the four new tests (`tests/ingest.rs`'s two new
  `channel=email`/`channel=whatsapp` tests, `tests/dispatcher.rs`'s two new matching tests).
- `just lint` clean.
- `just docs-check` clean (decision #31 is added, no cross-reference left stale).
- Manual sanity (optional but cheap): run `messgr-dispatcher` locally with
  `DISPATCHER_CHANNELS=sms,email,whatsapp` per the updated `.env.example` and confirm all three
  claim loops start (log line `starting claim loop` once per channel).

### Docs update (mandatory when user-facing)

`development/design/14-decisions-and-open-questions.md` gets decision #31 (Task 4). No other
design doc changes — build-order §14 step 11 itself carries no per-step "done" marker to flip
(status is tracked via `tickets/BOARD.md`, not inline in `13-build-order.md`).

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just docs-check` clean.
2. Docs updated per Task 4.
3. Write a summary: files touched, and call out explicitly that no production code path
   changed — this ticket is test coverage, dev-config, and a design-doc decision record.
4. Suggested commit message, e.g.:

   ```
   test(dispatcher): prove email and whatsapp already flow end to end (T-046)

   Both channels were already accepted by ingest validation, resolved via
   kind_for_channel, and dispatched via the same generic HttpSender as SMS —
   this was true by code reading only. Adds the missing end-to-end coverage,
   documents local dev wiring for all three channels, and records the
   reuse-not-rebuild decision so step 11 isn't read as unbuilt again.
   ```

5. Commit locally on the branch (no tidy-to-atomic step needed for a change this size, but
   fine to squash to one commit before presenting).
6. Present the commit message for approval; do not push or open a merge request without it.
   Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 11, remaining gap identified when auditing unticketed steps against the board
- 2026-09-19 — TO DO → READY: plan complete
- 2026-09-19 — plan amended inline: Task 1 dropped the
  per-channel `approve_template` call — `template` is keyed on `(template_id, version, locale)`
  only, not `channel`, so approving `"balance-alert"` v1/en-US a second time for `"email"`/
  `"whatsapp"` is rejected as already-approved by `approve_template`'s own immutability check,
  discovered when the new tests failed on first run. Reuses the one template `setup()` already
  approves for `"sms"` instead; `template_repo::find` never filters on `channel` so this proves
  the same thing. Not a retraction of a confirmed design decision (§0-§4 decisions untouched),
  just a wrong task-level instruction in the plan's prose.
- 2026-09-19 — READY → IN DEVELOPMENT: picked up
- 2026-09-19 — IN DEVELOPMENT → IN REVIEW: acceptance green
