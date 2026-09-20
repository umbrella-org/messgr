---
id: T-047
title: Webhook receiver and delivery-receipt ingestion (messgr-webhook)
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-047 — Webhook receiver and delivery-receipt ingestion (messgr-webhook)

## Outcome

After this ships, message status in the ledger and UI reflects what the provider actually did
(`delivered`, `bounced`, ...), not just "accepted by the provider" at send time, and bounces/
complaints feed suppression automatically instead of needing a manual loop.

## Description

Build-order step 12 (§14): `messgr-webhook`, the one internet-facing binary in an otherwise
internal system (`09-delivery-receipts.md` §10). Scope per that section:

- Separate minimal binary in the DMZ; signature verification and nothing else; a narrowly-scoped
  DB write path.
- Routes on an opaque per-tenant `webhook_token` (already provisioned in the control-DB `tenant`
  table, `03-data-model.md` §4.11 — "unread until messgr-webhook ships (step 12)"), never the
  tenant slug, so a public callback path leaks nothing.
- Tolerates duplicate, out-of-order, and early-arriving receipts: `comms_event`'s natural-key
  unique constraint plus `ON CONFLICT DO NOTHING` handles duplicates; the UI renders by
  `occurred_at`; a receipt with no matching `provider_ref` goes to `orphan_event`, whose
  reconciliation loop already shipped (T-030) and needs no rework here.

**Encryption placement — resolved during refinement (was open in `09-delivery-receipts.md` §10).**
`messgr-webhook` never holds any tenant's Vault credentials. It verifies the signature and writes
the raw payload into a new, narrowly-scoped staging table (`webhook_receipt_staging`) in the
target tenant's database — its only SQL statement. A new internal-only `messgr-control`
subcommand (run by cron, mirroring T-029/T-030's shape) resolves the customer, encrypts under
their DEK exactly as `orphan_reconcile`'s existing promotion path already does, and writes the
real `comms_event` row — or, if the `provider_ref` isn't known yet, hands the row to the existing
`orphan_event` table (unencrypted, the documented exception at `03-data-model.md:215`) for T-030's
reconciler to pick up on its own schedule. The exposure window is the staging table's dwell time
between the DMZ write and the next promoter run — bounded by the cron cadence, and the staging
table lives only inside the tenant database, never in the DMZ. See the Implementation Plan's
Confirmed design decisions for the full reasoning and the alternatives this ruled out.

Soft coupling: shares network-segmentation and firewall groundwork with whatever infrastructure
conversation on-prem deployment already requires (flagged in the same design section as usually
the longest-lead item).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-047-webhook-receiver-and-delivery-receipt-ingestion
```

Root-path child (`path = "."`) — tidy WIP commits into atomic ones before presenting; publish
only per the project's commit policy (no push/MR without explicit user approval).

### Prerequisite gate (hard)

None. `T-030` (orphan-event reconciliation) and `T-038` (suppression gate) are both merged to
`main` — this ticket reuses code from the former and feeds the latter, but declares no
`depends-on:` since both are already `6-done/` and merged.

### Confirmed design decisions (do not deviate without asking)

1. **Encryption placement: staging table + internal reconciler, not a DMZ-held Vault credential.**
   `messgr-webhook` writes the raw payload into a new tenant-database table,
   `webhook_receipt_staging`, and nothing else. A new one-shot `messgr-control` subcommand,
   invoked by cron, does the encrypt-and-promote step. This was the user's explicit choice among
   three options during refinement (the alternative that gave the DMZ binary per-tenant
   decrypt/encrypt AppRoles was rejected — see `09-delivery-receipts.md` §10 — as was a
   synchronous internal RPC to a separate encryptor service, which would put a new internal
   dependency on the DMZ binary's hot path for no reduction in *which* process ends up holding
   every tenant's decrypt capability).
2. **Provider selection stays open; build against a generic verifier.** Signature verification is
   a `WebhookVerifier` trait with one concrete implementation (shared-secret HMAC-SHA256 over the
   raw body), proven against a mock provider in tests — the same treatment T-012 gave the
   still-open "provider selection" question (Still Open #4) on the outbound side. A real vendor's
   actual signature scheme (Twilio, SendGrid, ...) is follow-up work for whenever a provider is
   chosen, not this ticket.
3. **No new per-binary Postgres role.** Every existing binary (`messgr-ingest`,
   `messgr-dispatcher`, `messgr-control`) already connects through one shared application-level
   Postgres role per tenant database; there is no existing infrastructure for per-binary
   least-privilege DB roles, and building one is out of scope here. The Description's
   "narrowly-scoped DB write path" is delivered as an application-level property instead:
   `messgr-webhook`'s only SQL statement, ever, is a single `INSERT` into
   `webhook_receipt_staging`.
4. **No `TenantRegistry` reuse.** `messgr-webhook` resolves its tenant by `webhook_token`, not by
   mTLS producer identity, and must not carry `TenantRegistry`'s DEK cache, tenant pepper, or
   kill-switch poll loop — all irrelevant to writing one staging row, and each is one more thing
   the platform's most exposed process would hold in memory. It gets its own minimal
   `Uuid -> PgPool` cache (Task 4).
5. **Promotion runs on a cron cadence, not a new long-lived listener.** Mirrors T-029/T-030's
   existing one-shot-`messgr-control`-subcommand-invoked-by-cron shape rather than introducing a
   persistent `LISTEN`/`NOTIFY` process. The recommended cadence (documented in Task 7's docs, not
   hardcoded) is the latency bound between "provider called the webhook" and "status visible" —
   tighten it there first if it's ever too slow, before reaching for a listener process.
6. **Cost stays `L`.** The promoter reuses `orphan_reconcile`'s existing `find_match` and
   encrypt-and-promote logic (Task 5) rather than inventing new machinery, and the new binary is
   the same shape as the four that already exist (Task 4). Re-checked against the backlog at
   refinement time; no re-grade needed.

### Tasks

#### Task 1 — Staging table migration

`migrations/tenant/0018_webhook_receipt_staging.sql`:

```sql
-- Holding area for a webhook receipt between messgr-webhook's write (DMZ, unencrypted --
-- see DESIGN.md §10 and T-047's Description) and the next webhook-promote run, which either
-- encrypts it into comms_event under the matched customer's DEK or, if the provider_ref isn't
-- known yet, hands it to orphan_event (the existing, narrower plaintext exception, §4.4) for
-- T-030's reconciler. A named erasure exemption like orphan_event: no customer_id is known yet,
-- so there is no DEK to encrypt under and nothing for erasure to key on.
CREATE TABLE webhook_receipt_staging (
    id                   uuid        PRIMARY KEY,
    received_at          timestamptz NOT NULL,
    provider             text        NOT NULL,
    provider_ref         text        NOT NULL,
    occurred_at          timestamptz NOT NULL,
    event_type           text        NOT NULL,
    provider_status      text,
    provider_payload_raw jsonb       NOT NULL,
    UNIQUE (provider, provider_ref, event_type, occurred_at)
);
CREATE INDEX ON webhook_receipt_staging (received_at);
```

Add `webhook_receipt_staging` to T-024's CI check (find it via `find . -iname "*erasure*"` under
`tests`/`scripts`) as a named exemption next to `orphan_event`, with the same reasoning.

#### Task 2 — Resolve tenant by webhook_token

`src/tenant/repo.rs`: add `find_by_webhook_token(pool: &PgPool, webhook_token: &str) ->
Result<Option<Tenant>, sqlx::Error>`, same shape and column list as the existing `find_by_slug`
(`src/tenant/repo.rs:7`) / `find_by_id` (`:26`).

#### Task 3 — `WebhookVerifier` trait + generic implementation

New module `src/webhook_verify/mod.rs`: a `WebhookVerifier` trait
(`fn verify(&self, secret: &[u8], body: &[u8], signature_header: &str) -> bool`) and one
implementation, `HmacSha256Verifier` (shared-secret HMAC-SHA256 over the raw body, constant-time
comparison — reuse whatever HMAC crate `src/destination_hmac.rs` already depends on rather than
adding a new one). The shared secret is read from the same Vault KV engine already used for
provider credentials (§7.6, `src/provider_config`), keyed per tenant.

#### Task 4 — `messgr-webhook` binary

New module `src/webhook/` (`mod.rs`, `handler.rs`) plus `src/bin/webhook.rs`; add
`[[bin]] name = "messgr-webhook" path = "src/bin/webhook.rs"` to `Cargo.toml`. Route:
`POST /webhook/:webhook_token/:provider`. Handler, in order:

1. `tenant_repo::find_by_webhook_token` against the control pool. Unknown token → a flat 404,
   identical in shape to any other unmatched route (§10: the response must not distinguish
   "unknown token" from "no such path" for an unauthenticated caller).
2. Fetch that tenant's webhook shared secret from Vault KV and run Task 3's verifier against the
   raw body and the provider's signature header, before parsing anything. Mismatch → 401, no DB
   write.
3. Parse the provider's payload into the generic receipt shape (`provider_ref`, `event_type`,
   `occurred_at`, `provider_status`, raw JSON) — same normalization T-012's generic outbound
   adapter already assumes.
4. Get-or-open that tenant's pool from a new `webhook::TenantPoolCache`: a
   `HashMap<Uuid, PgPool>` behind a `tokio::sync::RwLock`, populated via the existing
   `tenant::pool::connect_tenant_pool`. `ponytail: no idle-TTL eviction yet (unlike
   TenantRegistry's T-031 sweep) — add one if an offboarded tenant's pool needs dropping without
   a process restart.`
5. `INSERT INTO webhook_receipt_staging (...) ON CONFLICT (provider, provider_ref, event_type,
   occurred_at) DO NOTHING` — a provider's own retries before the next promoter run are free,
   same reasoning as `comms_event`'s own constraint.
6. Return 200 once the `INSERT` commits, regardless of whether promotion has happened yet — the
   provider only needs "received" (§10).

TLS: `axum_server::bind_rustls` with a plain TLS config, **no** client-cert acceptor (unlike
`messgr-ingest` — providers authenticate by signature, not mTLS). Bind a second, non-TLS listener
for `health::router()` (`src/health.rs`, T-032), matching `messgr-ingest`/`messgr-dispatcher`'s
existing dual-listener pattern. New `.env.example` entries: `WEBHOOK_LISTEN_ADDR`,
`WEBHOOK_TLS_CERT_FILE`, `WEBHOOK_TLS_KEY_FILE`, `WEBHOOK_HEALTH_LISTEN_ADDR`.

#### Task 5 — Promote staging rows into `comms_event` or `orphan_event`

Extract the "insert into `comms_event`, conditionally advance `comms_request.final_status`" half
of `orphan_reconcile::repo::promote` (`src/orphan_reconcile/repo.rs:109`) into a standalone helper
taking the individual fields it needs instead of a `PendingOrphan`, so both callers — the existing
`promote` (which then deletes the `orphan_event` row) and the new staging-promoter (which deletes
the `webhook_receipt_staging` row instead) — share one `INSERT` statement instead of two copies
free to drift apart.

New module `src/webhook_receipt/` (`repo.rs`, `promote.rs`) plus a `run_for_tenant` entry point
mirroring `orphan_reconcile::reconcile::run_for_tenant`'s resolve/connect/close shape
(`src/orphan_reconcile/reconcile.rs:139`). For each pending staging row: call the existing
`orphan_reconcile::repo::find_match`. On a match, encrypt (reuse
`customer_dek::lifecycle::get_or_create_dek` + `encryption::encrypt`, exactly as
`orphan_reconcile::reconcile::promote_match` already does) and call Task 5's shared insert helper,
then delete the staging row. On no match, insert the row into `orphan_event`, unencrypted (the
documented exception, `03-data-model.md:215`), for T-030's existing reconciler to pick up on its
own schedule, then delete the staging row.

#### Task 6 — CLI wiring

`src/bin/control.rs`: add `Command::WebhookPromote { command: WebhookPromoteCommand }` and
`WebhookPromoteCommand::Run { tenant_slug: String }`, mirroring `Command::OrphanReconcile` /
`OrphanReconcileCommand::Run` (around lines 166, 670, 1563 in the current file) exactly, including
the "this one needs Vault, unlike idempotency-sweep" comment already on that match arm.

#### Task 7 — Docs

- New `docs/user-manual/webhook.adoc` (mirrors `ingest.adoc`/`dispatcher.adoc`): the route, env
  vars, the webhook-token-not-slug rotation note (§10), the encryption-placement decision (so an
  operator finds the exposure-window reasoning here, not by rereading `DESIGN.md`), and the
  recommended `webhook-promote` cron cadence (start at every 1 minute; this is the tunable that
  bounds "provider called the webhook" → "status visible" latency).
- `docs/user-manual/control-plane-cli.adoc`: new `webhook-promote run` section, same shape as the
  existing `orphan-reconcile run` section (around line 141).
- `docs/user-manual.adoc`: add `include::user-manual/webhook.adoc[leveloffset=+1]`.
- `development/design/09-delivery-receipts.md` §10: replace the "not yet chosen" paragraph with
  the resolved decision, citing T-047.
- `development/design/14-decisions-and-open-questions.md`: add a "Decisions taken" row for the
  encryption-placement choice, citing §10 and T-047.

### Acceptance test

```
just build
just lint
just docs-check
just test   # add an integration test module covering:
            # 1. a correctly-signed mock receipt -> a webhook_receipt_staging row appears;
            #    running `messgr-control webhook-promote run --tenant-slug <slug>` then removes
            #    it and leaves a matching comms_event row whose ciphertext decrypts back to the
            #    original payload under the customer's DEK
            # 2. the same, but with a provider_ref not yet known -> the row lands in
            #    orphan_event instead, unencrypted, after webhook-promote runs
            # 3. an unsigned or mis-signed receipt -> 401, no webhook_receipt_staging row written
            # 4. the same signed receipt posted twice -> only one webhook_receipt_staging row
            #    (or, if already promoted, no duplicate comms_event row)
```

### Docs update (mandatory when user-facing)

See Task 7 above.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just lint`, `just docs-check` clean.
2. Docs updated and registered (Task 7).
3. Write a summary: files touched, decisions made (the six above), anything deferred (the
   `ponytail:` note in Task 4).
4. Suggested commit message:

   ```
   feat(webhook): add messgr-webhook receiver and delivery-receipt promotion (T-047)

   Adds the DMZ-facing webhook receiver, its generic signature-verification path, the
   webhook_receipt_staging table, and the internal webhook-promote CLI subcommand that
   encrypts and promotes staged receipts into comms_event (or orphan_event when the
   provider_ref isn't known yet), reusing T-030's existing match/encrypt/promote logic.
   ```

5. Tidy WIP commits into atomic ones (root-path child) before presenting.
6. Commit locally on the ticket branch. Do not push or open a merge request without explicit
   user approval; verify the remote base is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` prints nothing) before pushing once
   approved. Hand back to the user.

## Review

- [x] Reviewer independence settled (step 0): the reviewing session began with no memory of
  writing this branch (a fresh session per the conversation's own history) — the protocol's
  next-best handoff after a spawned independent reviewer. Audits below were run directly by
  this session, not delegated further.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a)
- [x] Docs-readability pass — conscious skip: no docs-readability reviewer configured in this
  session/host (step 4b)
- [x] Findings recorded with severity, class, and disposition; disposition summary + cost line
  below (step 5)
- [x] Ticket moved to `tickets/5-rework/`; `## History` appended (step 6)
- [x] Other references updated; governing documents reconciled (step 7)
- [x] Remaining-tickets impact sweep done (step 8) — no `1-to-do/`/`2-ready/` ticket depends on
  or references T-047
- [x] Summary + commit message presented for approval; next-ticket suggestion (step 9)

On `main` (`layout = "in-tree"`), so this ticket was read and is recorded here per the base
branch, not the feature branch — the branch's own worktree copy was stale (still showed IN
DEVELOPMENT), exactly the hazard `resources/review-protocol.md`'s intro box describes.

Build/lint/docs-check/test all re-run against `feat/T-047-webhook-receiver-and-delivery-receipt-ingestion`
(`just build`, `just lint`, `just docs-check`, `just test`) — all green, including the four
acceptance-test scenarios (`tests/webhook.rs`).

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | T-038 explicitly deferred "wiring automatic population [of suppression] from delivery receipts" to build-order step 12/this ticket (`tickets/6-done/T-038-suppression-gate-at-dispatch.md:40-46`), and this ticket's own Outcome/Description promise it ships here ("bounces/complaints feed suppression automatically instead of needing a manual loop"). The Implementation Plan's seven tasks never wire any bounce/complaint event into the `suppression` table — `webhook_receipt::promote` and `orphan_reconcile` only ever write `comms_event`/`orphan_event`; nothing in the tree calls `suppression::configure`. §5's suppression list (hard bounce, complaint, regulatory) stays manual-CLI-only for the two sources this ticket was supposed to automate. | `src/webhook_receipt/promote.rs`, `src/orphan_reconcile/reconcile.rs` (no `suppression` reference anywhere in either); `src/dispatcher/repo.rs:363` only *reads* `suppression`, confirming nothing else writes it; `grep -rn "automatic population\|feed suppression" tickets/` returns only T-038's deferral and this ticket's own unmet promise | Scoped rework: when a promoted receipt's `event_type` is a hard bounce or complaint, also insert/upsert a `suppression` row (reason `hard_bounce`/`complaint`, `review_at` per T-038's existing expiry convention) via the existing `suppression` module. `orphan_reconcile::repo::find_match`'s `Match` will need `destination_hmac` added (from `comms_request`) to key the suppression row correctly. Either fix in this ticket's scope, or — if genuinely deferred again — update the Outcome/Description to say so explicitly and record why, rather than silently shipping less than promised. |
| F2 | non-blocking | stale-xref | `development/design/03-data-model.md`'s `tenant.webhook_token` column comment still read "unread until messgr-webhook ships (step 12)" — false as of this branch, which is exactly what reads it (`src/tenant/repo.rs::find_by_webhook_token`). | `development/design/03-data-model.md:491` (pre-fix) | Fixed inline: reworded to "resolved by messgr-webhook (T-047)", commit `0dd44e0` (on the feature branch). |
| F3 | non-blocking | stale-xref | `development/design/13-build-order.md`'s named-exemption list for customer-data tables (step 12's own CI-check description) listed `suppression`, `orphan_event`, `customer_dek`, `customer_alias`, `outbox`, `customer` but omitted `webhook_receipt_staging`, which this branch adds as a named exemption in `tests/erasure_coverage.rs`. | `development/design/13-build-order.md:34` (pre-fix) | Fixed inline: added `webhook_receipt_staging` to the list, commit `0dd44e0` (on the feature branch). |
| F4 | non-blocking | test-gap | No test covers the unknown-`webhook_token` → flat 404 path (`src/webhook/handler.rs`'s first branch) or the Vault-secret-read-failure → 401 path — both real branches in `receive_webhook`, neither in the ticket's own 4-scenario acceptance test. | `src/webhook/handler.rs` (`receive_webhook`, the `Ok(None) =>` and Vault-error arms); `tests/webhook.rs` (4 tests, none exercising either) | Noted — small, and every other branch in the same handler is covered; not worth a follow-up ticket on its own. |

Disposition summary: 1 blocking (F1, routes to rework), 2 fixed inline (F2, F3), 1 noted (F4).

cost: estimated L, actual L

### Rework fix record — round 1 (commit `669ea28`)

F1: `orphan_reconcile::repo::insert_comms_event` — the shared funnel both `orphan_reconcile::repo::promote`
and `webhook_receipt::repo::promote` already route through — now upserts a `suppression` row when
the promoted `event_type` is `bounced`/`complaint`, in the same transaction as the `comms_event`
insert. `Match` gained `destination_hmac` (from `comms_request`, per F1's own suggestion) to key
the row. `review_at` is `now() + 365 days` (confirmed with the user: no existing default existed
anywhere in the codebase — `suppression add`'s `--review-at` is operator-supplied with no fallback
— and this is a compliance-adjacent policy call, not one to assume). The conflict update only
lengthens `review_at` (`WHERE EXCLUDED.review_at > suppression.review_at`), never shortens a
longer-standing entry such as a manually-set `regulatory_hold`, matching §5's fail-safe direction
for suppression as a whole.

New acceptance scenario: `tests/webhook.rs::a_bounce_receipt_auto_suppresses_the_destination`
(a signed `bounced` receipt promotes, then a matching `suppression` row appears with
`reason = hard_bounce` and `review_at` ~1 year out). `just build`, `just lint`, `just docs-check`,
`just test` all green, including this new test and the existing four scenarios.

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 12, remaining gap identified when auditing unticketed steps against the board
- 2026-09-19 — TO DO → READY: plan complete
- 2026-09-19 — READY → IN DEVELOPMENT: picked up
- 2026-09-19 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-19 — IN REVIEW → REWORK: F1 blocking: auto-suppression from bounce/complaint receipts never wired, contradicting the ticket's own Outcome and T-038's explicit deferral
- 2026-09-20 — REWORK → IN REVIEW: findings fixed
