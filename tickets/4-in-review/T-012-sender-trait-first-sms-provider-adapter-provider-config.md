---
id: T-012
title: Sender trait + first SMS provider adapter + provider_config
project: messgr
depends-on: [T-007]
spawned-by: []
family: T-007
impact: high
complexity: medium
cost: M
---

# T-012 — Sender trait + first SMS provider adapter + provider_config

## Outcome

After this ships, the send path has a channel-agnostic `Sender` trait, a concrete HTTP-contract
adapter (`HttpSender`) proven against a local mock server, and `provider_config` holding an
ordered provider list from day one — even at length 1 — so later multi-provider failover
(T-046) has a list to extend rather than a single hardcoded provider to refactor away. Which
real vendor sits behind `Sender` stays a later ticket's decision (DESIGN.md Still Open #5); this
one proves the trait boundary and the config shape.

## Description

Build the `Sender` trait, a generic HTTP-contract adapter (`HttpSender`), and `provider_config`
as an ordered list from day one (design §4.10, §12.1). No real vendor (Twilio or otherwise) is
wired up — DESIGN.md leaves provider selection genuinely open (Still Open #5) — and Vault-backed
credential retrieval for `provider_config.credential_path` is deferred to a later ticket, since
no KV-secret-read plumbing exists yet (only Transit, for DEKs, per T-003/T-004). Part of the
step-2 ticket family (`family: T-007`; see T-007). Depends on T-007 for the
tenant-scoped configure/audit pattern (`tenant_config::configure`) this ticket's
`provider_config::configure` mirrors.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-012-sender-trait-provider-config
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented (rules
§0). Do not push and do not open a merge request without explicit user approval. Ticket and board
bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-007` is in `6-done/` and merged to `main` — confirmed at refinement. This ticket's
  `provider_config::configure` mirrors `tenant_config::configure`'s resolve/connect/load/compare/
  upsert/audit shape exactly.
- `T-005` is in `6-done/` and merged — established the "no `tenant_id` column inside a
  tenant-database table" convention (decision 6) this ticket's `provider_config` table follows.
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init` — the
  `provider_config` integration test provisions a real tenant via `provision_tenant`, which
  creates a Transit mount regardless of whether this ticket's own code touches Vault.

### Confirmed design decisions (do not deviate without asking)

1. **Generic HTTP contract, not a named vendor.** Confirmed with the user during refinement:
   DESIGN.md leaves "provider selection" genuinely unresolved (Still Open #5) — `twilio` is only
   ever DESIGN.md's example value for the `provider` column (§4.10, §592), never a committed
   choice. `HttpSender` (Task 3) speaks a small JSON contract of this codebase's own design —
   `POST {base_url}/messages` with `{"to", "body"}`, expecting `{"message_id", "status"}` back —
   proving the trait/adapter boundary without picking a real vendor. A real vendor is a later
   ticket's adapter, not a rewrite of this one's trait.
2. **`provider_config` carries no `tenant_id` column.** DESIGN.md's own §4.10 SQL snippet still
   shows one, but that pre-dates the convention T-005 (decision 6) and T-007 (decision 2)
   established and shipped: a tenant-database table never carries `tenant_id`, because the
   tenant already is the database (§2.1). Same doc drift T-007 already corrected for
   `tenant_config`'s snippet; the Docs step below applies the identical fix here. Primary key is
   `(channel, priority)`.
3. **`credential_path` is stored, not read.** Vault KV-secret retrieval doesn't exist in this
   codebase yet — `keystore.rs` only wraps Transit (DEKs), and no KV mount/path convention has
   ever been decided for provider credentials. Confirmed with the user: this ticket defers that
   wiring to a later ticket, mirroring T-007 decision 4's "the column exists, nothing reads it
   yet" pattern. `HttpSender` takes its credential (`api_key`) as a plain constructor argument;
   nothing in this ticket resolves a `credential_path` against Vault.
4. **`rate_limit_per_sec` is stored, not enforced.** No reader exists yet — rate limiting against
   it is the dispatcher's job (T-013, not yet refined). Same deferral shape as decision 3.
5. **`Sender` is channel-agnostic.**
   `async fn send(&self, destination: &str, body: &str) -> Result<SendOutcome, SenderError>`
   carries no SMS-specific field, so email/WhatsApp (§14 step 11) can implement the same trait
   later without a signature change. `HttpSender` is this ticket's one concrete implementation,
   shaped for SMS-style short text bodies but not type-restricted to them.
6. **No production/mock guard on `Sender`, unlike `AuthProvider`'s `MockProvider`.** The
   `AuthProvider` guard (§11.1) exists because a mock authentication provider reachable in
   production is a full auth bypass — a security control. `Sender` has no equivalent hazard: an
   `HttpSender` pointed at the wrong `base_url` fails loudly (a connection error), it does not
   silently grant anything. No `MockSender` ships in `src/` — test doubles for `Sender` are
   local to whichever test needs one; this ticket's own unit tests use `wiremock` instead
   (decision 7).
7. **Adapter tests run against a local mock HTTP server (`wiremock`), never a live network
   call.** Confirmed with the user during refinement — no real provider account or credentials
   exist in this environment. `wiremock` is added as a dev-dependency; `HttpSender`'s `base_url`
   is a plain constructor argument so a test can point it at the mock server's own address.
8. **`reqwest` moves from `[dev-dependencies]` to `[dependencies]`.** It was only a test-side
   HTTP client for `tests/ingest.rs` until now; `HttpSender` is the first production code path
   that needs an HTTP client. The existing `rustls-tls`/`json` features already cover what it
   needs — no feature-flag change.

### Tasks

#### Task 1 — Cargo dependencies

Edit `Cargo.toml`:

- Move `reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }`
  from `[dev-dependencies]` to `[dependencies]` (decision 8).
- Add `wiremock = "0.6"` to `[dev-dependencies]` (decision 7).

#### Task 2 — `provider_config` migration

Create `migrations/tenant/0006_provider_config.sql`:

```sql
-- Per-channel ordered provider list (DESIGN.md §4.10, §12.1), tenant
-- database. No tenant_id column, matching 0001_producer.sql and
-- 0002_tenant_config.sql (§2.1 — the tenant already is the database);
-- DESIGN.md's own §4.10 snippet is corrected to match in this ticket's
-- Docs step (T-012 decision 2).
CREATE TABLE provider_config (
    channel            text     NOT NULL,
    priority           smallint NOT NULL,
    provider           text     NOT NULL,       -- free-text label; no vendor wired yet (T-012 decision 1)
    credential_path    text     NOT NULL,       -- Vault path; not read yet (T-012 decision 3)
    rate_limit_per_sec int      NOT NULL,       -- not enforced yet (T-012 decision 4)
    PRIMARY KEY (channel, priority)
);
```

No new migration runner needed — `provision_tenant` already runs
`sqlx::migrate!("./migrations/tenant")`; an existing dev tenant picks this up on its next
(idempotent) re-provision.

#### Task 3 — `Sender` trait + `HttpSender` adapter

Add `src/sender/mod.rs`:

- `SendOutcome { pub provider_ref: String, pub provider_status: String }` — mirrors the
  `comms_event.provider_ref`/`provider_status` columns (§4.4) the dispatcher will eventually
  write from it.
- `SenderError` enum: `Http(reqwest::Error)` (network/transport failure) and
  `Provider { status: u16, body: String }` (a non-2xx response) — `Display`/`Error`/
  `From<reqwest::Error>` in `KeyStoreError`'s shape (`src/keystore.rs`).
- `#[async_trait] pub trait Sender: Send + Sync { async fn send(&self, destination: &str, body: &str) -> Result<SendOutcome, SenderError>; }`
  (decision 5).
- `pub mod http;`

Add `src/sender/http.rs`:

- `pub struct HttpSender { client: reqwest::Client, base_url: String, api_key: String }` with
  `pub fn new(base_url: String, api_key: String) -> Self` building a plain `reqwest::Client`.
- `impl Sender for HttpSender`: `POST {base_url}/messages`, `Authorization: Bearer {api_key}`,
  JSON body `{"to": destination, "body": body}`. A 2xx response deserializes as
  `{"message_id": String, "status": String}` into
  `SendOutcome { provider_ref: message_id, provider_status: status }`. A non-2xx response reads
  the body as text and returns `SenderError::Provider { status, body }`. Any `reqwest::Error`
  (connection failure, timeout, decode failure) maps via `?`/`From` to `SenderError::Http`.
- `#[cfg(test)] mod tests` using `wiremock::{MockServer, Mock, ResponseTemplate}` (decision 7):
  a success case (asserts `provider_ref`/`provider_status` parsed correctly and the request
  body/headers `wiremock` observed), a `500` case (asserts
  `SenderError::Provider { status: 500, .. }`), and an unreachable-`base_url` case (asserts
  `SenderError::Http(_)`).

Add `pub mod sender;` to `src/lib.rs`.

#### Task 4 — `provider_config` model

Add `src/provider_config/model.rs`, mirroring `src/tenant_config/model.rs`'s
`TenantConfig`/`TenantConfigInput` split:

- `ProviderConfig` — `#[derive(Debug, Clone, sqlx::FromRow)]`: `channel: String`,
  `priority: i16`, `provider: String`, `credential_path: String`, `rate_limit_per_sec: i32`.
- `ProviderConfigInput` — same five fields, `#[derive(Debug, Clone, PartialEq)]`, plus
  `pub fn matches(&self, existing: &ProviderConfig) -> bool` comparing all five fields
  (`configure::set_provider_config`'s created/updated/idempotent check).

#### Task 5 — `provider_config` repository

Add `src/provider_config/repo.rs`, following `src/tenant_config/repo.rs`'s shape:

- `pub async fn list(pool: &PgPool, channel: &str) -> Result<Vec<ProviderConfig>, sqlx::Error>` —
  `SELECT` the five columns `WHERE channel = $1 ORDER BY priority`.
- `pub async fn load_one(pool: &PgPool, channel: &str, priority: i16) -> Result<Option<ProviderConfig>, sqlx::Error>` —
  `WHERE channel = $1 AND priority = $2`, `fetch_optional`; used by
  `configure::set_provider_config` to decide created/updated/idempotent.
- `pub async fn upsert(pool: &PgPool, input: &ProviderConfigInput) -> Result<(), sqlx::Error>` —
  `INSERT ... ON CONFLICT (channel, priority) DO UPDATE SET ...` for `provider`,
  `credential_path`, `rate_limit_per_sec`.

#### Task 6 — `provider_config` configure operation

Add `src/provider_config/configure.rs`, following `src/tenant_config/configure.rs`'s shape
exactly (resolve `tenant_slug`, connect the tenant pool, load-compare-upsert, audit):

- `ConfigureError`/`ConfigureOutcome` — identical shape to `tenant_config::configure`'s.
- `pub async fn set_provider_config(control_pool: &PgPool, base_db_url: &str, tenant_slug: &str, input: ProviderConfigInput, profile: Profile, actor: &str) -> Result<ConfigureOutcome, ConfigureError>` —
  resolves `tenant_slug` via `tenant::repo::find_by_slug`; on `None`, audits a
  `provider_config.set`/`rejected` row (`tenant_id: None`, per T-007 decision 8 / the T-005/F1
  pattern) and returns an error; on `Some`, opens the tenant pool, calls
  `repo::load_one(channel, priority)`, compares via `input.matches`, `upsert`s when not already
  identical, audits `created`/`updated`/`idempotent` (detail JSON: `channel`, `priority`,
  `provider`, `credential_path`, `rate_limit_per_sec`, `outcome`), closes the pool.
- `pub async fn list_provider_config(control_pool: &PgPool, base_db_url: &str, tenant_slug: &str, channel: &str, profile: Profile) -> Result<Vec<ProviderConfig>, ConfigureError>` —
  resolve, connect, `repo::list(channel)`, close; mirrors `show_tenant_config`'s shape.

#### Task 7 — Wiring

Add `src/provider_config/mod.rs` (`pub mod configure; pub mod model; pub mod repo;`) and
register `pub mod provider_config;` in `src/lib.rs`.

#### Task 8 — `messgr-control provider-config` subcommands

Extend `src/bin/control.rs` with a `ProviderConfig { command: ProviderConfigCommand }` variant,
following the `TenantConfig` arm's shape:

- `Set { tenant_slug: String, channel: String, priority: i16, provider: String, credential_path: String, rate_limit_per_sec: i32, actor: String }`
  (all `#[arg(long = "...")]`, matching `TenantConfigCommand::Set`'s kebab-case flag convention).
  Builds a `ProviderConfigInput`, calls `set_provider_config`, prints `outcome=<outcome>`.
- `List { tenant_slug: String, channel: String }` — calls `list_provider_config`, prints one
  line per row ordered by priority: `priority=<p> provider=<provider> credential_path=<path>
  rate_limit_per_sec=<n>`, or `no provider configured for channel <channel>` when empty.

No Vault client is connected for either arm, matching the `TenantConfig`/`Producer` arms — extend
the existing comment above `Command::TenantConfig` in `control.rs` to cover this arm too.

Add a small unit test in `src/bin/control.rs` (`#[cfg(test)] mod tests`, using
`Cli::try_parse_from`), following the existing `tenant-config set` parse test, asserting
`provider-config set` parses its required flags into the right `Command::ProviderConfig`
variant.

#### Task 9 — Integration tests

Add `tests/provider_config.rs`, following `tests/tenant_config.rs`'s conventions exactly
(`unique_name`, real provisioning via `provision_tenant`, `drop_test_tenant`-style best-effort
cleanup). Cover:

1. `list` returns empty immediately after provisioning, before any `set_provider_config` call.
2. `set_provider_config` then `repo::list` round-trips every typed field for a single
   `(channel="sms", priority=1)` row.
3. A second `set_provider_config` for `(sms, 2)` makes `list("sms")` return both rows **ordered
   by priority** — the "ordered list, even at length 1, extends without a schema change"
   property the ticket exists to prove (Outcome).
4. Re-`set_provider_config` with identical inputs at the same `(channel, priority)` is
   idempotent: `outcome == "idempotent"` on the second call, row count for that
   `(channel, priority)` stays 1.
5. Re-`set_provider_config` with a different `rate_limit_per_sec` at the same
   `(channel, priority)` returns `outcome == "updated"`, `repo::list` reflects the new value.
6. `set_provider_config` against an unknown `--tenant-slug` is rejected **and** writes a
   `provider_config.set`/`rejected` `platform_audit` row with `tenant_id IS NULL` (the T-005/F1
   pattern, T-007 decision 8).

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/provider_config.rs, src/sender/http.rs's
               # wiremock unit tests, and the new control.rs unit test
```

Then exercise the CLI end to end against a real tenant:

```
just provision acme eu tenant_acme operator@example.com
cargo run --bin messgr-control -- provider-config set --tenant-slug acme \
    --channel sms --priority 1 --provider generic-http \
    --credential-path secret/data/acme/sms --rate-limit-per-sec 10 \
    --actor operator@example.com
cargo run --bin messgr-control -- provider-config list --tenant-slug acme --channel sms
```

Expected: `set` prints `outcome=created`; `list` prints the one `priority=1 ...` line.
Re-running the identical `set` command prints `outcome=idempotent`. Adding a second row at
`--priority 2` and re-running `list` prints both, in priority order.

Verify the row and the audit trail:

```
psql postgres://messgr:messgr@localhost:5432/tenant_acme -c "SELECT * FROM provider_config ORDER BY priority"
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT action, detail->>'outcome' FROM platform_audit WHERE action = 'provider_config.set' ORDER BY at"
```

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control provider-config` subcommands, plus the
`Sender`/`HttpSender` addition to the crate's send-path story.

- `README.md` — add a "### Provider configuration" section after "### Tenant configuration",
  documenting `provider-config set|list`, the ordered-per-channel-list shape, and that
  `credential_path`/`rate_limit_per_sec` are stored but not yet read (decisions 3–4).
- `justfile` — add `provider-config-set`/`provider-config-list` recipes in the `control-plane`
  group, mirroring `tenant-config-set`/`tenant-config-show`.
- `DESIGN.md` §4.10 — correct the `provider_config` `CREATE TABLE` snippet to drop `tenant_id`
  and change the primary key to `(channel, priority)`, mirroring how `tenant_config`'s own
  snippet was corrected by T-007 for the identical reason (decision 2). Update the paragraph
  beneath the code block — currently "...later tickets: quiet-hours resolution, T-012's provider
  selection" — to note that T-012 ships the table and the `Sender` trait/adapter but leaves the
  real vendor choice open (Still Open #5 is not resolved by this ticket — decision 1).

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README, justfile, and DESIGN.md §4.10 updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred (Vault KV credential
   read, rate-limit enforcement, real vendor adapter — all explicitly out of scope per decisions
   1/3/4).
4. Suggested Conventional Commit message:

   ```
   feat(sender): add Sender trait, HTTP adapter, and provider_config (T-012)

   Adds the third tenant migration (provider_config, DESIGN.md §4.10) as an
   ordered per-channel provider list, a channel-agnostic Sender trait, and
   HttpSender — a generic JSON-over-HTTP adapter proven against a local
   wiremock server rather than a committed real vendor (DESIGN.md's own
   "provider selection" question, Still Open #5, stays open). A
   messgr-control provider-config set|list subcommand manages the list the
   same way tenant-config manages its own singleton row. credential_path
   and rate_limit_per_sec are stored but not yet read — Vault KV wiring and
   rate-limit enforcement are later tickets' work.
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration+model+repo / sender trait+adapter / configure+CLI / tests / docs is
   a natural split) before presenting them.
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
- 2026-09-01 — READY → IN DEVELOPMENT: picked up
- 2026-09-01 — IN DEVELOPMENT → IN REVIEW: acceptance green
