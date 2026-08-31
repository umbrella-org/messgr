---
id: T-011
title: messgr-ingest: POST /comms, idempotency replay, ledger + outbox write, encryption
project: messgr
depends-on: [T-006, T-008, T-009, T-010]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-011 — messgr-ingest: POST /comms, idempotency replay, ledger + outbox write, encryption

## Outcome

After this ships, a registered producer can `POST /comms` and get back a ledger row: the
request is recorded and the outbox row written in the same transaction, payload and destination
are encrypted under a per-customer DEK, and a repeated idempotency key replays instead of
double-sending.

## Description

Build `messgr-ingest`: `POST /comms`, idempotency replay, a single-transaction ledger + outbox
write, and payload/destination encryption + HMAC (design §4.1–§4.3, §7, §11). Producer identity
comes from the mTLS layer built in T-006, never from the request body (§11, §4.9) — that rule is
enforced here and every later ingest ticket inherits it. Part of the step-2 ticket family
(`family: T-007`; see T-007). Depends on T-006 (identity resolution, merged), T-008 (DEK
lifecycle), T-009 (ledger/outbox schema), and T-010 (template store).

This is the **first ingest binary** — `resolve_producer`'s own doc comment (`src/producer/resolve.rs`)
names "extracting [the cert subject] from an actual TLS handshake" as this ticket's job — and the
first thing in the codebase that terminates TLS, does AEAD encryption, or serves HTTP at all. Three
gaps surfaced while refining scope, none of them optional to resolve before writing code:

- **No `customer_address` table exists yet** (§4.6, table lands in `T-015`/step 3). `outbox.address_id`
  is `NOT NULL` with no FK (T-009 decision 1) but has nothing real to reference. Confirmed with the
  user: mint a fresh `Uuid::new_v4()` per request, stored only in `outbox`, never persisted anywhere
  else — safe because outbox rows are `DELETE`d on reaching a terminal state (§4.2) by the dispatcher
  (`T-013`) long before `T-016`'s resolution path would ever need to reconcile one.
- **Resolution at ingest (§4.7 — alias expansion, external-id/HMAC lookup, provisional-shell minting)
  is `T-016`, not built yet.** So this ticket cannot resolve a caller-supplied external id or bare
  destination into a `customer_id` the way the design's full table eventually allows. Confirmed with
  the user: the request body requires an explicit `customer_id` (uuid) and an explicit `destination`
  (address string) — the same shape §4.8 already mandates for OTP, just extended to every class
  because no resolution path exists to fall back to.
- **`class = auth` must never reach the outbox** (AGENTS.md hard invariant 1; OTP's real path is
  `sms-sender`/`otp-api`, `T-047`, not filed yet). Confirmed with the user: `POST /comms` rejects
  `class = "auth"` with `422` — there is no legitimate caller of that class through this endpoint
  until `T-047` exists, and accepting it here would need the outbox/encryption write it just skips.

Also underspecified by the design itself: `comms_request.dek_id` is declared (`uuid`, nullable) but
never defined — `customer_dek`'s primary key is `customer_id`, not a synthetic id, so there is no
value this column could hold that means anything yet. Leaving it `NULL` (no consumer reads it before
some future key-rotation ticket defines what it should be) is the only defensible choice; flagged
here rather than guessed at.

Vault access for this service does **not** use the per-tenant AppRole login path
(`VaultKeyStore::connect_as_tenant`) — that mechanism's own doc comment scopes it to "a tenant's
dispatcher deployment" (one tenant per process, §2.3's dispatcher sharding). `messgr-ingest` is
multi-tenant per process (§2.3's connection-budget discussion), so confirmed with the user: it
connects once with the shared admin `VAULT_TOKEN` (`VaultKeyStore::connect`, the same client the CLI
and `customer_dek::pre_provision_for_tenant` already use) and reads/writes every tenant's Transit
mount through it. Per-tenant AppRole login for ingest is deferred to a later hardening ticket.

## Implementation Plan

### Prerequisite gate (hard)

- `T-006`, `T-008`, `T-009`, `T-010` all in `6-done/` and merged to `main` — confirmed on the board
  (all four merged as of this refinement).
- Clean working tree before branching.
- Local stack up: `just db-up`, `just control-migrate`, `just vault-dev-init`, `just dev-pki-bootstrap`.

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-011-messgr-ingest-post-comms
```

Root-path child (`path = "."`): WIP commits encouraged, then interactive-rebased into atomic
commits before the summary is presented (rules §0). Do not push and do not open a merge request
without explicit user approval. Ticket/board bookkeeping is committed on `main`, never on this
branch.

### Confirmed design decisions (do not deviate without asking)

1. **Request contract, given no resolution path exists yet (§4.7 is `T-016`).** `POST /comms` JSON
   body: `customer_id` (uuid, required), `destination` (string, required — the raw address as the
   producer knows it), `channel` (`sms|email|whatsapp`, required), `class` (`transactional|marketing`,
   required — `auth` is a hard `422`, see decision 3), `template_id` (string, required),
   `template_version` (i32, required — no "latest" convenience; pinning is the point, §4.4),
   `locale` (string, optional — defaults to the tenant's `tenant_config.default_locale`),
   `campaign_id` (string, optional — must be absent/null when `class = "marketing"` is not required,
   but **must be null when `class = "transactional"`**, `422` otherwise, per §4.1's own comment on the
   column), `variables` (`map<string,string>`, optional, default empty — passed straight to
   `template::render::render`). No `scheduled_for`/`expires_at` in this ticket's request body —
   scheduling is `T-027` (step 9); `outbox.next_attempt_at` is always `now()` here.
2. **Idempotency key transport: `Idempotency-Key` request header, required.** Missing header is a
   `400`. Matches the header-based convention `idempotency` (§4.3) implies without naming a transport.
3. **`class = "auth"` is rejected with `422` before anything else runs.** No outbox row, no
   encryption, no ledger write — see Description. `channel`/`class` values outside their closed sets
   are also `422`.
4. **`outbox.address_id` is a fresh `Uuid::new_v4()` per request, `comms_request.dek_id` is always
   `NULL`.** Both per Description; both are deliberate, not a placeholder to "fix later" inside this
   ticket.
5. **Vault: shared admin-token `VaultKeyStore::connect(profile)` at startup, one client for every
   tenant.** No per-tenant AppRole login here — see Description. `mount` for Transit calls is
   `tenant.vault_mount`, read once per tenant and cached (decision 8).
6. **Encryption: AES-256-GCM via the `aes-gcm` crate (RustCrypto), matching §7.1's own naming.**
   New module `src/encryption.rs`: `encrypt(dek: &[u8], aad: &[u8], plaintext: &[u8]) ->
   Result<Vec<u8>, EncryptionError>` / `decrypt(dek: &[u8], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>,
   EncryptionError>`. Wire format: a fresh random 12-byte nonce (`Aes256Gcm::generate_nonce`, one per
   call — GCM nonce reuse under the same key breaks confidentiality) prepended to the ciphertext+tag,
   i.e. `blob = nonce(12) || ciphertext_with_tag`. `aad` is `comms_request_id`'s 16 raw bytes for both
   `destination_ciphertext` and `payload_ciphertext` on a given row — binds each ciphertext to its own
   row so one row's blob cannot be silently swapped onto another's during a future bulk operation.
   `decrypt` is unused by this ticket (nothing reads these columns back yet) but ships alongside
   `encrypt` because an encryption module that can only write and never verifies its own round-trip is
   how §7's "very nearly missed" mistake (comms_event provider payloads, §4.4) happened the first time
   — the acceptance test exercises both directions.
7. **HTTP/TLS stack: `axum` + `axum-server` (`tls-rustls` feature) + `rustls` 0.23, terminating mTLS
   in-process — no reverse proxy.** Matches `resolve_producer`'s own doc comment (Description). New
   crates: `axum`, `axum-server`, `rustls`, `rustls-pemfile`, `x509-parser` (leaf-certificate subject
   extraction — CN/SAN — from the verified peer certificate), all runtime dependencies; `reqwest`
   (`rustls-tls` feature, client identity support) as a dev-dependency only, for `tests/ingest.rs`.
8. **Multi-tenant pool + config cache: `src/tenant/registry.rs`, keyed by `tenant_id`, populated
   lazily on first request (§2.3: "pools are created lazily").** `TenantContext { pool: PgPool,
   vault_mount: String, config: TenantConfig }`, built once per tenant behind a lock (`tokio::sync::RwLock<HashMap<Uuid, Arc<TenantContext>>>`)
   via `tenant::repo::find_by_id` (new — `find_by_slug` exists, `find_by_id` doesn't yet) +
   `tenant::pool::connect_tenant_pool` (using `config.control_database_url` as the base URL — the
   same reuse `pre_provision_for_tenant`/`register_producer` already established, not a second env
   var) + `tenant_config::repo::find`. A tenant with no `tenant_config` row yet is a `424` (not fully
   onboarded) — `tenant-config set` is expected to have run before a tenant's producers go live.
9. **Server/client TLS material comes from PEM files named by env vars, identically in dev and
   production** — `INGEST_TLS_CERT_FILE`, `INGEST_TLS_KEY_FILE` (the ingest service's own leaf
   cert+key) and `INGEST_TLS_CLIENT_CA_FILE` (the CA every producer client certificate must chain to,
   for `rustls`'s client-cert verifier). No dev-only branch inside `messgr-ingest` itself — the
   *existing* `dev-pki` guard (`dev_pki::assert_dev_profile`) is still the only gate on minting
   certificates; this ticket only ever reads PEM files a human already generated. Local dev workflow
   (documented, not automated): `just dev-pki-issue-cert messgr-ingest.internal /tmp/ingest-cert`
   writes `cert.pem`/`key.pem`/`ca.pem`; point `INGEST_TLS_CERT_FILE`/`INGEST_TLS_KEY_FILE` at the
   first two and `INGEST_TLS_CLIENT_CA_FILE` at `ca.pem` (the same root CA every `dev-pki issue-cert`
   producer certificate already chains to).
10. **Chain-of-trust validation happens entirely at the TLS layer, not the database.** `producer_cert`
    (control DB) has never stored a fingerprint or public key (`cert_repo.rs`) — only a subject
    string. That's only safe because `rustls`'s client-cert verifier already rejects, at the handshake,
    any certificate that doesn't chain to `INGEST_TLS_CLIENT_CA_FILE`; the DB lookup afterward maps an
    *already-authenticated* subject to `(tenant_id, producer_id)` and checks `enabled`. Recorded here
    because nothing in DESIGN.md §4.9/§11.1 states this explicitly and it is load-bearing for the
    identity model's actual security, not just its bookkeeping.

### Tasks

#### Task 1 — Dependencies

`Cargo.toml`: add `axum = "0.8"`, `axum-server = { version = "0.7", features = ["tls-rustls"] }`,
`rustls = "0.23"`, `rustls-pemfile = "2"`, `x509-parser = "0.16"`, `aes-gcm = "0.10"`. Add
`[dev-dependencies]` `reqwest = { version = "0.12", default-features = false, features =
["rustls-tls", "json"] }`. Add the new binary:
```toml
[[bin]]
name = "messgr-ingest"
path = "src/bin/ingest.rs"
```

#### Task 2 — Encryption module

`src/encryption.rs` — `encrypt`/`decrypt` per decision 6, plus `EncryptionError`. Unit tests:
round-trip succeeds; decrypting with the wrong DEK fails; decrypting with mismatched `aad` fails;
a truncated blob (shorter than the nonce) is a clean error, not a panic. Register `pub mod
encryption;` in `src/lib.rs`.

#### Task 3 — `tenant::repo::find_by_id`

`src/tenant/repo.rs` — add `find_by_id(pool: &PgPool, tenant_id: Uuid) -> Result<Option<Tenant>,
sqlx::Error>`, same column list as `find_by_slug`, `WHERE id = $1`.

#### Task 4 — Tenant registry

`src/tenant/registry.rs` — `TenantContext` and `TenantRegistry` per decision 8: `get_or_open(&self,
control_pool: &PgPool, tenant_id: Uuid, profile: Profile) -> Result<Arc<TenantContext>,
RegistryError>`, `RegistryError` covering "unknown tenant" (should be unreachable — `resolve_producer`
already validated the cert — but handled, not `unwrap`ped), "tenant not configured" (no
`tenant_config` row), and the underlying `sqlx::Error`. Register `pub mod registry;` under
`src/tenant/mod.rs`.

#### Task 5 — mTLS termination

`src/mtls.rs` — `PeerCertSubject(pub String)` (an axum request-extension type); `load_server_config
(cert_pem_path, key_pem_path, client_ca_pem_path) -> Result<rustls::ServerConfig, MtlsError>` (builds
a `rustls::RootCertStore` from the client-CA file, a `WebPkiClientVerifier` requiring a valid client
cert, and the server's own cert chain + key via `rustls-pemfile`); `subject_from_der(der: &[u8]) ->
Result<String, MtlsError>` using `x509_parser::parse_x509_certificate` to render the leaf's `Subject`
as an RFC-4514-ish string (CN if present, else the full DN — must match whatever
`producer::register::register_producer`'s `cert_subject` argument is given at registration time, so
document the exact rendering in this module's doc comment). A small `axum-server`
`Accept` wrapper around `axum_server::tls_rustls::RustlsAcceptor` that, after the handshake, reads
the peer's leaf certificate DER off the `rustls::ServerConnection`, calls `subject_from_der`, and
inserts a `PeerCertSubject` extension into the request via `axum-server`'s `AddExtension` service
wrapper (its documented mechanism for attaching per-connection, post-handshake data — verify the
exact type names against the installed `axum-server` version's own docs/examples when implementing;
this is that crate's standard client-cert-extraction pattern, not a new one). Register `pub mod
mtls;` in `src/lib.rs`.

#### Task 6 — Ingest domain module

`src/ingest/mod.rs` (`pub mod model; pub mod identity; pub mod repo; pub mod handler;`), registered
as `pub mod ingest;` in `src/lib.rs`.

- `src/ingest/model.rs` — `CreateCommsRequest` (`serde::Deserialize`, fields per decision 1),
  `CreateCommsResponse { comms_request_id: Uuid }`, `IngestError` (`UnknownProducer`,
  `ProducerDisabled`, `MissingIdempotencyKey`, `InvalidClass`, `InvalidChannel`,
  `CampaignIdOnTransactional`, `TemplateNotFound`, `RenderError(template::render::RenderError)`,
  `TenantNotConfigured`, `Database(sqlx::Error)`, `Encryption(EncryptionError)`,
  `Vault(KeyStoreError)`) with an `axum::response::IntoResponse` impl mapping each to its status code
  (`403` for the two producer variants, `400` for the header/validation ones, `422` for the class/
  campaign ones, `404` for the template, `424` for tenant-not-configured, `500` for the rest) and a
  JSON `{ "error": "<message>" }` body.
- `src/ingest/identity.rs` — `ProducerContext { tenant_id: Uuid, producer_id: Uuid, tenant: Arc<TenantContext> }`
  implementing `axum::extract::FromRequestParts<AppState>`: reads the `PeerCertSubject` extension
  (absent means the mTLS layer is mis-wired — a `500`, not a `403`, since it should never happen post-
  handshake), calls `producer::resolve::resolve_producer`, maps `ResolutionError` to
  `IngestError::UnknownProducer`/`ProducerDisabled`, then `TenantRegistry::get_or_open`.
- `src/ingest/repo.rs` — `find_idempotent_reply(pool: &PgPool, key: &str) -> Result<Option<Uuid>,
  sqlx::Error>` (plain `SELECT`, the fast pre-check) and `insert_transactional(pool: &PgPool, ...) ->
  Result<InsertOutcome, sqlx::Error>` implementing the claim-then-insert transaction from Description:
  `INSERT INTO idempotency ... ON CONFLICT (key) DO NOTHING`, check rows affected; `0` → roll back,
  return `InsertOutcome::Replayed(existing_id)` (caller re-`SELECT`s it); `1` → insert `comms_request`
  then `outbox` in the same transaction, `COMMIT`, return `InsertOutcome::Created`. `priority` is `1`
  for `transactional`, `2` for `marketing` (§4.2's own comment). `next_attempt_at = now()`.
  `idempotency.expires_at = now() + 30 days` (§4.3; the nightly sweep job itself is out of scope here,
  same as `T-009` deferred it — no ticket currently owns it, worth a follow-up note at review time).
- `src/ingest/handler.rs` — `async fn create_comms(State(app): State<AppState>, producer:
  ProducerContext, headers: HeaderMap, Json(body): Json<CreateCommsRequest>) -> Result<(StatusCode,
  Json<CreateCommsResponse>), IngestError>`: validate `Idempotency-Key` present, validate
  channel/class/campaign_id (decisions 1/3), fast idempotency pre-check, look up the template
  (`template::repo::find`, using `body.locale` or `producer.tenant.config.default_locale`), render it
  (`template::render::render`), `customer_dek::lifecycle::get_or_create_dek` for `body.customer_id`,
  compute `destination_hmac` (`destination_hmac::compute`, using the tenant's pepper —
  `tenant_pepper::ensure_tenant_pepper`, cached per tenant in `TenantContext`), encrypt destination and
  rendered body (`encryption::encrypt`, decision 6), then `repo::insert_transactional`. Mint
  `comms_request_id = Uuid::new_v4()` and `address_id = Uuid::new_v4()` (decision 4) before encrypting,
  since `comms_request_id` is the AAD.

#### Task 7 — `AppState` and the `messgr-ingest` binary

`src/bin/ingest.rs`: loads `Config::from_env()` plus `INGEST_LISTEN_ADDR` (default
`0.0.0.0:8443`), `INGEST_TLS_CERT_FILE`, `INGEST_TLS_KEY_FILE`, `INGEST_TLS_CLIENT_CA_FILE`
(all required, no default — refuse to start with a clear message if unset); connects the control
pool (`db::connect`); connects `VaultKeyStore::connect(profile)` (decision 5); builds an `AppState {
control_pool, keystore: Arc<dyn KeyStore>, registry: TenantRegistry, profile }`; builds the `axum::Router`
with `POST /comms` → `ingest::handler::create_comms`; loads the TLS config (`mtls::load_server_config`)
and serves via `axum-server`'s rustls acceptor wrapped per Task 5.

#### Task 8 — Integration test

`tests/ingest.rs`, following `tests/producer.rs`'s conventions (`unique_name`,
`control_database_url`, `vault_keystore`, real `provision_tenant`, `drop_test_tenant`-style
cleanup). Bootstrap dev PKI once (`dev_pki::bootstrap`), issue one client cert per producer under
test and one server cert for the harness itself (`dev_pki::issue_cert`), write them to a temp dir,
start `messgr-ingest`'s router in-process on an ephemeral port (same `axum-server` wiring as Task 7,
not a spawned subprocess — keeps the test fast and lets it assert directly against the tenant pool
afterward), and drive it with `reqwest::Client` built with `Identity::from_pem` (client cert+key) and
`add_root_certificate` (the CA). Cover:

1. A registered, enabled producer's `POST /comms` (`class = "transactional"`) returns `201`, and the
   `comms_request`/`outbox` rows exist with the expected `channel`/`class`/`priority`/`producer_id`;
   `destination_ciphertext`/`payload_ciphertext` decrypt (via `encryption::decrypt` using the DEK
   fetched through `customer_dek::lifecycle::get_or_create_dek`) back to the original destination and
   rendered body.
2. Repeating the exact same request with the same `Idempotency-Key` returns `200` with the identical
   `comms_request_id`, and no second `outbox`/`comms_request` row exists.
3. A client certificate that chains to the trusted CA but was never registered (no `producer_cert`
   row) gets `403` — proves the TLS handshake alone is not authorization.
4. A disabled producer's cert gets `403`, distinguishable in the response body from case 3 (mirrors
   `ResolutionError`'s own `UnknownCert`/`Disabled` split).
5. `class = "auth"` is `422` and writes nothing to `comms_request`/`outbox`/`idempotency`.
6. `class = "transactional"` with a non-null `campaign_id` is `422`.
7. An unknown `channel` value is `422`.
8. Missing `Idempotency-Key` header is `400`.
9. An unapproved/nonexistent `(template_id, template_version, locale)` is `404`.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just dev-pki-bootstrap
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/ingest.rs
```

Manual end-to-end walkthrough:

```
just provision acme eu tenant_acme operator@example.com
just tenant-config-set acme 7 UTC en-US UTC 300 operator@example.com
just dev-pki-issue-cert fraud-alerts.internal /tmp/producer-cert
just dev-pki-issue-cert messgr-ingest.internal /tmp/ingest-cert
just producer-register acme fraud-alerts "$(openssl x509 -noout -subject -nameopt RFC2253 -in /tmp/producer-cert/cert.pem | sed 's/subject=//')" fraud-team oncall@example.com operator@example.com
just template-approve acme balance-alert 1 sms en-US /tmp/body.txt operator@example.com   # body.txt: "Hi {{name}}, your balance is {{balance}}."

INGEST_TLS_CERT_FILE=/tmp/ingest-cert/cert.pem \
INGEST_TLS_KEY_FILE=/tmp/ingest-cert/key.pem \
INGEST_TLS_CLIENT_CA_FILE=/tmp/ingest-cert/ca.pem \
cargo run --bin messgr-ingest &

curl -sk --cert /tmp/producer-cert/cert.pem --key /tmp/producer-cert/key.pem \
  --cacert /tmp/ingest-cert/ca.pem \
  -H "Idempotency-Key: demo-1" -H "Content-Type: application/json" \
  -d '{"customer_id":"<uuid>","destination":"+15550100","channel":"sms","class":"transactional","template_id":"balance-alert","template_version":1,"variables":{"name":"Jordan","balance":"100.00"}}' \
  https://localhost:8443/comms
```

Expected: `201` with a `comms_request_id`; repeating the identical `curl` (same `Idempotency-Key`)
returns `200` with the same id; `psql postgres://messgr:messgr@localhost:5432/tenant_acme -c "select
channel, class, final_status from comms_request"` shows one row, `final_status` still `NULL`
(nothing dispatches it — `T-013` doesn't exist yet, matching §14 step 2's "no gates, no send" scope).

### Docs update (mandatory when user-facing)

`README.md`: the "### mTLS resolution and dev PKI" section's sentence "No TLS-terminating binary
exists yet to call it — that is `T-011` (`messgr-ingest`)" is now stale — replace it with a note that
`messgr-ingest`'s `POST /comms` is that binary. Add a new "### messgr-ingest: `POST /comms`" section
documenting `INGEST_LISTEN_ADDR`/`INGEST_TLS_CERT_FILE`/`INGEST_TLS_KEY_FILE`/
`INGEST_TLS_CLIENT_CA_FILE`, the `dev-pki-issue-cert` workflow for minting a server identity locally,
and a runnable `curl` example (the one above, trimmed). Update the "## Status" paragraph: two
binaries exist now, not one. `.env.example`: add a commented block for the four `INGEST_*` variables
(no default values — Task 7 refuses to start without them). `justfile`: add an `ingest-run` recipe
(`cargo run --bin messgr-ingest`) alongside the existing `build`/`test` group. No `DESIGN.md` change
expected — this implements §4.1–§4.3/§7/§11 as written; if implementation forces a deviation, stop
and raise it rather than editing the design to match the code (matches `T-009`'s own docs step).

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. Docs updated per the docs step above.
3. Write a summary: files touched, decisions honoured, anything deferred (the idempotency sweep job
   and per-tenant AppRole login for ingest, both explicitly out of scope — see Description/decisions).
4. Suggested Conventional Commit message:

   ```
   feat(ingest): add messgr-ingest POST /comms with mTLS, encryption, idempotency (T-011)

   Adds the first ingest binary: axum + rustls mTLS termination resolving a
   client certificate to (tenant_id, producer_id) via T-006's resolve_producer,
   a single-transaction idempotency-claim + comms_request + outbox write, and
   AES-256-GCM encryption of the destination and rendered template body under
   the customer's DEK (T-008). No resolution at ingest (T-016) and no gate
   chain (T-017) yet -- customer_id/destination are caller-supplied and
   class=auth is rejected outright, matching step 2's "prove queue mechanics,
   no gates" scope (PLAN.md build step 2).
   ```

5. Root-path child: interactive-rebase WIP commits into atomic, correctly scoped commits (a natural
   split: dependencies/encryption module, tenant registry, mTLS termination, ingest handler +
   transaction, integration test, docs) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit user
   approval. On approval, keep the tidied history (root-path default), verify `git fetch origin main
   && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints nothing (in-tree layout,
   rules §0), then push and open the merge request. Merging is the human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-08-31 — TO DO → READY: plan complete
- 2026-08-31 — READY → IN DEVELOPMENT: picked up
- 2026-08-31 — IN DEVELOPMENT → IN REVIEW: acceptance green
