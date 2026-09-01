---
id: T-013
title: Minimal dispatcher: per-channel claim loop, LISTEN/NOTIFY wakeup, comms_event write
project: messgr
depends-on: [T-011, T-012]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-013 — Minimal dispatcher: per-channel claim loop, LISTEN/NOTIFY wakeup, comms_event write

## Outcome

After this ships, `messgr-dispatcher` picks up a real `outbox` row for the one tenant/channel
it runs against, calls the configured `Sender`, and closes the loop: the outcome lands as one
`comms_event` row and one `comms_request.final_status` update, and the `outbox` row is deleted.
One process per tenant, one attempt per message — no leader election, no retry/backoff, no
rate-limit enforcement (all later build-order steps) — proving the queue mechanics DESIGN.md
§4.2/§9 describe end to end, matching step 2's "a system that always sends" scope. This is the
ticket that completes the first real end-to-end send in build step 2.

## Description

Build the minimal dispatcher per design §4.2, §4.1, §9: a per-channel claim loop using leases
and `SKIP LOCKED`, `LISTEN`/`NOTIFY` wakeup with a 1s poll fallback, `comms_event` writes, and a
single `final_status` update. No gates exist yet at this step — the point is proving queue
mechanics end to end, deliberately a system that always sends (design §14 step 2). Part of the
step-2 ticket family (`family: T-007`; see T-007). Depends on T-011 (ingest, so there is
something in the outbox to claim) and T-012 (the `Sender` trait and first adapter it calls).

Four gaps surfaced during refinement, all resolved with the user (recorded as confirmed design
decisions below, not guessed at):

- **`provider_config` has no `base_url` column, and `credential_path` is a Vault path with no
  KV-read plumbing (T-012 decision 3 deferred that).** `HttpSender::new` needs a `base_url` and
  an `api_key`; this ticket sources both from env vars per channel, as a documented dev stand-in
  — `provider_config` itself is not read by this ticket at all.
- **No retry/backoff/circuit-breaker exists yet** (build order step 7). A send failure is
  terminal on the first attempt: `comms_event(failed)` + `final_status = 'failed'`, same shape
  as a success.
- **Leader election (`pg_try_advisory_lock`, §9) is step 7, not this ticket.** This binary
  assumes exactly one instance is run per tenant.
- **`provider_config.rate_limit_per_sec` stays unenforced.** T-012 decision 4 named this ticket
  as the eventual reader; refinement confirmed enforcement is still out of scope — this ticket
  doesn't query `provider_config` at all.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-013-minimal-dispatcher
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented (rules
§0). Do not push and do not open a merge request without explicit user approval. Ticket and board
bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-011` and `T-012` are both in `6-done/` and merged to `main` — confirmed on the board (PR
  #13, PR #14).
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init`.

### Confirmed design decisions (do not deviate without asking)

1. **`Sender` credentials come from env vars, per channel — not from `provider_config`.**
   `DISPATCHER_<CHANNEL>_BASE_URL` / `DISPATCHER_<CHANNEL>_API_KEY` (channel uppercased, e.g.
   `DISPATCHER_SMS_BASE_URL`), required, no defaults. Confirmed with the user during refinement:
   `provider_config` carries no `base_url` column and `credential_path` (Vault KV) has no reader
   yet (T-012 decisions 3–4); this ticket does not touch `provider_config` at all — a later
   ticket makes the dispatcher read it (ordered failover, real credential resolution, rate
   limits).
2. **A send failure is terminal on the first attempt.** `Sender::send` returning `Err` writes
   `comms_event(event_type = "failed")` and `comms_request.final_status = "failed"`, then deletes
   the `outbox` row — exactly the same shape as a success, just a different terminal outcome. No
   requeue, no `attempts`-driven backoff. Confirmed with the user: retry/backoff/circuit-breakers
   are build-order step 7, a later ticket.
3. **No leader election.** `pg_try_advisory_lock`-based HA (§9) is step 7. This binary assumes
   exactly one `messgr-dispatcher` instance is run per tenant; running two against the same
   tenant is a known-unsupported deployment before that ticket lands.
4. **`provider_config.rate_limit_per_sec` is not read or enforced.** Confirmed with the user:
   stays deferred past this ticket (T-012 decision 4's "not enforced yet" still holds).
5. **Wakeup is a database trigger, not an app-level `pg_notify` call.** `AFTER INSERT ON outbox`
   calls `pg_notify('outbox_' || NEW.channel, NEW.comms_request_id::text)` (migration, Task 2).
   A trigger means every future outbox writer (bulk campaigns, T-018) wakes the dispatcher for
   free, rather than needing to remember to notify — `ingest::repo::insert_transactional` is not
   touched by this ticket. One Postgres NOTIFY channel per outbox `channel` value
   (`outbox_sms`, `outbox_email`, `outbox_whatsapp`) so each per-channel claim loop only ever
   wakes for its own channel, no payload filtering needed.
6. **`comms_event.provider_payload_ciphertext` stays `NULL` from this ticket.** `SendOutcome`
   (T-012) carries only `provider_ref`/`provider_status` strings, never the raw provider JSON —
   `HttpSender` already discards the raw response body once parsed. The column exists for the
   webhook receiver (build order step 12), which is the first thing that will ever have a raw
   payload to encrypt.
7. **The dispatcher connects to exactly one tenant, chosen by a required env var
   (`DISPATCHER_TENANT_SLUG`), using per-tenant AppRole Vault login
   (`VaultKeyStore::connect_as_tenant`).** This is the "single-tenant-per-process" case that
   method's own doc comment (`src/keystore.rs`) already names as its intended caller — unlike
   `messgr-ingest`'s shared-admin-token connection (T-011 decision 5), which is multi-tenant per
   process. Requires `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID` in the environment (printed by
   `just provision` on a tenant's first provisioning — see README's "Provisioning" section).
8. **`outbox.expires_at`/`cancelled_at` are not checked.** No writer sets either column yet
   (`ingest::repo::insert_transactional` always inserts both `NULL`; no scheduling or
   cancellation ticket exists) — checking them now would be dead code with no way to exercise it.
   A later ticket (T-027 scheduling / cancellation) adds the check when there is something to
   check against.
9. **Claim loops are generic over `channel`, but only `"sms"` is started by default.** The
   binary reads `DISPATCHER_CHANNELS` (comma-separated, default `sms`) and spawns one claim-loop
   task per entry — matching build order step 2's "one channel (SMS)"; step 11 adds the rest
   behind the same `Sender` trait with no code change here, only new env vars.
10. **`outbox.attempts` increments by 1 as part of the claim `UPDATE`,** even though nothing
    reads it yet — cheap bookkeeping on a column that already exists and already means "how many
    times has this been attempted."

### Tasks

#### Task 1 — Cargo.toml

Add the new binary (no new dependencies — `sqlx`'s `postgres` feature already provides
`PgListener`, and `HttpSender`/`aes-gcm`/etc. all already exist from T-011/T-012):

```toml
[[bin]]
name = "messgr-dispatcher"
path = "src/bin/dispatcher.rs"
```

#### Task 2 — Outbox notify trigger migration

Create `migrations/tenant/0007_outbox_notify.sql`:

```sql
-- Wakes an idle dispatcher's per-channel claim loop (DESIGN.md §4.2:
-- "LISTEN/NOTIFY on insert wakes an idle dispatcher immediately; a
-- 1-second poll is the fallback"). A trigger, not an app-level pg_notify
-- call in ingest::repo::insert_transactional, so every future outbox
-- writer wakes the dispatcher for free (T-013 decision 5).
CREATE OR REPLACE FUNCTION outbox_notify() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('outbox_' || NEW.channel, NEW.comms_request_id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER outbox_notify_trigger
    AFTER INSERT ON outbox
    FOR EACH ROW
    EXECUTE FUNCTION outbox_notify();
```

No new migration runner needed — `provision_tenant` already runs
`sqlx::migrate!("./migrations/tenant")`; an existing dev tenant picks this up on its next
(idempotent) re-provision.

#### Task 3 — Dispatcher domain module: model

Add `src/dispatcher/model.rs`:

- `ClaimedOutbox` (`#[derive(Debug, Clone, sqlx::FromRow)]`): mirrors the `outbox` row exactly —
  `comms_request_id: Uuid`, `created_at: DateTime<Utc>`, `channel: String`, `class: String`,
  `priority: i16`, `customer_id: Uuid`, `address_id: Uuid`, `producer_id: Uuid`,
  `campaign_id: Option<String>`, `next_attempt_at: DateTime<Utc>`,
  `expires_at: Option<DateTime<Utc>>`, `cancelled_at: Option<DateTime<Utc>>`, `attempts: i16`,
  `leased_until: Option<DateTime<Utc>>`.
- `RequestCiphertexts` (`#[derive(Debug, Clone, sqlx::FromRow)]`): `destination_ciphertext:
  Vec<u8>`, `payload_ciphertext: Option<Vec<u8>>`.

#### Task 4 — Dispatcher domain module: repo

Add `src/dispatcher/repo.rs`:

- `pub async fn claim(pool: &PgPool, channel: &str, limit: i64, leased_until: DateTime<Utc>) -> Result<Vec<ClaimedOutbox>, sqlx::Error>` —
  the design's own claim query (§4.2), plus `attempts = attempts + 1` (decision 10):
  ```sql
  UPDATE outbox SET leased_until = $3, attempts = attempts + 1
  WHERE comms_request_id IN (
      SELECT comms_request_id FROM outbox
      WHERE channel = $1 AND next_attempt_at <= now() AND leased_until IS NULL
      ORDER BY priority, next_attempt_at
      LIMIT $2
      FOR UPDATE SKIP LOCKED
  )
  RETURNING comms_request_id, created_at, channel, class, priority, customer_id, address_id,
            producer_id, campaign_id, next_attempt_at, expires_at, cancelled_at, attempts,
            leased_until
  ```
  Caller computes `leased_until = Utc::now() + Duration::minutes(2)` (the design's own lease
  length) and passes it in, rather than binding a Postgres `interval`.
- `pub async fn load_ciphertexts(pool: &PgPool, created_at: DateTime<Utc>, comms_request_id: Uuid) -> Result<Option<RequestCiphertexts>, sqlx::Error>` —
  `SELECT destination_ciphertext, payload_ciphertext FROM comms_request WHERE created_at = $1 AND id = $2`.
  Both key columns are needed — `comms_request`'s primary key is `(created_at, id)`, and
  `outbox.created_at` is already carried for exactly this join (its own column comment: "FK
  component into ledger partition").
- `pub async fn write_terminal(pool: &PgPool, created_at: DateTime<Utc>, comms_request_id: Uuid, customer_id: Uuid, event_type: &str, provider_ref: Option<&str>, provider_status: Option<&str>, final_status: &str) -> Result<(), sqlx::Error>` —
  one transaction: `INSERT INTO comms_event (comms_request_id, customer_id, occurred_at,
  event_type, provider_ref, provider_status, provider_payload_ciphertext) VALUES ($1, $2, $3, $4,
  $5, $6, NULL) ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO
  NOTHING`, then `UPDATE comms_request SET final_status = $1, finalized_at = $2 WHERE created_at
  = $3 AND id = $4`, then `DELETE FROM outbox WHERE comms_request_id = $1`, `COMMIT`. Matches the
  Outcome's "one `comms_event` row and one `final_status` update" shape and the ledger's own
  "exactly one `UPDATE` per row" invariant (§4.1).

#### Task 5 — Dispatcher domain module: worker

Add `src/dispatcher/worker.rs`:

- `pub struct DispatcherContext { pub pool: PgPool, pub keystore: Arc<dyn KeyStore>, pub cache: Arc<KeyCache>, pub mount: String, pub sender: Arc<dyn Sender> }`.
- `const CLAIM_BATCH_SIZE: i64 = 20;`, `const LEASE_DURATION: chrono::Duration = chrono::Duration::minutes(2);`,
  `const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);` (§4.2's own
  fallback interval).
- A private `DispatchError` enum (`Database(sqlx::Error)`, `Dek(CustomerDekError)`,
  `Encryption(EncryptionError)`, `InvalidUtf8(std::string::FromUtf8Error)`,
  `MissingRequest(Uuid)`, `MissingPayload(Uuid)`) with `Display`/`Error`/`From` impls, following
  `RegistryError`'s shape (`src/tenant/registry.rs`).
- `async fn try_process(ctx: &DispatcherContext, row: &ClaimedOutbox) -> Result<(), DispatchError>` —
  loads ciphertexts (`repo::load_ciphertexts`; a `None` is `DispatchError::MissingRequest`, which
  should be unreachable since the outbox row's own insert is in the same transaction as its
  ledger row, T-011), fetches the DEK (`customer_dek::lifecycle::get_or_create_dek` with
  `ctx.mount`/`ctx.cache`), decrypts destination and payload with `encryption::decrypt` (`aad =
  row.comms_request_id.as_bytes()`, matching T-011 decision 6) and `String::from_utf8`s both,
  calls `ctx.sender.send(&destination, &body)`, then `repo::write_terminal` with:
  - `Ok(outcome)` → `event_type = "sent"`, `provider_ref = Some(&outcome.provider_ref)`,
    `provider_status = Some(&outcome.provider_status)`, `final_status = "sent"`.
  - `Err(SenderError::Provider { status, .. })` → `event_type = "failed"`, `provider_ref =
    None`, `provider_status = Some(&status.to_string())`, `final_status = "failed"` (decision 2).
  - `Err(SenderError::Http(_))` → `event_type = "failed"`, `provider_ref = None`,
    `provider_status = None`, `final_status = "failed"` (decision 2).
- `async fn process_one(ctx: &DispatcherContext, row: ClaimedOutbox)` — calls `try_process`,
  logging (`tracing::error!`) rather than propagating on `Err` — one bad row must not stop the
  loop from claiming the rest of its batch.
- `pub async fn run_channel_loop(ctx: Arc<DispatcherContext>, channel: String)` — opens a
  dedicated LISTEN connection via `sqlx::postgres::PgListener::connect_with(&ctx.pool)` (verify
  the exact method name against the installed `sqlx` version's own docs when implementing) and
  `listener.listen(&format!("outbox_{channel}"))`, then loops: `repo::claim(&ctx.pool, &channel,
  CLAIM_BATCH_SIZE, Utc::now() + LEASE_DURATION)`; if the claimed batch is empty,
  `tokio::select! { _ = listener.recv() => {}, _ = tokio::time::sleep(POLL_INTERVAL) => {} }`
  before looping again; otherwise `process_one` every claimed row (sequentially — no
  in-loop concurrency yet, matching "minimal") and loop immediately without waiting (drain the
  ready set before going idle).

#### Task 6 — Wiring

Add `src/dispatcher/mod.rs` (`pub mod model; pub mod repo; pub mod worker;`) and register `pub
mod dispatcher;` in `src/lib.rs`.

#### Task 7 — `messgr-dispatcher` binary

Add `src/bin/dispatcher.rs`:

- Reads `Config::from_env()` plus `DISPATCHER_TENANT_SLUG` (required), `DISPATCHER_CHANNELS`
  (optional, default `"sms"`, comma-separated).
- Connects the control pool (`db::connect`), resolves the tenant
  (`tenant::repo::find_by_slug`, panic with a clear message if not found).
- Connects the tenant pool (`tenant::pool::connect_tenant_pool`).
- Connects Vault via `VaultKeyStore::connect_as_tenant(config.profile)` (decision 7) — requires
  `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID` in the environment.
- Builds one `Arc<KeyCache>` (`NonZeroUsize::new(100_000)`, `Duration::from_secs(3600)` — the
  sizing `key_cache.rs`'s own doc comment already recommends for "T-013's dispatcher").
- For each channel in `DISPATCHER_CHANNELS`: reads `DISPATCHER_<CHANNEL>_BASE_URL` /
  `DISPATCHER_<CHANNEL>_API_KEY` (uppercased, decision 1; panic with a clear message if either is
  unset), builds an `HttpSender`, builds a `DispatcherContext`, and `tokio::spawn`s
  `worker::run_channel_loop`.
- Awaits every spawned task (the process runs forever; there is no shutdown path in this
  ticket, matching every other long-running binary this codebase has today — none of them have
  one either).

#### Task 8 — Integration test

Add `tests/dispatcher.rs`, following `tests/ledger_outbox_schema.rs`/`tests/provider_config.rs`'s
conventions (`unique_name`, real `provision_tenant`, `drop_test_tenant`-style cleanup). Write a
real outbox row the same way `messgr-ingest` would — call `ingest::repo::insert_transactional`
directly with hand-encrypted ciphertexts (`encryption::encrypt` under a DEK fetched via
`customer_dek::lifecycle::get_or_create_dek`, `aad = comms_request_id.as_bytes()`) — rather than
standing up mTLS/axum, since this test is about the dispatcher's read/decrypt/send/write path,
not ingest's. Cover, driving `dispatcher::worker::try_process`/`repo::claim` directly (not the
binary):

1. A claimed row whose `Sender` call succeeds (a `wiremock` server returning 200) ends with:
   `comms_request.final_status = 'sent'`, one `comms_event` row (`event_type = 'sent'`,
   `provider_ref`/`provider_status` matching the mock's response), and the `outbox` row gone.
2. A claimed row whose `Sender` call fails (`wiremock` returning 500) ends with:
   `final_status = 'failed'`, one `comms_event` row (`event_type = 'failed'`, `provider_status =
   Some("500")`), and the `outbox` row gone — no requeue (decision 2).
3. Two concurrent `repo::claim` calls for the same channel against two ready rows each claim a
   disjoint row — `SKIP LOCKED` proves no double-claim (mirrors T-011/F2's concurrency-test
   style for its own "no double-send" claim).
4. `repo::claim` ignores a row already leased (`leased_until` in the future) and a row not yet
   due (`next_attempt_at` in the future).
5. Inserting an outbox row fires the notify trigger: a `sqlx::postgres::PgListener` subscribed
   to `outbox_sms` receives a notification carrying the inserted row's `comms_request_id`,
   without needing to wait out `POLL_INTERVAL`.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/dispatcher.rs
```

Manual end-to-end walkthrough (extends T-011's own — a message actually gets sent this time):

```
just provision acme eu tenant_acme operator@example.com   # prints vault_role_id / vault_wrapped_secret_id — copy both
just tenant-config-set acme 7 UTC en-US UTC 300 operator@example.com
just dev-pki-issue-cert fraud-alerts.internal /tmp/producer-cert
just dev-pki-issue-cert messgr-ingest.internal /tmp/ingest-cert
just producer-register acme fraud-alerts "$(openssl x509 -noout -subject -nameopt RFC2253 -in /tmp/producer-cert/cert.pem | sed 's/subject=//')" fraud-team oncall@example.com operator@example.com
just template-approve acme balance-alert 1 sms en-US /tmp/body.txt operator@example.com

# a local stand-in "provider": a trivial HTTP echo server on :9000 answering
# POST /messages with {"message_id": "...", "status": "queued"}

INGEST_TLS_CERT_FILE=/tmp/ingest-cert/cert.pem INGEST_TLS_KEY_FILE=/tmp/ingest-cert/key.pem \
INGEST_TLS_CLIENT_CA_FILE=/tmp/ingest-cert/ca.pem cargo run --bin messgr-ingest &

curl -sk --cert /tmp/producer-cert/cert.pem --key /tmp/producer-cert/key.pem \
  --cacert /tmp/ingest-cert/ca.pem -H "Idempotency-Key: demo-1" -H "Content-Type: application/json" \
  -d '{"customer_id":"<uuid>","destination":"+15550100","channel":"sms","class":"transactional","template_id":"balance-alert","template_version":1,"variables":{"name":"Jordan","balance":"100.00"}}' \
  https://localhost:8443/comms

DISPATCHER_TENANT_SLUG=acme DISPATCHER_CHANNELS=sms \
DISPATCHER_SMS_BASE_URL=http://localhost:9000 DISPATCHER_SMS_API_KEY=dev-key \
VAULT_ROLE_ID=<from provision> VAULT_WRAPPED_SECRET_ID=<from provision> \
cargo run --bin messgr-dispatcher &

psql postgres://messgr:messgr@localhost:5432/tenant_acme -c \
  "select final_status from comms_request" \
  -c "select event_type, provider_status from comms_event" \
  -c "select count(*) from outbox"
```

Expected: within ~1s (the poll fallback; the NOTIFY should make it near-instant),
`final_status = 'sent'`, one `comms_event` row with `event_type = 'sent'`, and `outbox` empty.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-dispatcher` binary and its env vars.

- `README.md` — add a "### messgr-dispatcher" section (after "### messgr-ingest: `POST
  /comms`") documenting `DISPATCHER_TENANT_SLUG`, `DISPATCHER_CHANNELS`,
  `DISPATCHER_<CHANNEL>_BASE_URL`/`_API_KEY`, that it is single-tenant-per-process with no HA/
  retry yet (decisions 2–3), and a runnable walkthrough (the one above, trimmed). Update the
  "## Status" paragraph: three binaries exist now.
- `.env.example` — add a commented block for `DISPATCHER_TENANT_SLUG`, `DISPATCHER_CHANNELS`,
  `DISPATCHER_SMS_BASE_URL`, `DISPATCHER_SMS_API_KEY` (no defaults, matching the `INGEST_*`
  block's style).
- `justfile` — add a `dispatcher-run` recipe (`cargo run --bin messgr-dispatcher`) in the
  `control-plane` group, matching `ingest-run`'s actual placement (T-011/F4 noted the plan's own
  wording said "build/test group" but the shipped file used `control-plane` — follow the shipped
  precedent, not the stale wording).
- No `DESIGN.md` change expected — this implements §4.2/§4.1/§9 as written; if implementation
  forces a deviation, stop and raise it rather than editing the design to match the code (matches
  T-011's own docs step).

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. Docs updated per the docs step above.
3. Write a summary: files touched, decisions honoured, anything deferred (leader election,
   retry/backoff, rate-limit enforcement, real `provider_config`-driven credential resolution —
   all explicitly out of scope per decisions 1–4).
4. Suggested Conventional Commit message:

   ```
   feat(dispatcher): add minimal per-channel claim loop and comms_event write (T-013)

   Adds messgr-dispatcher: a per-tenant, per-channel claim loop (SKIP LOCKED
   leases, LISTEN/NOTIFY wakeup via an outbox trigger with a 1s poll
   fallback) that calls the Sender trait built in T-012 and writes exactly
   one comms_event + comms_request.final_status update per outbox row,
   deleting it on completion. Single attempt, single process, no rate
   limiting -- leader election, retry/backoff, and provider_config-driven
   credential resolution are later tickets (DESIGN.md build order step 7+).
   Sender credentials come from env vars for now, since provider_config has
   no base_url column and its credential_path has no Vault KV reader yet
   (T-012 decisions 3-4).
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration+model+repo / worker / binary+wiring / tests / docs is a natural
   split) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-09-01 — TO DO → READY: plan complete
