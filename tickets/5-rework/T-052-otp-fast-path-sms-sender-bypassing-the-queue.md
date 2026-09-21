---
id: T-052
title: OTP fast path: sms-sender bypassing the queue
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: high
cost: L
---

# T-052 — OTP fast path: sms-sender bypassing the queue

## Outcome

After this ships, customer login stops depending on the messaging platform at all — a marketing
incident, a full outbox, or a dead dispatcher can no longer lock a customer out of the bank
(AGENTS.md hard invariant #1), because OTP sends go through `sms-sender`, synchronous and
outside the queue/gate chain entirely, while still appearing in messgr's ledger and UI.

## Description

Build-order step 17 (§14), deliberately last among the pre-cloud steps — "because it touches the
auth flow and should not be the thing shaking out bugs in the sender adapters." Builds the
on-prem shape of §3's OTP design (`02-otp.md`):

- `sms-sender` talks to the SMS provider synchronously — no queue, no gate chain, no dispatcher
  involvement.
- The audit record write to `comms_request`/`comms_event` is asynchronous and best-effort: if
  Postgres is unavailable, the send still succeeds and the record is buffered to local disk and
  backfilled. `payload_ciphertext` is NULL for the auth class (§7.4) — the code is a live
  credential and must never be retained.
- Quiet hours are not evaluated on this path — OTP is exempt by policy (§6, decision-table
  confirmation already reflected in T-043's `quiet_hours_policy`, which OTP never reads).
- Quotas never apply either (decision 11, §5.1 — auth is exempt, and this path never reaches the
  dispatcher where quota counters live anyway).

**Correction to §3, confirmed with the user during refinement.** §3 describes on-prem
`sms-sender` as "a thin Rust library (or a dedicated single-purpose HTTP service if the calling
auth service is not Rust)... linked into the bank's own auth service — same process, no network
hop." That assumed the bank writes or can link foreign code into its own auth service. The
actual calling auth system for this deployment is a **third-party product** — nothing can be
linked into it. `sms-sender` is therefore built as the HTTP-service branch §3 already
anticipated, never the library branch: a new binary in this repo (`messgr-sms-sender`, see
Implementation Plan), called synchronously over the bank's own internal network. A message queue
was considered and rejected outright — it is the exact mechanism AGENTS.md hard invariant 1
exists to keep OTP off ("any proposal that routes OTP through the outbox is a regression"), and
would reintroduce the shared-fate problem §3's whole design avoids. This local HTTP hop is still
categorically different from cloud's `otp-api` (§3.1): single-tenant, on the bank's own network,
no dispatcher/queue anywhere in the path — `otp-api` crosses a real multi-tenant network/trust
boundary to a shared regional service. §3 should be corrected in place to state the HTTP-service
shape as on-prem's actual mechanism, not a fallback for a non-Rust caller — task 1 below.

**Gap found in §4.1's schema, not previously covered anywhere in the design doc.**
`comms_request.template_id`/`template_version` are `NOT NULL` on every row, but OTP never
renders a template — `payload_ciphertext` is NULL for auth class (§7.4) and the code itself must
never be templated or retained. Resolved: a fixed sentinel template row
(`template_id = 'otp'`, `version = 1`, empty body, never rendered or read) approved once per
tenant via the existing `messgr-control template approve` command (T-010) — task 6 below adds a
correction note to `03-data-model.md`.

**`tenant.auth_enabled` (§5.2, migration `0005_tenant_auth_enabled.sql`) gets its first reader
here.** The column has existed unread since T-016; its own comment mandates failing *open* on a
read failure or timeout ("a control-database outage must never silently disable customer
login"). Confirmed with the user: dual control stays the out-of-band `psql` + `platform_audit`
runbook already established by decision 29 for steps 4 and 14 — `sms-sender` only reads the flag
and refuses to call the provider while it is `false`; no mechanical two-person-approval tooling
is built here. This resolves Still Open #9 in `14-decisions-and-open-questions.md` for step 17 —
task 7 below closes it out.

**`customer_id` is supplied directly by the caller, not resolved.** Confirmed with the user:
the auth service already holds messgr's own `customer_id` and passes it as-is on every request.
No `customer::resolve` call, no provisional-customer minting on this path — matches §4.8's "no
customer_id linkage beyond what the auth service already knows." An unresolvable/garbage id is
still written as-is (§4.7: "never reject a send because resolution failed" — and by the time the
audit write happens, the OTP has already been sent or has already failed either way).

**Provider selection reuses `provider_config` (T-012) directly, not T-051.** T-051 (SMS provider
failover) is still in `1-to-do/` and, checked while researching this ticket, its Description's
claim that "each dispatcher already runs a per-provider circuit breaker... plus exponential
backoff with jitter" is only half true — `grep` finds the backoff (T-021) but no circuit breaker
anywhere in this codebase yet. Flagging for whoever refines T-051 next; not fixed here, out of
this ticket's scope. Given that, `sms-sender` does not depend on or wait for T-051: it walks
`provider_config` rows for `(channel = 'sms')` in priority order **within a single synchronous
send**, trying the next provider immediately on failure, with no persistent per-provider
state — a live request the customer is waiting on needs "try the next one now," not the
windowed circuit-breaker/half-open-probe behaviour that's the dispatcher's own job for queued
traffic that can afford to wait. This is the "reuse where practical" the original Description
pointed at: the same ordered table, a much smaller mechanism than what T-051 will eventually
build for the dispatcher. No hard `depends-on:` — same conclusion T-051's own Description
already reached, kept here as a soft coupling only.

Out of scope: `otp-api`, the cloud-only network-hop variant of this same mechanism (§3.1) — that
is part of T-054 (cloud enablement, step 19), not this ticket.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-052-otp-fast-path-sms-sender-bypassing-the-queue
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1. Does not
require T-051 to be merged first — see the Description's provider-selection decision.

### Confirmed design decisions (do not deviate without asking)

1. **`sms-sender` is a new binary, `messgr-sms-sender`, not a library.** The calling auth
   service is third-party — nothing can be linked into it. Synchronous HTTP, mTLS-authenticated,
   no queue, no gate chain, no dispatcher, no outbox row ever created for this path.
2. **mTLS/producer/tenant identity is resolved exactly as `messgr-ingest` does it** — a
   `ProducerContext`-shaped extractor built the same way as `src/ingest/identity.rs`, reusing
   `producer::resolve::resolve_producer` and `TenantRegistry::get_or_open`. The auth service is
   registered as an ordinary producer (`messgr-control producer register`, T-005/T-006), no new
   identity mechanism. Satisfies AGENTS.md hard invariant 10 the same way every other caller
   does.
3. **`customer_id` is caller-supplied, never resolved.** The request body carries `customer_id`
   directly; no `customer::resolve` call, no provisional-customer minting, no rejection if it
   doesn't resolve to a real row.
4. **`class` is the literal `"auth"`.** Hardcoded, not imported from `ingest::model::class` —
   that module's `AUTH` constant exists only so `messgr-ingest` can name the value it rejects;
   depending on it from `sms-sender` would be a needless cross-module coupling for one string
   literal.
5. **`payload_ciphertext` is always NULL and no template is rendered.** A fixed sentinel
   `(template_id = 'otp', version = 1)` template row (empty body) is written onto every
   `comms_request` row purely to satisfy the `NOT NULL` columns — approved once per tenant via
   the existing `messgr-control template approve`, not part of this ticket's runtime code path.
6. **`destination_hmac`/`destination_ciphertext` are still computed and still DEK-encrypted.**
   Reuses `destination_hmac::compute` and `customer_dek::lifecycle::get_or_create_dek` exactly as
   `messgr-ingest` does (`src/ingest/handler.rs`'s own calls are the template). The destination
   itself is never resolved against the customer projection (§4.8) — it's the raw value the
   caller supplied, used as-is.
7. **Provider selection is a synchronous, in-request walk of `provider_config`.** Load
   `provider_config` rows for `(channel = 'sms')` ordered by `priority` (existing
   `provider_config::repo`, T-012), try each with `sender::http::HttpSender` (reusing the
   existing `Sender` trait unchanged) until one succeeds or the list is exhausted. No persistent
   circuit-breaker state, no cross-request memory of a down provider — see the Description's
   provider-selection note for why this is deliberately smaller than what T-051 will eventually
   build for the dispatcher.

   **Plan amended inline during implementation.** As written, this loads `provider_config` fresh
   from `tenant.pool` on every send — the same pool decision 9's audit write uses. That makes
   provider *selection*, not just the audit write, depend on the tenant database being reachable,
   contradicting decision 9's "the send still succeeds" (02-otp.md §3) and this ticket's own
   acceptance test, which requires the provider call to be unaffected by a tenant-DB outage.
   Added `sms_sender::provider::ProviderConfigCache`: a per-tenant, last-known-snapshot cache
   (`HashMap<Uuid, Vec<ProviderConfig>>` behind an `RwLock`), refreshed opportunistically on every
   send rather than a poll timer (no natural place to enumerate every tenant this multi-tenant
   process might serve) — the same "fail to the last successful read" shape `AuthEnabledCache`/
   `KillSwitchCache` already use elsewhere in this codebase. `AppState` gained one field,
   `provider_config_cache: Arc<ProviderConfigCache>`.
8. **`tenant.auth_enabled` is read from the control database, fail-open.** One process-wide
   `AuthEnabledCache` (`HashMap<Uuid, bool>` behind an `RwLock`, mirroring
   `kill_switch::cache::KillSwitchCache`'s shape but *not* reusing it directly — `auth_enabled`
   is one query against the shared `control_pool`, not a per-tenant-database poll, so it needs
   no per-tenant task and no `TenantContext` field) refreshes every 5 seconds via one
   `SELECT id, auth_enabled FROM tenant` against `control_pool`, spawned once in `main()`. A row
   missing from the last successful refresh, or a refresh that itself errored/timed out, reads as
   `true` (enabled) — the migration's own fail-open mandate. When the cached value is `false`,
   the handler never calls the provider; it writes a `comms_request`/`comms_event` pair with
   `final_status`/`event_type = "discarded"` (same vocabulary `dispatcher::drain.rs`'s
   kill-switch discard path already uses) and returns an error to the caller.
9. **The audit write is one best-effort transaction, id-idempotent, no idempotency table.**
   `sms-sender` mints `comms_request_id` itself. A direct Postgres write failure appends the
   record (as JSON) to a local append-only buffer file; a background task retries the oldest
   buffered entries on a short interval, rewriting the file with whatever's left after each
   pass. Because the buffered record carries the same `id`/`created_at` on every retry, the
   insert uses `ON CONFLICT (created_at, id) DO NOTHING` on `comms_request` (and relies on
   `comms_event`'s existing `UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)`)
   — no new idempotency table needed; retries are naturally safe.
   `ponytail: single buffer file, full-file rewrite per drain pass — fine at OTP volumes;
   shard by tenant if throughput ever makes the rewrite itself a bottleneck.`
10. **No gate chain, no outbox, no quotas.** `comms_event.event_type` is `"sent"`/`"failed"`,
    written directly from the synchronous provider call's own outcome — never via
    `dispatcher::repo`, whose functions all assume an existing outbox-claimed row.

### Tasks

#### Task 1 — Design-doc correction: §3's on-prem mechanism

`development/design/02-otp.md`: reword the on-prem bullet to state the HTTP-service shape as
what's actually built (a new binary, `messgr-sms-sender`, called over the bank's internal
network — not a library, since the calling auth service is a third-party product nothing can be
linked into), with a short correction note in this doc's existing correction-callout style,
citing T-052. Keep the "no queue, no gate chain, no dispatcher" framing — that part of §3 is
still exactly right, only the "same process, same binary" framing was wrong.

#### Task 2 — New binary scaffold

`Cargo.toml`: add
```toml
[[bin]]
name = "messgr-sms-sender"
path = "src/bin/sms_sender.rs"
```
New module `src/sms_sender/`:
- `mod.rs` — `AppState` (mirrors `src/ingest/mod.rs`: `control_pool`, `control_database_url`,
  `keystore`, `registry`, `tenant_pool_max_connections`, `profile`, plus `auth_flag:
  Arc<AuthEnabledCache>`).
- `model.rs` — request/response types (`customer_id`, `destination`, `body`, `channel` fixed to
  `"sms"`), `IngestError`-equivalent error type.
- `identity.rs` — the `ProducerContext`-shaped extractor from decision 2.
- `auth_flag.rs` — `AuthEnabledCache` + its poll loop (decision 8).
- `provider.rs` — the provider-config-ordered synchronous send walk (decision 7).
- `buffer.rs` — the local-disk buffer file + drain task (decision 9).
- `repo.rs` — `write_audit_record` (one transaction: `comms_request` insert with `final_status`
  already set, `comms_event` insert) and `write_discarded` (decision 8's kill-switch-blocked
  case).
- `handler.rs` — ties it together: extract `ProducerContext`, check `AuthEnabledCache`, resolve
  DEK + compute `destination_hmac`/`destination_ciphertext`, call `provider::send`, attempt
  `repo::write_audit_record`, fall back to `buffer::append` on failure, return the provider
  outcome to the caller regardless of how the audit write went.

`src/bin/sms_sender.rs`: main() bootstrap mirroring `src/bin/ingest.rs` (control pool connect,
keystore construction, `TenantRegistry::new`, spawn the `AuthEnabledCache` poll loop and the
buffer drain task, build the axum app, serve).

#### Task 3 — mTLS + tenant/producer resolution

Adapt `src/ingest/identity.rs`'s `ProducerContext` pattern into `src/sms_sender/identity.rs` —
same `resolve_producer`/`registry.get_or_open` calls, same rejection on a missing peer
certificate. No changes to `src/producer/` or `src/tenant/registry.rs`.

#### Task 4 — `AuthEnabledCache`

`src/sms_sender/auth_flag.rs`: `AuthEnabledCache::refresh(&self, control_pool: &PgPool)` runs
`SELECT id, auth_enabled FROM tenant`, swaps in a fresh `HashMap<Uuid, bool>`. `is_enabled(&self,
tenant_id: Uuid) -> bool` returns `true` for a missing key (never-yet-refreshed or newly
provisioned tenant not seen yet — fail open, decision 8). A `run_refresh_loop`-shaped poll task
(`tokio::time::sleep(Duration::from_secs(5))`, no `LISTEN` — this is a control-DB poll, not a
per-tenant-DB one) is spawned once from `sms_sender.rs`'s `main()`. A refresh error is logged
(`tracing::error!`) and leaves the previous snapshot in place — never clears it to a default,
since that would defeat fail-open on a control-DB blip shorter than one full refresh cycle.

#### Task 5 — Provider send + audit write

`src/sms_sender/provider.rs`: load `provider_config` rows for `(channel = "sms")` via the
existing `provider_config::repo` query, sorted by `priority`; build an `HttpSender` per row
(reusing `sender::http::HttpSender`, resolving `credential_path` via `keystore` exactly as
`messgr-dispatcher` does today); call `.send()` on each in order until one returns
`Ok(SendOutcome)` or the list is exhausted (last error returned).

`src/sms_sender/repo.rs::write_audit_record`: one transaction —
`INSERT INTO comms_request (..., final_status, finalized_at) VALUES (..., $final_status, now())
ON CONFLICT (created_at, id) DO NOTHING`, then `INSERT INTO comms_event (...) ON CONFLICT DO
NOTHING` (relies on the table's own unique index) — `final_status`/`event_type` `"sent"` on a
successful send, `"failed"` otherwise, `provider_ref`/`provider_status` from `SendOutcome` (or
empty string / the last error's status on failure, matching `provider_ref NOT NULL DEFAULT ''`).

`handler.rs`: on `write_audit_record`'s own failure (Postgres unreachable), call
`buffer::append` with the same fully-formed row (task 6) — never retries the provider call
itself; the OTP send already completed one way or the other.

#### Task 6 — Local-disk buffer + drain

`src/sms_sender/buffer.rs`: `append(path: &Path, record: &BufferedRecord)` serializes one JSON
line and appends+flushes synchronously before returning (durability requirement — decision 9).
`drain(path: &Path, control_pool: &PgPool, registry: &TenantRegistry)` reads every line, attempts
`repo::write_audit_record` for each against the record's own tenant pool, and rewrites the file
containing only the lines that failed again — spawned as a loop (interval e.g. 10s) from
`sms_sender.rs`'s `main()`, same shape as task 4's poll loop but against the buffer file instead
of the control DB.

#### Task 7 — `tenant.auth_enabled` reader + docs close-out

`docs/user-manual/kill-switches.adoc`: update the `== The auth kill switch` section — it
currently reads "as of T-016 it exists as schema only... its only intended reader" in a way that
implies no reader exists; correct to state `messgr-sms-sender` is now that reader, on a 5-second
poll, fail-open on a control-DB read failure.

`development/design/14-decisions-and-open-questions.md`: close Still Open #9 for step 17 —
replace its text with a short resolved note ("Resolved for step 17 (T-052): out-of-band, per
decision 29 — `sms-sender` reads the flag and fails open; no mechanical dual-control tooling"),
matching how #9's step-4/step-14 clauses were already struck through inline.

#### Task 8 — Design-doc correction: `template_id`/`template_version` sentinel

`development/design/03-data-model.md`: add a correction note beneath the `comms_request`
snippet (matching the doc's existing correction-callout style) stating that auth-class rows
carry a fixed sentinel `(template_id = 'otp', version = 1)` since OTP renders no real template,
found and resolved during T-052.

#### Task 9 — Producer + template provisioning (operational, not runtime code)

Document the two one-time-per-tenant setup steps in task 7's `docs/user-manual/` addition (a new
`docs/user-manual/sms-sender.adoc`, mirroring `docs/user-manual/webhook.adoc`'s structure): `1)`
register the auth service as a producer (`messgr-control producer register`) and issue it an
mTLS client cert; `2)` approve the sentinel template (`messgr-control template approve
--template-id otp --version 1 --channel sms --locale <tenant default> --body ""`).

### Acceptance test

New `tests/sms_sender.rs`, mirroring `tests/ingest.rs`'s setup (provision a tenant, register a
producer with a dev-PKI cert via `producer::dev_pki`, approve the sentinel `otp` template,
configure one `provider_config` row pointing at a mock HTTP provider):

1. A well-formed request (mTLS cert + `customer_id` + `destination` + `body`) returns success
   from the handler; assert the mock provider received exactly one call. Poll briefly for the
   `comms_request`/`comms_event` rows to appear (the write is best-effort/async) and assert
   `payload_ciphertext IS NULL`, `class = 'auth'`, `final_status = 'sent'`.
2. Point the handler's tenant pool at an unreachable Postgres (or drop the connection mid-test)
   before the call: the handler still returns success (the provider call is unaffected) and the
   record lands in the local buffer file instead. Restore the pool, run one `drain` pass, assert
   the row now exists in `comms_request`/`comms_event`.
3. Set `tenant.auth_enabled = false` on the control DB, wait past one refresh interval (or call
   `AuthEnabledCache::refresh` directly in the test), send a request: assert the mock provider
   received **zero** calls and the written row has `final_status = 'discarded'`.
4. Configure two `provider_config` rows (priority 1, 2) for `sms`; make the mock provider at
   priority 1 return an error: assert the request still succeeds and the mock provider at
   priority 2 received the call.
5. `just build`, `just test`, `just lint`, `just docs-check` all clean.

### Docs update (mandatory when user-facing)

New `docs/user-manual/sms-sender.adoc` (mirrors `docs/user-manual/webhook.adoc`'s structure):
what `messgr-sms-sender` is, how to deploy/run it, the producer-registration and
template-approval one-time setup (task 9), and the `auth_enabled` fail-open behaviour
cross-referenced from `kill-switches.adoc` (task 7). Register the new page in whatever
navigation index `docs/user-manual/introduction.adoc` or the doc build config uses for the
existing per-binary pages.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint`, `just docs-check` clean.
2. Docs updated and registered (see Docs update above).
3. Write a summary of everything done (files touched, decisions made, anything deferred).
4. Suggest a Conventional Commit message, ticket id in brackets at the end of the subject, e.g.:
   ```
   feat(sms-sender): add OTP fast path bypassing the queue (T-052)
   ```
5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits before
   presenting (`path = "."`, root-path child).
6. Commit locally on the ticket branch. Do not push or open a merge request without explicit
   user approval. Present the commit message; only after approval finalize the branch, verify
   `origin/main...HEAD` carries no `tickets/` path, push, and open the merge request — merging
   is always the human's. Hand back to the user.

## Review

- [x] Reviewer independence settled (step 0): **independent** — fresh session (post-`/clear`),
  no memory of authoring this branch; audits run directly, nothing delegated.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2):
  `cargo test --test sms_sender` (4/4 pass), full `just test` (all suites green, no
  regressions), `just build` clean.
- [x] Quality audit (step 3): tests are mutation-resistant (paired assertions, not bare
  `is_err()`); see findings below for the one gap this audit found.
- [x] Consistency audit (step 4): cross-checked against `AGENTS.md` hard invariants 1/3/4/5/10,
  the gate-chain's auth exemptions (§5), and existing `KeyStore`/`ingest`/`dispatcher` patterns;
  see findings below.
- [x] Documentation audit (step 4a): `just docs-check` clean; `docs/user-manual/sms-sender.adoc`,
  `kill-switches.adoc`, `control-plane-cli.adoc` reviewed for coverage and accuracy — all
  correct. One pre-existing staleness found elsewhere in the tree (F4).
- [x] Docs-readability pass (step 4b): **conscious skip** — no docs-readability reviewer
  configured in this host.
- [x] Findings recorded below with severity, class, and disposition (step 5).
- [x] Ticket moved per step 6.
- [x] Governing documents reconciled or reason given (step 7): `02-otp.md`, `03-data-model.md`,
  `14-decisions-and-open-questions.md` corrections in this branch verified accurate against the
  actual implementation. No further governing-doc edit made by this review — F3's gap doesn't
  fit the "made false by this branch" bar for an inline fix; recorded as a finding for the
  eventual erasure ticket (build-order step 15) to pick up instead.
- [x] Remaining-tickets impact sweep done (step 8): T-051, T-053, T-054 (the only tickets
  referencing T-052, all still in `1-to-do/`, unrefined) re-read; no assumption they encode was
  invalidated by what actually shipped.
- [x] Summary + commit message & MR attributes presented for approval (step 9) — see below.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | The OTP send is not actually resilient to a Vault or Postgres outage, contradicting decision 9 / §3's "the send still succeeds" and this ticket's own Outcome ("customer login stops depending on the messaging platform at all"). `handler.rs` calls `get_or_create_dek(...).await?` — which, on a DEK cache-miss, hits `customer_dek` (Postgres) and Vault — *before* the provider call, with a bare `?` and no buffer/fallback: a miss during an outage aborts the whole request (500), nothing sent, nothing buffered. Separately, `provider::send`'s `keystore.read_provider_credential` call is per-request with no cache at all, so a Vault outage blocks every send regardless of DEK-cache state. | `src/sms_sender/handler.rs` (`get_or_create_dek(...).await?` precedes `provider::send`, no fallback); `src/sms_sender/provider.rs` (`read_provider_credential` called fresh every request — contrast `AuthEnabledCache`/`ProviderConfigCache`, which both cache against exactly this kind of outage); `tests/sms_sender.rs`'s `postgres_write_failure_buffers_and_drain_recovers_it` pre-warms the DEK cache specifically "rather than fetched through the (now-broken) pool" — its own comment says this proves only the audit write, not DEK resolution, is what the test breaks; `development/design/04-gate-chain.md`: "the whole design of the OTP path is that it has no dependency on Postgres or Vault being reachable"; real `dek_cache` TTL is 1 hour (`src/tenant/registry.rs:31`), so any customer who hasn't triggered a send in the last hour hits this on their very next OTP. | Cache the provider credential the same way T-054 already specifies for cloud's `otp-api` (fetch at startup, refresh on a timer, not per-request — `02-otp.md`'s own §3.1 correction); give DEK resolution the same best-effort treatment decision 9 gives the audit write, or explicitly narrow decision 9's promise to the warm-cache case if a real fix isn't feasible here. |
| F2 | blocking | correctness | — | `repo::write_audit_record` binds `finalized_at` to the same query parameter as `created_at` (`$3`, reused at the end of the `VALUES` list), so every row this binary ever writes gets `finalized_at == created_at` — never the actual time the DEK/HMAC work and provider call finished. Silently wrong data in a bank audit ledger, exposed verbatim through `query_api`'s `finalized_at` field. No test in `tests/sms_sender.rs` asserts `finalized_at` at all. | `src/sms_sender/repo.rs`: `VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, $10, NULL, $11, NULL, NULL, $12, $3)` — finalized_at bound to `$3` = `created_at`; compare `src/dispatcher/repo.rs`'s own `UPDATE ... SET final_status = $1, finalized_at = $2` using a freshly-captured `now`, and `03-data-model.md`: "setting `final_status` and `finalized_at` when the outbox row reaches a terminal state." | Bind a `Utc::now()` captured at write time instead of reusing `created_at`; add a regression assertion (e.g. `finalized_at >= created_at` under a delay, or simply `!=`) to the acceptance test. |
| F3 | non-blocking | spec-unclear | noted | The new local-disk buffer file (`sms-sender-buffer.jsonl`) is a customer-data storage location — `customer_id`, `tenant_id`, DEK-encrypted `destination_ciphertext`/`destination_hmac` — invisible to the schema-based erasure mechanism `DESIGN.md` §7.2 describes and `tests/erasure_coverage.rs` will eventually check (build-order step 15, not built yet). A record buffered during an outage and drained *after* that customer's DEK is later crypto-shredded lands in Postgres as an ordinary, non-tombstoned row. Erasure isn't built yet, so nothing ships wrong today — this is a design-doc gap, not a live bug — but neither `DESIGN.md` nor this ticket flags it for whoever builds step 15. | `src/sms_sender/buffer.rs` (plain JSON-lines file on local disk); `development/design/06-pii-retention.md` §7.2 (schema-based erasure statements, no filesystem equivalent); `tests/erasure_coverage.rs`'s own header comment ("that module does not exist yet"). | Add a Still Open item to `14-decisions-and-open-questions.md` for build-order step 15 to account for out-of-band buffer files (this one, and any future equivalent) when it designs the erasure job. |
| F4 | non-blocking | docs-gap | noted | `docs/user-manual/introduction.adoc`'s "Status" section ("Three binaries exist today: `messgr-control`, `messgr-ingest`, `messgr-dispatcher`...") and its `version`-subcommand list are already stale — `messgr-webhook` and `messgr-query-api` shipped (T-047/T-048) and are already missing from both. This ticket adds `messgr-sms-sender` as a fourth undocumented case. Pre-existing staleness this branch didn't cause, so not "fixed inline" territory under the rules' causation test. | `docs/user-manual/introduction.adoc` lines 11-19, 22-23. | Whoever next touches that page's Status section should refresh the binary list (ingest, dispatcher, webhook, query-api, sms-sender). Not on its own worth a dedicated ticket. |

Disposition summary: 2 blocking (F1, F2) — not dispositioned, fixed via rework. 2 non-blocking:
`noted` ×2 (F3, F4).

cost: estimated L, actual L.

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 17, remaining gap identified when auditing unticketed steps against the board
- 2026-09-21 — TO DO → READY: plan complete
- 2026-09-21 — READY → IN DEVELOPMENT: picked up
- 2026-09-21 — plan amended inline: added `ProviderConfigCache` (decision 7) so provider selection survives a tenant-DB outage independently of the audit write, matching decision 9 and the acceptance test's "provider call is unaffected" requirement — `provider_config::repo::list` was being read fresh from the same pool the audit write buffers around
- 2026-09-21 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-21 — IN REVIEW → REWORK: 2 blocking findings: OTP send not actually resilient to a Vault/Postgres outage (F1); finalized_at bound to created_at (F2)
