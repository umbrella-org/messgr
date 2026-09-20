---
id: T-048
title: Query API and UI: AuthProvider, MockProvider, customer/campaign views
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: XL
---

# T-048 — Query API and UI: AuthProvider, MockProvider, customer/campaign views

## Outcome

After this ships, a role-authenticated bank employee — not just someone with `psql` — can look
up a customer's cross-channel message timeline, inspect a single message's rendered content and
event history, and run campaign-reach queries, with every access enforced server-side by role
and every compliance/`customer_service` lookup logged.

## Description

Build-order step 13 (§14): `query-api`, a REST API with a published OpenAPI spec plus a
server-rendered Askama+htmx UI in the same binary (`10-query-api-ui.md` §11), reading only from
a streaming replica so an unbounded compliance search cannot starve ingestion.

Scope, per §11/§11.1/§11.2:

- Endpoints: `GET /comms` (filtered list), `GET /comms/{id}` (detail + event history),
  `GET /customers/{id}/timeline`, `GET /campaigns/{id}/reach` (aggregate reach/delivery-status
  counts — **added during refinement**: §11's own route list and this ticket's original
  Endpoints line omitted it, even though §11.2's "campaign reach summary" query pattern and
  §11.1's `campaign_ops` role both require one; a design-doc correction is Task 1 of the
  Implementation Plan, matching AGENTS.md's instruction to fix an error found in the design and
  say so plainly), `GET /producers/{id}/usage`, `GET /producers/{id}/quota`.
  (`POST /comms`, `POST /comms/bulk`, `DELETE /comms/{id}` already exist on `messgr-ingest`
  per T-011/T-041 — this ticket does not duplicate them, only reads.)
- **Scope decision, confirmed with the user during refinement: `AuthProvider` trait +
  `MockProvider` only.** `OidcProvider` — discovery-document fetch, JWKS validation,
  group-to-role mapping, and the `tenant_config` schema change (issuer/client_id/group_claim,
  §4.10) it would need — is deferred to a future ticket, gated on a tenant's IdP actually being
  available. This matches decision 2 ("OIDC, mocked initially") and open question 3 ("OIDC
  group-to-role claim mapping — needed only when real OIDC replaces the mock") as written; this
  ticket's original Endpoints/scope bullets listing `OidcProvider` as in-scope over-read those
  two references. The **mandatory** production guard still ships in the same first commit that
  introduces the `AuthProvider` trait: refuse to start if `auth.provider = "mock"` while
  `profile != "dev"`. §11.1 is explicit this guard "belongs in the first commit that introduces
  the trait rather than being retrofitted" — not a follow-up. `MockProvider` unblocks the rest of
  this ticket regardless of IdP availability (decision 2).
- Five roles enforced server-side, never in the UI layer: `customer_service` (single-customer
  only, no list/export), `compliance` (unrestricted search + export, every access audit-logged),
  `campaign_ops` (aggregates, no bodies), `comms_ops` and `admin` are read-only from this
  ticket's perspective — their write surfaces (kill switches, quota, producer registry, template
  approval) belong to T-049 (admin panel, step 14), not here.
- Views: customer timeline (all channels, `customer_alias`-expanded so merged customers show
  one history — §11.2), message detail (template version, rendered content, full event
  history), campaign reach summary (`(campaign_id, created_at)` index on the replica,
  §11.2 — no rollup table, no OLAP tier, cut and documented as unjustified at biweekly query
  frequency).

**History note.** An earlier, narrower attempt at this surface (T-027) was filed and dropped
before T-018/T-020/T-021/T-023/T-024 (open correctness/security tickets at the time) landed,
specifically because it proposed an *unauthenticated* web server — directly against §11.1's
guard. Those prerequisite tickets are now all in `6-done/`; this ticket is the real §11/§11.2
surface, not a repeat of that shortcut.

Coupling: T-049 (admin panel) is explicitly "same binary, same server-rendered stack" (§11.3)
— it extends whatever this ticket stands up and carries a hard `depends-on: [T-048]` (this
ticket) for that reason.

Out of scope: `messgr-control` / platform console (§11.4, already separate, different binary
and auth realm), the rollup/OLAP tier (explicitly deferred), the `OidcProvider` implementation
itself — not just its live-IdP wiring — per the scope decision above (left for a future ticket).

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd /Users/nka/Projects/messgr
git checkout main
git checkout -b feat/T-048-query-api-and-ui-authprovider-mockprovider-customer-campaign-views
```

WIP commits encouraged. Publish only per the project's commit policy (`path = "."`,
`layout = "in-tree"` — no push/MR without explicit user approval; tidy WIP into atomic commits
before presenting; verify `origin/main...HEAD` carries no `tickets/` path before pushing).

### Prerequisite gate (hard)

None. `depends-on: []`. Board WIP clear: `3-in-development/` 0/1, `4-in-review/` 0/1. Every
build-order prerequisite step (0–4, including all three gates, T-036/T-037/T-038) is in
`6-done/` and merged.

### Confirmed design decisions (do not deviate without asking)

1. **`AuthProvider` trait + `MockProvider` only; `OidcProvider` is a future ticket.** Confirmed
   with the user (see Description). No `tenant_config` schema change in this ticket.
2. **`MockProvider` needs no session/cookie/login flow.** Design's own table says it is "static
   user table, no network, role selected by config" — it authenticates *every* request from
   process config (env vars), not from a per-request credential. So this ticket adds no login
   page, no callback route, and no session-cookie dependency. `AuthProvider::authenticate` is
   shaped so a future `OidcProvider` (real redirect + session cookie, reading the request itself)
   slots in behind the same trait without touching any call site.
3. **Tenant resolution is a path prefix, `/t/{tenant_slug}/...`, not hostname-based.** §11.1 names
   either as legal; hostname-based virtual-hosting needs DNS/vhost infrastructure this project has
   nowhere else. A middleware resolves `tenant_slug` → `tenant::repo::find_by_slug` → tenant pool,
   before `AuthProvider::authenticate` runs, matching §11.1's "tenant resolution happens... before
   the OIDC flow starts." Hostname routing can be swapped in later behind the same extractor
   without touching handlers.
4. **Reuse `webhook::TenantPoolCache` verbatim** for the per-tenant pool cache — it is already
   tenant-agnostic (keyed on `tenant_id`, built on `connect_tenant_pool`) despite living in
   `src/webhook/mod.rs`; `messgr-query-api` is, like `messgr-webhook`, a long-lived multi-tenant
   server process with the identical need. No second cache type is written.
5. **New `QUERY_API_DATABASE_URL` env var, read directly by `src/bin/query_api.rs`** (mirroring
   how `webhook.rs` reads its own listen-address/TLS env vars directly rather than through the
   shared `Config` struct) — the base DSN `TenantPoolCache::get_or_open` connects tenant pools
   through. Points at the streaming replica in production (§11: "reads never touch the primary").
   Falls back to `CONTROL_DATABASE_URL`'s value when unset, since dev/`compose.yml` stands up one
   Postgres instance with no replica — a documented dev-only gap, not a second code path (§2.1
   invariant 8: one codebase, no build flags). Whoever wires a real replica into `compose.yml`/CI
   sets the env var; no code changes.
6. **`GET /comms/{id}` takes `created_at` as a required query parameter
   (`?created_at=<rfc3339>`), not `id` alone.** **Correction found during T-048's applicability
   gate (2026-09-20):** this decision originally claimed "there is no index that makes an
   `id`-only lookup efficient across partitions" — that claim is wrong.
   `migrations/tenant/0015_comms_request_id_index.sql` (T-041, already merged) added
   `CREATE INDEX ON comms_request (id)` specifically to serve "DELETE /comms/{id} (and the
   future GET /comms/{id}, §10)" per its own comment, so an id-only lookup is efficient. The
   compound-key route is kept anyway, on narrower grounds: `comms_request`'s primary key is
   `(created_at, id)` because the table is partitioned by `created_at` range
   (`migrations/tenant/0004_ledger_outbox_schema.sql`), and every existing lookup in this
   codebase (`dispatcher::repo::load_ciphertexts`, `write_terminal`, …) takes that compound key,
   never `id` alone. A caller always already has `created_at` from whatever list/timeline view
   linked to this detail page. Matches this codebase's existing key shape rather than
   introducing the first `id`-only lookup path alongside it for one route (review-addendum step
   2 item 1: "verbatim from the design is not a defence" — cuts the other way here too: the
   existence of 0015's index is not by itself a reason to add a second lookup convention).
7. **New index `CREATE INDEX ON comms_event (comms_request_id, occurred_at)`.** `comms_event` has
   no existing index on `comms_request_id` alone (only `(customer_id, occurred_at)`), and the
   message-detail event-history lookup is a much higher-frequency path (every support-agent
   lookup) than the campaign query §11.2 sizes at "roughly biweekly." The `(campaign_id,
   created_at)` index §11.2 calls out for the campaign-reach query **already exists** on
   `comms_request` since migration `0004` — confirmed by reading the migration, not assumed from
   the design doc; no new migration needed for that one.
8. **`access_audit` (the compliance access-audit log, §11.1) is a named exemption in
   `tests/erasure_coverage.rs`, not covered by erasure.** Confirmed with the user: it is evidence
   of what a compliance user did, not the customer's own data — the same reasoning
   `06-pii-retention.md` already gives for `suppression`/`orphan_event`'s exemptions — and a
   bank's audit trail is expected to outlive the record it describes. `access_audit.customer_id`
   is nullable (a list/campaign search names no single customer).
9. **No OpenAPI-codegen dependency.** The published spec is a small, fixed, hand-authored
   `openapi/query-api.yaml` (OpenAPI 3.0) covering the six routes below, served statically at
   `GET /t/{tenant_slug}/openapi.yaml` (unauthenticated — it documents shapes, not data) via
   `include_str!`. Six routes don't justify a derive-macro toolchain (e.g. `utoipa`) on top of
   hand-writing the handlers anyway.
10. **UI dependency: add `askama = "0.12"`.** The design mandates Askama+htmx explicitly (§11) —
    not a discretionary dependency choice. `htmx` itself is vendored as a static file,
    `assets/htmx.min.js` (checked into the repo, served by a plain `GET` route via
    `include_bytes!`), rather than pulled from a CDN — this is an on-prem-capable system (§2.1)
    that must not assume the query-api process has outbound internet access.
11. **Role enforcement is one extractor (`query_api::auth_mw::AuthedUser`) plus a small
    `require_role` check, never in template logic** (§11.1: "enforced server-side on every query,
    never in the UI layer"). The role → endpoint matrix (Task 6) is the single place that
    encodes which of the five roles may call which route.
12. **`customer_service` is restricted at the extractor, not the handler.** It must supply
    `customer_id` and that id must match the route's own `{id}` (on `/comms/{id}` — whose owning
    `customer_id` is loaded and compared — and `/customers/{id}/timeline`); it gets a `403` from
    `GET /comms` (list/search), `GET /campaigns/{id}/reach`, and both `/producers/*` routes
    outright, before any repo call.
13. **`GET /producers/{id}/usage` and `/quota` reuse `producer_quota::repo::load_current_usage`
    / `load_one` / the existing `ProducerQuota` model directly** — thin handlers, no new query
    logic. Scoped to `comms_ops` only, matching §11.1's role table literally (`admin`'s listed
    scope is template/policy/provider/registry/override *configuration*, not this dashboard;
    T-049 can widen the role list if it turns out to need to — noted as a soft coupling, not
    decided here).
14. **Message-detail body decryption: `customer_dek::repo::find` → `keystore.unwrap_dek` →
    `encryption::decrypt`.** **Correction found during Task 4/5 implementation (2026-09-20):**
    the original wording ("reuses `customer_dek::repo::find` + `encryption::decrypt` exactly as
    the dispatcher does") skipped the Vault unwrap step the dispatcher's own path actually needs
    in between (`src/dispatcher/worker.rs` calls `lifecycle::get_or_create_dek`, which internally
    unwraps via `keystore.unwrap_dek(mount, &row.wrapped_dek)` before `encryption::decrypt` can
    run — `customer_dek::repo::find` alone only returns the still-wrapped DEK). This binary needs
    a `KeyStore` it did not otherwise have.
    - Not `lifecycle::get_or_create_dek`: its *create*-on-miss branch is the wrong behavior for a
      read surface — a `comms_request` row only exists because ingest already created a DEK for
      that customer, so a missing `customer_dek` row here is a data-integrity bug, not a "mint a
      fresh one" case (a freshly minted DEK cannot decrypt ciphertext written under the original
      one). `comms_query::repo::detail` treats `find` returning `None` as an error, not a
      fallback.
    - `AppState` gains `keystore: Arc<dyn KeyStore>`, constructed in Task 4's `query_api.rs`
      exactly like `webhook.rs` builds its own (`VaultKeyStore::connect(config.profile)`,
      `VAULT_ADDR`/`VAULT_TOKEN` from the environment) — read-only Transit *unwrap* capability
      scoped to whatever policy this tenant's AppRole carries, the same posture §11's "rendered
      content subject to §7" already implies for query-api (unlike `messgr-control`'s platform
      console, §11.4, which by design holds no Transit policy for any tenant mount at all).
    - `TenantContext` (Task 4) also carries `vault_mount: String` from the already-fetched
      `Tenant` row (`tenant::repo::find_by_slug` already loads it; no second query).
    - No `KeyCache` (`src/key_cache.rs`) added for this ticket — support/compliance lookups are
      not the hot, whole-outbox-throughput path `KeyCache` was sized for (T-008), so an unwrap
      per detail view is an acceptable cost for a first cut. `ponytail: no DEK cache on the
      query-api read path; add one (sized well below the dispatcher's 100k-entry cache) if
      repeated message-detail views on the same customer show up as Vault-call latency.`
    - `comms_query::repo::detail` (Task 5) returns the row with its ciphertext fields
      (`destination_ciphertext`, `payload_ciphertext`) un-decrypted — the decrypt step (DEK
      find/unwrap/decrypt) runs in the handler (Task 6, which already holds `AppState.keystore`
      and `TenantContext.vault_mount`), mirroring `dispatcher::repo` (DB-only) vs.
      `dispatcher::worker` (decrypts) rather than adding a Vault dependency to `comms_query::repo`
      itself.
15. **Customer-timeline alias expansion needs a new, separate function, not
    `customer::repo::expand_alias`.** `expand_alias` walks `old_customer_id → customer_id`
    *forward* — it answers "what id is this now, given one that might be stale," which is what
    ingest needs. The timeline view needs the *reverse*: given the canonical id the operator is
    looking at, every id that was ever merged into it, so ledger rows written under a
    since-superseded id still show up. New `customer::repo::alias_set(pool, canonical_id) ->
    Result<Vec<Uuid>, sqlx::Error>` (Task 2) does that with a bounded recursive CTE, mirroring
    `expand_alias`'s own `ALIAS_HOP_LIMIT` (8) as the recursion depth cap:
    ```sql
    WITH RECURSIVE aliases(id, depth) AS (
        SELECT $1::uuid, 0
        UNION ALL
        SELECT ca.old_customer_id, a.depth + 1
        FROM customer_alias ca
        JOIN aliases a ON ca.customer_id = a.id
        WHERE a.depth < 8  -- mirrors customer::repo::ALIAS_HOP_LIMIT
    )
    SELECT DISTINCT id FROM aliases
    ```
    `comms_query::repo::timeline` calls `alias_set` first, then queries
    `WHERE customer_id = ANY($1)`.
16. **Role → endpoint matrix** (enforced by decision 11's extractor):

    | Endpoint | `customer_service` | `compliance` | `campaign_ops` | `comms_ops` |
    |---|---|---|---|---|
    | `GET /comms` (search/list) | ✗ | ✓ audited | ✗ | ✗ |
    | `GET /comms/{id}` (detail) | ✓ own customer only | ✓ audited | ✗ | ✗ |
    | `GET /customers/{id}/timeline` | ✓ own customer only | ✓ audited | ✗ | ✗ |
    | `GET /campaigns/{id}/reach` | ✗ | ✓ audited | ✓ | ✗ |
    | `GET /producers/{id}/usage` | ✗ | ✗ | ✗ | ✓ |
    | `GET /producers/{id}/quota` | ✗ | ✗ | ✗ | ✓ |

    `admin` has no route in this ticket (§11.1's `admin` scope is write surfaces — T-049).
    "audited" = an `access_audit` row is written for every `compliance`-role call, successful or
    not, before the handler runs (decision 8).
17. **`access_audit` is written by one middleware, not per-handler.** An
    `axum::middleware::from_fn_with_state` layer wraps every data route and runs *before* the
    handler — it re-resolves tenant + identity itself (`TenantContext`, then
    `AuthProvider::authenticate`), independently of the handler's own `require_role` check, not
    after it. **Correction found during review (2026-09-20; F3):** the original wording said this
    middleware "runs after the role check," which doesn't match how an axum `.layer()` executes
    relative to the handler it wraps. If the resolved identity's role is `compliance`, it writes
    one row (`actor`, `role`, `route`, `customer_id` from the path when the route names one, else
    `NULL`, and the raw query string) before the handler ever runs — logged regardless of what the
    handler subsequently returns, including a `403` from the handler's own role check, since a
    zero-result, failed, or role-rejected search is still a compliance access. No handler calls
    `access_audit::repo::record` itself.

### Tasks

#### Task 1 — Design-doc correction: the missing campaign-reach route

`development/design/10-query-api-ui.md`: add `GET /campaigns/{id}/reach` to the `##` route
listing at the top of §11 (aggregate reach/delivery-status counts for a campaign), and a short
correction note directly beneath it, in this doc's existing correction-callout style, stating
that the route was missing from the original list despite §11.2's "campaign reach summary" query
pattern and §11.1's `campaign_ops` role both requiring one — found during T-048's refinement.

#### Task 2 — Schema: `access_audit`, the `comms_event` index, and `alias_set`

`migrations/tenant/0019_access_audit.sql`:

```sql
-- Compliance access-audit log (DESIGN.md §11.1, T-048): every query-api
-- access made under the `compliance` role is recorded here. Not
-- partitioned -- one row per compliance query, nowhere near ledger scale.
-- Named exemption from erasure (tests/erasure_coverage.rs, T-048 decision
-- 8): this is evidence of what a compliance user did, not the customer's
-- own data, so it outlives the record it describes -- same reasoning as
-- suppression/orphan_event's existing exemptions.
CREATE TABLE access_audit (
    id            uuid        PRIMARY KEY,
    occurred_at   timestamptz NOT NULL,
    actor         text        NOT NULL,
    role          text        NOT NULL,
    route         text        NOT NULL,
    customer_id   uuid,                  -- NULL for a query naming no single customer
    query_params  text        NOT NULL DEFAULT ''
);
CREATE INDEX ON access_audit (occurred_at);
CREATE INDEX ON access_audit (customer_id) WHERE customer_id IS NOT NULL;
```

`migrations/tenant/0020_comms_event_request_id_index.sql`:

```sql
-- Message-detail event history (DESIGN.md §11, T-048): comms_event has no
-- existing index on comms_request_id alone (only (customer_id,
-- occurred_at)); this lookup runs far more often than the campaign-reach
-- query the existing (campaign_id, created_at) index on comms_request
-- already serves.
CREATE INDEX ON comms_event (comms_request_id, occurred_at);
```

`src/access_audit/model.rs` + `src/access_audit/repo.rs`, mirroring `suppression`'s model/repo
split: `AccessAudit` (`sqlx::FromRow`), `AccessAuditInput`, one function —

```rust
pub async fn record(pool: &PgPool, input: &AccessAuditInput, now: DateTime<Utc>) -> Result<(), sqlx::Error>
```

Register `pub mod access_audit;` in `src/lib.rs`.

`src/customer/repo.rs`: add `alias_set` per decision 15, directly beneath `expand_alias`.

`tests/erasure_coverage.rs`: add `("access_audit", "<the decision-8 reasoning, one line>")` to
the `EXEMPT` list.

#### Task 3 — `src/auth/`: role vocabulary, `AuthProvider` trait, `MockProvider`

`src/auth/role.rs` (mirrors `suppression::reason`/`verification_mode`'s closed-string-vocab
convention):

```rust
pub mod role {
    pub const CUSTOMER_SERVICE: &str = "customer_service";
    pub const COMPLIANCE: &str = "compliance";
    pub const CAMPAIGN_OPS: &str = "campaign_ops";
    pub const COMMS_OPS: &str = "comms_ops";
    pub const ADMIN: &str = "admin";
}
```

`src/auth/provider.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Identity {
    pub actor: String,   // stable user identifier, for the access-audit log
    pub role: String,    // one of the `role` constants
    pub tenant_id: Uuid,
}

#[derive(Debug)]
pub enum AuthError {
    Unauthenticated,
}

#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    async fn authenticate(&self, tenant_id: Uuid) -> Result<Identity, AuthError>;
}
```

`src/auth/mock.rs`:

```rust
/// Local development and tests only (DESIGN.md §11.1) -- static identity
/// from config, no network. The guard lives in `new`, the only
/// constructor, so a `MockProvider` value can never exist outside
/// `profile = dev`: enabling this in production is a full authentication
/// bypass.
pub struct MockProvider {
    actor: String,
    role: String,
}

impl MockProvider {
    pub fn new(profile: Profile, actor: String, role: String) -> Self {
        if !profile.is_dev() {
            tracing::error!(
                "refusing to start: auth.provider=mock while MESSGR_PROFILE is not dev \
                 (DESIGN.md \u{a7}11.1 -- a mock auth provider reachable in production is a \
                 full authentication bypass)"
            );
            panic!("MockProvider is not permitted outside profile=dev");
        }
        Self { actor, role }
    }
}

#[async_trait::async_trait]
impl AuthProvider for MockProvider {
    async fn authenticate(&self, tenant_id: Uuid) -> Result<Identity, AuthError> {
        Ok(Identity { actor: self.actor.clone(), role: self.role.clone(), tenant_id })
    }
}
```

Register `pub mod auth;` (with `pub mod mock; pub mod provider; pub mod role;` inside) in
`src/lib.rs`.

#### Task 4 — `messgr-query-api` binary skeleton and per-request tenant/auth context

`Cargo.toml`: add `askama = "0.12"` to `[dependencies]`; add
`[[bin]] name = "messgr-query-api" path = "src/bin/query_api.rs"`.

`src/bin/query_api.rs`, mirroring `src/bin/webhook.rs`'s shape (clap `Version` subcommand, TLS via
`axum_server::tls_rustls`, a second health listener via `messgr::health`): reads
`QUERY_API_LISTEN_ADDR` (default `0.0.0.0:8544`), `QUERY_API_HEALTH_LISTEN_ADDR`,
`QUERY_API_TLS_CERT_FILE`/`QUERY_API_TLS_KEY_FILE`, `QUERY_API_DATABASE_URL` (decision 5),
`AUTH_PROVIDER` (only legal value for now: `mock`, checked with `PossibleValuesParser`-style
validation and a clear panic on anything else), `MOCK_AUTH_ACTOR`/`MOCK_AUTH_ROLE` (read only when
`AUTH_PROVIDER=mock`, which is the only case today). Builds `Config::from_env()`, connects
`control_pool`, constructs `MockProvider::new(config.profile, ...)` behind `Arc<dyn
AuthProvider>`, builds `webhook::TenantPoolCache::new()` (decision 4), builds `Arc<VaultKeyStore>`
behind `Arc<dyn KeyStore>` exactly like `webhook.rs` does (decision 14 — `VaultKeyStore::connect(config.profile)`,
`VAULT_ADDR`/`VAULT_TOKEN` from the environment), mounts the router (Task 6), starts the health
listener the same way the other three bins do.

`src/query_api/mod.rs`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub control_pool: PgPool,
    pub query_api_database_url: String,
    pub auth: Arc<dyn AuthProvider>,
    pub pool_cache: Arc<webhook::TenantPoolCache>,
    pub tenant_pool_max_connections: u32,
    pub keystore: Arc<dyn KeyStore>,
}
```

`src/query_api/tenant.rs`: an axum extractor `TenantContext { tenant_id: Uuid, pool: PgPool,
vault_mount: String }` implementing `FromRequestParts<AppState>` — reads the `:tenant_slug` path
segment, `tenant::repo::find_by_slug(&state.control_pool, &slug)` (404 on `None`; carries
`tenant.vault_mount` into the extractor, decision 14 — no second query), then
`state.pool_cache.get_or_open(&state.control_pool, &state.query_api_database_url, tenant.id,
&tenant.database_name, state.tenant_pool_max_connections)` (decision 3/4/5).

`src/query_api/auth_mw.rs`: an extractor `AuthedUser(pub Identity)` implementing
`FromRequestParts<AppState>`, built on top of `TenantContext` — calls
`state.auth.authenticate(tenant_ctx.tenant_id)`, mapping `AuthError::Unauthenticated` to a `401`.
A `require_role(identity: &Identity, allowed: &[&str]) -> Result<(), StatusCode>` function (a
function, not a macro — three lines, called from every handler per Task 6's matrix) returning
`403` on mismatch.

#### Task 5 — `src/comms_query/`: the read-model queries

`src/comms_query/filter.rs`: `CommsFilter` (`serde::Deserialize`, used via axum's `Query`
extractor) — `customer_id: Option<Uuid>`, `channel: Option<String>`, `class: Option<String>`,
`campaign_id: Option<String>`, `producer_id: Option<Uuid>`, `from: Option<DateTime<Utc>>`,
`to: Option<DateTime<Utc>>`, `status: Option<String>`, `scheduled: Option<bool>`.

`src/comms_query/repo.rs`:

- `pub async fn list(pool: &PgPool, filter: &CommsFilter, limit: i64) -> Result<Vec<CommsRequestSummary>, sqlx::Error>`
  — one fixed query, each filter field applied as `($n::type IS NULL OR column = $n)` (no dynamic
  SQL builder for a first cut — every predicate is optional-equality or a range, which this
  pattern covers without introducing `sqlx::QueryBuilder`); `scheduled = Some(true)` maps to
  `scheduled_for IS NOT NULL AND final_status IS NULL`. Ordered `created_at DESC`, capped at
  `limit`.
- `pub async fn detail(pool: &PgPool, created_at: DateTime<Utc>, id: Uuid) -> Result<Option<CommsRequestDetail>, sqlx::Error>`
  (decision 6's compound key) — loads the `comms_request` row plus its `template_id`/
  `template_version` resolved via `template::repo::find`. Returns `payload_ciphertext` and
  `destination_ciphertext` un-decrypted (`CommsRequestDetail` carries the raw ciphertext fields);
  decrypting them is `handlers::comms_detail`'s job (decision 14), not this function's — keeps
  `comms_query::repo` a DB-only layer with no `KeyStore` dependency, mirroring
  `dispatcher::repo`/`dispatcher::worker`'s own split.
- `pub async fn events(pool: &PgPool, comms_request_id: Uuid) -> Result<Vec<CommsEventRow>, sqlx::Error>`
  — `SELECT ... FROM comms_event WHERE comms_request_id = $1 ORDER BY occurred_at`, using Task 2's
  new index.
- `pub async fn timeline(pool: &PgPool, canonical_customer_id: Uuid, limit: i64) -> Result<Vec<CommsRequestSummary>, sqlx::Error>`
  — `customer::repo::alias_set` then `WHERE customer_id = ANY($1)` (decision 15).
- `pub async fn campaign_reach(pool: &PgPool, campaign_id: &str) -> Result<Vec<(Option<String>, i64)>, sqlx::Error>`
  — `SELECT final_status, COUNT(*) FROM comms_request WHERE campaign_id = $1 GROUP BY final_status`
  (§11.2: "aggregating on `final_status`"; `NULL` = still in flight).

#### Task 6 — REST handlers, routing, and the OpenAPI file

`src/query_api/handlers.rs` (one function per route, each: `require_role` per decision 16's
matrix → for `/comms/{id}` and `/customers/{id}/timeline`, an extra `customer_service`-owns-this-
customer check per decision 12 → call the matching `comms_query::repo`/`producer_quota::repo`
function → JSON response):

```
GET /t/{tenant_slug}/comms                       -> handlers::list_comms
GET /t/{tenant_slug}/comms/{id}                  -> handlers::comms_detail   (?created_at=... required)
GET /t/{tenant_slug}/customers/{id}/timeline     -> handlers::customer_timeline
GET /t/{tenant_slug}/campaigns/{id}/reach        -> handlers::campaign_reach
GET /t/{tenant_slug}/producers/{id}/usage        -> handlers::producer_usage
GET /t/{tenant_slug}/producers/{id}/quota        -> handlers::producer_quota
GET /t/{tenant_slug}/openapi.yaml                -> handlers::openapi_spec  (no auth, decision 9)
GET /assets/htmx.min.js                          -> handlers::htmx_asset    (no auth, decision 10)
```

`openapi/query-api.yaml`: hand-authored OpenAPI 3.0 document for the six data routes above
(decision 9), embedded via `include_str!("../../openapi/query-api.yaml")`.

`assets/htmx.min.js`: vendored htmx (pin a specific released version in a comment at the top of
the file, e.g. `htmx.org 1.9.x`), served via `include_bytes!` with
`content-type: application/javascript`.

Router assembly in `src/query_api/mod.rs`: nest the six data routes under `/t/{tenant_slug}`, each
wrapped with `axum::middleware::from_fn_with_state(state.clone(), access_audit_mw)` (decision 17);
mount `/openapi.yaml` inside the same nest, unauthenticated; mount `/assets/htmx.min.js` at the
router root.

#### Task 7 — Askama + htmx UI

`templates/query_api/base.html.j2` (or whatever extension the `askama.toml` config picks — base
layout: `<script src="/assets/htmx.min.js">`, nav, role/actor display).

Three view templates, each an Askama `Template` struct in `src/query_api/views.rs`, rendered by
a corresponding handler under the *same* nested `/t/{tenant_slug}` path + role checks as Task 6
(no separate auth path for the UI vs. the API — same `AuthedUser` extractor):

- `templates/query_api/timeline.html.j2` + `GET /t/{tenant_slug}/ui/customers/{id}/timeline` —
  renders `comms_query::repo::timeline`'s rows, `customer_service`/`compliance` only (decision 16).
- `templates/query_api/message_detail.html.j2` + `GET /t/{tenant_slug}/ui/comms/{id}` — template
  version, rendered content, full event history (`comms_query::repo::detail` + `events`); the
  same `?created_at=` requirement as the API route (decision 6).
- `templates/query_api/campaign_reach.html.j2` + `GET /t/{tenant_slug}/ui/campaigns/{id}/reach` —
  `campaign_ops`/`compliance` only.

Each Askama `Template` implements `axum::response::IntoResponse` by calling `.render()` and
mapping a render error to a `500` — no `askama_axum`/`askama_web` adapter crate added for that
one method (ponytail: one dependency's worth of adapter code is not worth a second dependency).

#### Task 8 — Wiring

`src/lib.rs`: `pub mod access_audit; pub mod auth; pub mod comms_query; pub mod query_api;`.

### Acceptance test

New `tests/query_api.rs` (provisions a real tenant, no mocks, following `tests/dispatcher.rs`'s/
`tests/suppression.rs`'s conventions; drives the axum `Router` in-process via
`tower::ServiceExt::oneshot` — no listening socket needed):

1. `mock_provider_refuses_to_construct_outside_dev_profile` (`src/auth/mock.rs`'s own
   `#[cfg(test)]`, `#[should_panic]`) — direct proof of decision 8/Task 3's guard.
2. `customer_service_role_can_only_reach_its_own_customer_timeline` — `GET
   /t/{slug}/customers/{own_id}/timeline` → `200`; `GET /t/{slug}/customers/{other_id}/timeline`
   → `403`; `GET /t/{slug}/comms` → `403`.
3. `compliance_role_search_is_audited` — `GET /t/{slug}/comms?channel=sms` → `200`; assert exactly
   one new `access_audit` row with `role = 'compliance'`, `route` matching, `customer_id IS NULL`
   (a channel-only filter names no customer).
4. `comms_detail_requires_matching_created_at` — seed one row, request with the correct
   `created_at` → `200` with the right body; request with a `created_at` one second off → `404`
   (direct proof of decision 6 — an `id`-only lookup would find it regardless).
5. `customer_timeline_includes_rows_written_under_a_merged_alias` — insert a `customer_alias` row
   (`old_customer_id = A`, `customer_id = B`) and a `comms_request` under `A`; request the timeline
   for `B`; assert the row under `A` appears (direct proof of decision 15's `alias_set`, the one
   this ticket's refinement found `expand_alias` would get backwards).
6. `campaign_reach_aggregates_by_final_status` — seed rows across at least two distinct
   `final_status` values (plus one still-`NULL`/in-flight) for one `campaign_id`; assert the
   returned counts match; `customer_service` role on the same route → `403`.
7. `producers_usage_and_quota_are_comms_ops_only` — `compliance` and `campaign_ops` roles both get
   `403` on `GET /producers/{id}/usage` and `/quota`; `comms_ops` gets `200` with the values
   `producer_quota::repo::load_current_usage`/`load_one` return directly.
8. `openapi_yaml_is_served_without_authentication` — `GET /t/{slug}/openapi.yaml` with no
   `AuthProvider` configured to accept the caller → `200` (decision 9).

Run: `just build && just test && just lint && just docs-check`.

### Docs update (mandatory when user-facing)

New `docs/user-manual/query-api.adoc`, mirroring `docs/user-manual/webhook.adoc`'s structure:
binary purpose and env vars (`QUERY_API_LISTEN_ADDR`, `QUERY_API_TLS_CERT_FILE`/`KEY_FILE`,
`QUERY_API_DATABASE_URL` and its dev-only fallback, `AUTH_PROVIDER`/`MOCK_AUTH_ACTOR`/
`MOCK_AUTH_ROLE`), the five roles and decision 16's matrix, the six routes plus `/openapi.yaml`,
and an explicit callout that `MockProvider` is dev-only and `OidcProvider` is a future ticket.
Register it in `docs/user-manual.adoc` with `include::user-manual/query-api.adoc[leveloffset=+1]`
(appended after the existing `kill-switches.adoc` line).

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint`, `just docs-check` clean.
2. `docs/user-manual/query-api.adoc` added and registered as above.
3. Write a summary (files touched, decisions made, anything deferred — in particular
   `OidcProvider` and the `tenant_config` OIDC columns, decision 1) and hand back.
4. Suggested commit message:

   ```
   feat(query-api): read-only REST API + UI with AuthProvider/MockProvider and RBAC (T-048)

   Adds messgr-query-api: five read endpoints plus a discovered-missing
   campaign-reach route, server-rendered Askama+htmx views over the same
   data, an AuthProvider trait (MockProvider only; OidcProvider deferred)
   with the mandatory dev-only production guard, server-side role
   enforcement for all five roles, and a compliance access-audit log
   (named erasure exemption).
   ```

5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits (root-path
   child) before presenting.
6. Commit locally on
   `feat/T-048-query-api-and-ui-authprovider-mockprovider-customer-campaign-views`. Do not push or
   open an MR without user approval. Present the commit message; after approval, verify
   `origin/main...HEAD` carries no `tickets/` path, then push and open the MR. Merging is the
   human's.

## Review

**Reviewer independence (step 0):** fresh session (conversation cleared before this review
began), no memory of authoring this branch — proceeding as an independent review directly, no
delegation triggered. Heavy code-reading (steps 2–4a) was run by a same-context fork to keep
this review's own context lean; all forked findings were re-verified by hand against the actual
files before being recorded here, per step 0's "delegation buys independence, not accuracy."

**Commands (step 2):** `just build` — pass. `just lint` (`cargo fmt --all -- --check` +
`cargo clippy --all-targets --all-features -- -D warnings`) — pass; addendum step 2 item 8
(justfile/CI parity) N/A, this branch's only `justfile` change is an additive `query-api-run`
recipe. `just docs-check` — pass. `just test` (`cargo test`, full default-parallel run) — 20
failures, all in the pre-existing `tests/dispatcher.rs` suite (a file this branch does not
touch); isolated re-run `cargo test --test dispatcher -- --test-threads=1` — 30/30 pass.
Read the diff (`git diff main...HEAD --stat`) — no dispatcher-touching files in it, confirming
this is parallel-execution resource contention in an unrelated, pre-existing suite, not a
regression T-048 introduced. This ticket's own acceptance suite — `tests/query_api.rs`'s 7 named
tests plus `src/auth/mock.rs`'s own `#[should_panic]` unit test (8 total, matching the plan) —
all pass, and each is genuinely falsifiable (real DB row-count/status-code assertions, not
`is_err()`-only checks; addendum step 3's mutation-test bar).

**Implementation, quality, consistency, docs (steps 2–4a):** all 8 Tasks and all 17 confirmed
decisions verified present and correct against the actual code — schema, `AuthProvider`/
`MockProvider` (the dev-only guard lives in the sole constructor, genuinely unbypassable), the
five-role matrix and `customer_service`-owns check enforced server-side, decision 6's
compound-key `detail` lookup, decision 14's `find → unwrap_dek → decrypt` chain (not
`get_or_create_dek`), decision 15's reverse-direction `alias_set` CTE, no SQL built from
unparameterized input, no new `UNIQUE`/`ON CONFLICT` NULL-semantics hazard (migrations 0019/0020
add no such constraint), no secrets read outside `VAULT_ADDR`/`VAULT_TOKEN`, `access_audit`
correctly carries no `tenant_id` column (database-per-tenant, §2.1), no stale `§N`
cross-references introduced, `docs/user-manual/query-api.adoc` covers all routes/roles/env vars
and is registered. Findings below are the exceptions to that.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | stale-xref | — | New table `access_audit` (Task 2) was never added to `development/design/06-pii-retention.md` §7.2's named-exemption reasoning, contrary to Decision 8's own claim ("the same reasoning `06-pii-retention.md` already gives for `suppression`/`orphan_event`'s exemptions"). The doc's "these six tables ... are the complete named-exemption list" sentence is now false — the actual `EXEMPT` list has 8 entries. AGENTS.md hard invariant 6 / review-addendum step 2 item 5: blocking. | `development/design/06-pii-retention.md:65`; `tests/erasure_coverage.rs`'s `EXEMPT` array (8 entries, `access_audit` at the end) | Add an `access_audit` exemption paragraph to §7.2, mirroring the `suppression`/`orphan_event` style already there, and correct the "six tables" sentence to the current complete list. |
| F2 | non-blocking | stale-xref | noted | Same §7.2 "six tables ... complete list" sentence was already stale *before* this ticket: `webhook_receipt_staging` (added to `EXEMPT` by T-047) is also absent from `06-pii-retention.md`'s prose and count. Not caused by this branch (rules §5's causation test — "did this branch break it?" — no), so not eligible for `fixed inline`; no existing ticket owns this ground, so `noted` rather than `folded`. | `development/design/06-pii-retention.md:65`; `tests/erasure_coverage.rs`'s `webhook_receipt_staging` entry | Convenience note: whoever fixes F1 touches this exact sentence, so folding this correction into that same edit costs nothing extra — not an obligation of this review. |
| F3 | non-blocking | stale-xref | fixed inline | Decision 17 described the `access_audit` middleware as running "after the role check"; the shipped code re-resolves tenant + identity itself in the middleware, independent of the handler's own `require_role`, and executes as a `.layer()` wrapping the routes — i.e. before the handler, not after. Behaviour matches intent (audit fires unconditionally for the `compliance` role, including on an eventual `403`); only the plan's descriptive prose was wrong, and this branch's own decision-17 text is what made it false. | `src/query_api/handlers.rs:345-369` (`access_audit_mw` re-resolves `TenantContext` + calls `state.auth.authenticate` itself); `src/query_api/mod.rs:50-58` (`.layer()` wraps the data routes) | Fixed in this review — Decision 17's text corrected above. |

Disposition summary: 1 fixed inline (F3), 1 noted (F2), 1 blocking → `5-rework/` (F1). No `new
ticket`/`folded` dispositions this round.

cost: estimated XL, actual XL

**Docs/governing-document reconciliation (step 7):** F1/F2 are the only governing-document gaps
found; F1 goes to rework (blocking, addendum-elevated), F2 is `noted` (pre-existing, out of this
branch's causation). `development/design/10-query-api-ui.md` §11's route list was already
corrected by this ticket's own Task 1 (the missing `GET /campaigns/{id}/reach` route) — verified
present, no further doc drift found in the design tree.

**Impact sweep (step 8):** `tickets/1-to-do/T-049-*.md` (`depends-on: [T-048]`) re-read — its
Description's assumptions ("same binary, same server-rendered stack," gated on T-048's
`AuthProvider`/role-gating, `admin` having no route in T-048) all still hold against what
actually shipped. No correction needed.

**Docs-readability pass (step 4b):** conscious skip — no docs-readability reviewer configured in
this session/host.

### Rework fix record — round 1 (commit 36689fd)

F1 fixed: added a `development/design/06-pii-retention.md` §7.2 "Named exemption: `access_audit`"
paragraph, mirroring the `suppression`/`orphan_event` style, carrying Decision 8's reasoning
(evidence of compliance-user activity, not the customer's own data; a bank's audit trail is
expected to outlive the record it describes). Also folded in F2's `webhook_receipt_staging`
paragraph at zero extra cost, per F2's own suggestion — correcting the "N tables ... complete
list" sentence to the actual current `EXEMPT` list (8 entries) required naming both missing
tables, not just the one this branch introduced; leaving `webhook_receipt_staging` out would
have left the corrected sentence still false. The summary sentence now reads "these eight tables"
and lists all eight `EXEMPT` entries by name.

Branch tip before this fix: `e4e336c`. Diff: `git diff e4e336c..36689fd` —
`development/design/06-pii-retention.md` only (no code touched, so no re-run of the acceptance
test's *behaviour* was needed; re-ran anyway for hygiene). `just build`/`just lint`/
`just docs-check` clean; `cargo test --test query_api` — 7/7 pass; `cargo test --test
erasure_coverage` — 1/1 pass (unaffected by a docs-only change, run to confirm the `EXEMPT`
count referenced above is still accurate).

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 13, remaining gap identified when auditing unticketed steps against the board
- 2026-09-20 — TO DO → READY: plan complete
- 2026-09-20 — plan amended inline: applicability-gate audit found Decision 6's stated
  justification factually wrong (claimed no id-only index exists on `comms_request`; migration
  0015/T-041 already added one for this exact future route). Kept the compound-key route on
  narrower grounds (matches existing lookup convention) and corrected the prose; no other
  findings from the audit.
- 2026-09-20 — READY → IN DEVELOPMENT: picked up
- 2026-09-20 — plan amended inline: found during Task 4/5 implementation that Decision 14's
  wording skipped the Vault-unwrap step the dispatcher's real decrypt path uses
  (`customer_dek::repo::find` alone returns only the wrapped DEK); `messgr-query-api` had no
  `KeyStore` wired in at all. Added `AppState.keystore: Arc<dyn KeyStore>` (built exactly like
  `webhook.rs`'s own `VaultKeyStore::connect`) and `TenantContext.vault_mount`; corrected Task
  4/5 text accordingly. Confirmed against `development/design/10-query-api-ui.md` §11 that
  decrypt is intentionally in scope for query-api (unlike the platform console, §11.4, which by
  design holds no Transit policy at all) — this is a wiring gap, not a scope question.
- 2026-09-20 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-20 — IN REVIEW → REWORK: F1 blocking: access_audit missing from DESIGN.md §7.2's erasure/exemption statements (addendum step 2 item 5, AGENTS.md hard invariant 6)
- 2026-09-20 — REWORK → IN REVIEW: findings fixed
