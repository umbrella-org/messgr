---
id: T-004
title: Per-tenant Transit mount + AppRole creation wired into the provisioning command
project: messgr
depends-on: [T-003]
spawned-by: []
impact: high
complexity: high
cost: L
---

# T-004 — Per-tenant Transit mount + AppRole creation wired into the provisioning command

## Outcome

Provisioning a new tenant (`messgr-control provision`, from T-001) now creates that tenant's own
Vault Transit mount, key, ACL policy, and AppRole as part of the same command — not a manual
follow-up. The RoleID is recorded in the control database; the SecretID is printed once,
response-wrapped, for out-of-band delivery to that tenant's dispatcher deployment, and is never
persisted anywhere. From this ticket onward, every tenant has a working, isolated Vault identity
from the moment `provision` returns — the substrate `KeyStore` (T-003) was built to talk to, and
T-009's DEK lifecycle will be the first real caller of it.

## Description

T-003 built the key-management substrate — the `KeyStore` trait, a Transit client, and a
dev-mode Vault in Compose — but deliberately left one seam open: nothing yet creates a
per-tenant Transit mount or AppRole. T-004 fills that seam: `messgr-control provision` gains a
Vault-provisioning step alongside its existing database-provisioning step, so a tenant is fully
ready — control-database row, physical database, **and** Vault identity — after one command.
Per DESIGN.md §7.6 / §11.4.

**A bug found in refinement, fixed here rather than inherited.** `provision.rs` (T-001) writes
`tenant.vault_mount = "transit/{slug}/messgr-dek"` — the full mount+key string DESIGN.md §7.6
used to describe the naming *convention*. But `keystore.rs`'s `KeyStore` trait (T-003) treats its
`mount: &str` parameter as the bare engine mount path and appends the fixed `KEY_NAME` constant
internally — T-003's own decision record: "mount varies per tenant, key name never does." Passed
straight through, the old value would double the key-name path segment the first time any code
actually built a Vault request from the column — caught now only because nothing has wired
`tenant.vault_mount` into a live `KeyStore` call before this ticket. **Fixed at the source:**
`DESIGN.md` §7.6 is corrected to state plainly that `tenant.vault_mount` holds the mount path
only (`transit/<slug>`); this ticket changes `provision.rs` to match. See DESIGN.md §7.6's
inline correction note and this ticket's Implementation Plan, decision 1.

**What gets created, and how it's scoped:**
- A per-tenant Transit mount `transit/<slug>` and, inside it, a key named `messgr-dek` (the
  fixed constant `KeyStore` already assumes).
- A Vault ACL policy scoped to exactly the two paths `KeyStore`'s two methods touch under that
  mount (`datakey/plaintext/messgr-dek`, `decrypt/messgr-dek`) — tighter than "the whole mount",
  which would also permit key rotate/export/delete. Still satisfies §7.6's "an AppRole [bound]
  to exactly one mount" claim; a two-path subset of one mount is still exactly one mount.
- One Vault AppRole per tenant (role name = tenant slug) inside a single, shared `approle` auth
  backend — Vault's own multi-tenant convention: isolation comes from each role's own
  `token_policies` binding, not from a separate auth mount per tenant. Enabling the `approle`
  auth method itself is a one-time, cluster-wide bootstrap (like the dev-mode Vault container's
  own existence), added to `just vault-dev-init` / the CI bootstrap step — not per-tenant
  provisioning code.
- A SecretID, minted only when the tenant row is freshly created (never on an idempotent
  re-provision of an already-active tenant), delivered response-wrapped and printed once to the
  operator's terminal — matching §7.6: "SecretID delivered response-wrapped at deploy time."
  Rotating/reissuing a SecretID later is an operational runbook, out of this ticket's scope
  (PLAN.md places "AppRole/SecretID delivery" ops work under T-005).

**Open question (§ "Still open", item 12) — not a blocker.** "Vault edition" asks only to
*confirm* open-source-with-mounts is acceptable versus paying for Enterprise namespaces; §7.6's
licensing note and the Decisions-taken table (row 17: "Uses mounts + policies, not Enterprise
namespaces") already commit to the open-source mechanism this ticket builds. Flagged for
awareness, not gating this plan.

**Constraint carried from T-003 (§7.6):** `KeyStore` must keep `wrapped_dek` opaque. The
deferred "wrapped DEKs in Vault KV" migration only stays available if the column never leaks
into queries or the API — this ticket must not add any path that exposes it. (Unaffected here:
this ticket never touches `wrapped_dek`; it only creates the substrate `customer_dek` will use.)

Soft coupling: T-005 (production Vault topology, 3-node Raft + Shamir unseal, AppRole/SecretID
delivery runbook) is the operational hardening this ticket's dev/single-node story defers to; no
hard dependency. T-009 (DEK lifecycle) is the first real consumer of the mount this ticket
creates, reading `tenant.vault_mount`/`tenant.vault_role_id` to authenticate as the tenant
(AppRole login) — out of scope here; this ticket only creates the identity, it does not wire
any runtime process to authenticate as it.

## Implementation Plan

> **Live-Vault caveat.** T-003's refinement compiled and ran its code against a live dev-mode
> Vault container. Docker is not reachable in this refinement session, so every Vault call below
> was instead verified by reading `vaultrs` 0.8.0's own source directly (function signatures,
> request/response shapes, idempotency semantics, response-wrapping machinery —
> `~/.local/share/cargo/registry/src/.../vaultrs-0.8.0/src/{sys,transit,auth/approle,client,api}.rs`).
> The acceptance test re-runs everything against a live dev-mode Vault at implementation time,
> same as every prior ticket — that is where this gets its live confirmation, not here.

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-004-per-tenant-transit-mount-approle-creation-wired-into-the-provisioning-command
```

All work on this branch, in this repo (`project: messgr`, `path = "."`, `layout = "in-tree"`).
Commit locally as you go. Do not push or open a merge request without explicit user approval
(this project's commit policy). Before pushing, verify the remote base is not behind:
`git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` must print
nothing.

### Prerequisite gate (hard)

`T-003` is in `6-done/` with a `MERGED` History line (`main`) — confirmed. Working tree must be
clean before starting. No other precondition.

### Confirmed design decisions (do not deviate without asking)

1. **`tenant.vault_mount` stores the mount path only — `transit/{slug}` — never the key-name
   suffix.** Fixes the bug found in refinement (Description, above); `DESIGN.md` §7.6 is
   corrected in the same spirit. `provision.rs`'s `let vault_mount = format!("transit/{slug}/messgr-dek");`
   becomes `let vault_mount = format!("transit/{slug}");`.
2. **New control-DB column `tenant.vault_role_id text UNIQUE` (nullable). No `vault_secret_id`
   column, ever.** A RoleID is not a secret (Vault's own docs: it behaves like a username) and is
   safe to persist and query. The SecretID is minted response-wrapped at provisioning time,
   printed once to the operator's terminal, and never written to Postgres — not `tenant`, not
   `platform_audit`. Persisting a wrapped SecretID would defeat the reason it is wrapped.
3. **One shared `approle` auth backend; one Vault role per tenant, `role_name = <slug>`.**
   Isolation comes from each role's own `token_policies` binding, not from a second per-tenant
   Vault resource. Enabling the `approle` auth method is a one-time, cluster-wide bootstrap step
   (`just vault-dev-init` / the CI bootstrap step, T-003's established pattern for global
   setup) — provisioning code assumes it is already enabled and does not itself enable it.
4. **Per-tenant policy scoped to exactly the two paths `KeyStore`'s two methods touch, not the
   whole mount.** `transit/<slug>/datakey/plaintext/messgr-dek` and
   `transit/<slug>/decrypt/messgr-dek`, both `["create", "update"]` (Transit's POST-only
   endpoints need both capabilities under Vault's ACL model for endpoints with no separately
   readable resource). Tighter than §7.6's literal "one mount" phrasing, which is a strictly
   stronger, still-compliant position, not a deviation from it.
5. **A SecretID is minted only when the tenant row is freshly created** (the `provision.rs`
   `None =>` / `"created"` branch), never on an idempotent re-provision of an already-active
   tenant. Re-running `provision_tenant` on an existing tenant must not mint a fresh live
   credential on every operator debugging session. Rotation/reissue for an existing tenant is out
   of scope (an ops runbook — PLAN.md places "AppRole/SecretID delivery" under T-005).
6. **The admin Vault client provisioning uses is the same guarded, env-based client
   `VaultKeyStore` already builds** (`VAULT_ADDR`/`VAULT_TOKEN`, same non-dev TLS guard) —
   extracted into a shared `pub(crate) fn connect_client(profile: Profile) -> Result<VaultClient,
   KeyStoreError>` in `keystore.rs`, used by both `VaultKeyStore::connect` and the new admin
   path in `provision.rs`. In dev this is the same root token already used everywhere; in
   production, whoever runs `messgr-control provision` supplies an admin-capable `VAULT_TOKEN`,
   distinct from any tenant's own AppRole-derived token (constructing *that* — logging in as a
   tenant's own AppRole to use `KeyStore` at runtime — is T-009's job, not this one).
7. **`KEY_NAME` moves from private to `pub(crate)` in `keystore.rs` and is imported by the new
   module, never duplicated.** Two modules independently defining the same string is exactly the
   drift hazard `KeyStore`'s narrow-seam design (T-003) exists to avoid.
8. **New module `src/tenant/vault.rs` holds every mount/key/policy/role provisioning call.**
   Kept out of `keystore.rs` (stays the narrow two-method runtime trait, per T-003's own stated
   scope) and out of `provision.rs` (which orchestrates: one call in, one outcome out).
9. **Mount creation is idempotency-checked by listing existing mounts first** (`sys::mount::list`
   returns `HashMap<String, MountResponse>` keyed by path with Vault's own trailing slash
   appended), not by pattern-matching Vault's error text — more robust across Vault versions than
   sniffing an English error string for "already in use". Transit key creation is *not*
   pre-checked: Vault's documented behaviour is that creating a key that already exists (same
   type) is a no-op, not an error, so `transit::key::create` is called unconditionally.
10. **`provision_tenant`'s signature changes:** it now takes an extra `vault_client: &VaultClient`
    parameter; its return type changes from `Result<Uuid, sqlx::Error>` to
    `Result<ProvisionOutcome, ProvisionError>`, where `ProvisionOutcome { tenant_id, vault_role_id,
    vault_wrapped_secret_id: Option<String> }` and `ProvisionError` wraps both `sqlx::Error` and
    `keystore::KeyStoreError` (a provisioning run can now fail on either side). Every existing
    call site (`src/bin/control.rs`, all ten call sites across `tests/tenancy.rs`) is updated in
    this ticket — per PLAN.md's own rule that a ticket touching tenant-scoped data extends the
    two-tenant suite rather than assuming it, this is in scope, not a follow-up.

### Tasks

#### Task 1 — Migration, `migrations/control/0002_tenant_vault_role_id.sql` (new file)

```sql
-- Adds the public Vault AppRole RoleID issued per tenant (DESIGN.md §7.6, §11.4).
-- RoleID is not a secret (Vault's own docs describe it as behaving like a
-- username) and is safe to persist and query. The corresponding SecretID is
-- deliberately NOT a column here: it is generated response-wrapped at
-- provisioning time and printed once to the operator's terminal for
-- out-of-band delivery to the tenant's dispatcher deployment (§7.6:
-- "SecretID delivered response-wrapped at deploy time"). Persisting it in
-- Postgres would defeat the point of wrapping it.
ALTER TABLE tenant ADD COLUMN vault_role_id text UNIQUE;
```

#### Task 2 — `src/keystore.rs`: shared client constructor, `pub(crate) KEY_NAME`, admin accessor

Extract the env-reading + guard logic `VaultKeyStore::connect` already has into a reusable free
function, so the new admin path in `provision.rs` builds its Vault client the same way instead of
a second time:

```rust
// KEY_NAME: change `const KEY_NAME: &str = "messgr-dek";` to:
pub(crate) const KEY_NAME: &str = "messgr-dek";
```

```rust
impl VaultKeyStore {
    /// Connects using `VAULT_ADDR`/`VAULT_TOKEN` from the environment
    /// (`vaultrs`'s own default — see `VaultClientSettingsBuilder`).
    pub fn connect(profile: Profile) -> Result<Self, KeyStoreError> {
        Ok(Self {
            client: connect_client(profile)?,
        })
    }

    /// The underlying Vault client, for callers that need admin-level
    /// operations `KeyStore`'s own two methods don't cover (T-004's
    /// mount/key/policy/role provisioning). Same guarded, env-based client
    /// either way — see `connect_client`.
    pub fn client(&self) -> &VaultClient {
        &self.client
    }
}

/// Builds a Vault client from `VAULT_ADDR`/`VAULT_TOKEN` (`vaultrs`'s own
/// env defaults), applying the non-dev TLS guard. Shared by `VaultKeyStore::connect`
/// (data-plane, tenant-scoped calls) and `tenant::vault`'s admin provisioning
/// path (T-004) — one way this codebase turns environment variables into a
/// Vault client, not two.
pub(crate) fn connect_client(profile: Profile) -> Result<VaultClient, KeyStoreError> {
    dotenvy::dotenv().ok();

    let settings: VaultClientSettings = VaultClientSettingsBuilder::default()
        .build()
        .unwrap_or_else(|err| panic!("failed to build Vault client settings: {err}"));

    assert_tls_outside_dev(&settings.address, profile);

    Ok(VaultClient::new(settings)?)
}
```

No other change to `keystore.rs` — `create_dek`/`unwrap_dek`, the guard function, and its three
unit tests are untouched.

#### Task 3 — `src/tenant/vault.rs` (new file): mount, key, policy, role, SecretID provisioning

```rust
//! Per-tenant Vault Transit mount, key, ACL policy, and AppRole provisioning
//! (DESIGN.md §7.6, §11.4). Fills the seam `keystore::KeyStore` (T-003)
//! deliberately left open: creating the substrate `KeyStore` talks to.
//! Assumes the `approle` auth method is already enabled cluster-wide (a
//! one-time Vault bootstrap — `just vault-dev-init` / the CI bootstrap step
//! enable it once; see decision 3). Never authenticates *as* a tenant —
//! that is T-009's job.

use std::collections::HashMap;

use vaultrs::api::auth::approle::requests::GenerateNewSecretIDRequest;
use vaultrs::api::ResponseWrapper;
use vaultrs::auth::approle::role as approle_role;
use vaultrs::client::VaultClient;
use vaultrs::error::ClientError;
use vaultrs::sys::{mount, policy};
use vaultrs::transit::key as transit_key;

use crate::keystore::{KeyStoreError, KEY_NAME};

const APPROLE_MOUNT: &str = "approle";

/// The result of provisioning (or re-confirming) one tenant's Vault identity.
pub struct VaultProvisionOutcome {
    /// Public AppRole identifier — safe to persist (`tenant.vault_role_id`).
    pub role_id: String,
    /// A response-wrapped SecretID token (10-minute default TTL, single-use
    /// unwrap), present only when `mint_secret_id` was true. Print once for
    /// out-of-band delivery; never persist.
    pub wrapped_secret_id: Option<String>,
}

/// Creates (or idempotently re-confirms) `tenant_slug`'s Transit mount, key,
/// ACL policy, and AppRole. Mints a new response-wrapped SecretID only when
/// `mint_secret_id` is true (decision 5) — pass `false` on an idempotent
/// re-provision of an already-active tenant.
pub async fn provision_vault(
    client: &VaultClient,
    tenant_slug: &str,
    mint_secret_id: bool,
) -> Result<VaultProvisionOutcome, KeyStoreError> {
    let mount_path = format!("transit/{tenant_slug}");

    ensure_transit_mount(client, &mount_path).await?;
    transit_key::create(client, &mount_path, KEY_NAME, None).await?; // idempotent: same-type re-create is a no-op (Vault docs)

    let policy_name = format!("tenant-{tenant_slug}-transit");
    let policy_hcl = format!(
        r#"
path "{mount_path}/datakey/plaintext/{KEY_NAME}" {{
  capabilities = ["create", "update"]
}}
path "{mount_path}/decrypt/{KEY_NAME}" {{
  capabilities = ["create", "update"]
}}
"#
    );
    policy::set(client, &policy_name, &policy_hcl).await?; // upsert, no idempotency concern

    approle_role::set(
        client,
        APPROLE_MOUNT,
        tenant_slug,
        Some(&mut vaultrs::api::auth::approle::requests::SetAppRoleRequest::builder()
            .token_policies(vec![policy_name])
            .to_owned()),
    )
    .await?; // upsert, no idempotency concern

    let role_id = approle_role::read_id(client, APPROLE_MOUNT, tenant_slug)
        .await?
        .role_id;

    let wrapped_secret_id = if mint_secret_id {
        let endpoint = GenerateNewSecretIDRequest::builder()
            .mount(APPROLE_MOUNT)
            .role_name(tenant_slug)
            .build()
            .expect("static builder inputs always build");
        let wrapped = endpoint.wrap(client).await?;
        Some(wrapped.info.token)
    } else {
        None
    };

    Ok(VaultProvisionOutcome {
        role_id,
        wrapped_secret_id,
    })
}

async fn ensure_transit_mount(client: &VaultClient, mount_path: &str) -> Result<(), ClientError> {
    let existing: HashMap<String, vaultrs::api::sys::responses::MountResponse> =
        mount::list(client).await?;
    let normalized = format!("{mount_path}/"); // Vault always returns paths with a trailing slash
    if !existing.contains_key(&normalized) {
        mount::enable(client, mount_path, "transit", None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_hcl_names_exactly_the_two_keystore_paths() {
        // Pure-function regression guard for decision 4 (least privilege): if
        // this ever grows a third path or a broader capability, this test
        // should be the thing that has to change, not a live-Vault surprise.
        let mount_path = "transit/acme";
        let policy_hcl = format!(
            r#"
path "{mount_path}/datakey/plaintext/{KEY_NAME}" {{
  capabilities = ["create", "update"]
}}
path "{mount_path}/decrypt/{KEY_NAME}" {{
  capabilities = ["create", "update"]
}}
"#
        );
        assert!(policy_hcl.contains("transit/acme/datakey/plaintext/messgr-dek"));
        assert!(policy_hcl.contains("transit/acme/decrypt/messgr-dek"));
        assert!(!policy_hcl.contains("rotate"));
        assert!(!policy_hcl.contains("export"));
        assert!(!policy_hcl.contains("delete"));
    }
}
```

Register it: add `pub mod vault;` to `src/tenant/mod.rs`.

**Note for the implementer:** verify `SetAppRoleRequestBuilder`'s exact setter name/shape
(`.token_policies(vec![...])`) and the builder-into-`Option<&mut _>` calling convention against
the actual `vaultrs` 0.8.0 docs/source at implementation time — `approle_role::set`'s `opts`
parameter type was confirmed from source (`Option<&mut SetAppRoleRequestBuilder>`) but the exact
ergonomics of constructing that `&mut _` inline were not compiled in this refinement (no live
build was run — see the caveat above). If the inline builder expression doesn't compile as
written, bind it to a local `let mut opts = SetAppRoleRequest::builder(); opts.token_policies(...);`
first — same effect, easier to get past the borrow checker.

#### Task 4 — `src/tenant/model.rs` and `src/tenant/repo.rs`: `vault_role_id`

`model.rs`, add a field to `Tenant`:

```rust
pub vault_role_id: Option<String>,
```

`repo.rs`: extend `find_by_slug`'s `SELECT` column list to include `vault_role_id`, and add:

```rust
pub async fn record_vault_role_id(
    pool: &PgPool,
    tenant_id: Uuid,
    vault_role_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tenant SET vault_role_id = $1 WHERE id = $2")
        .bind(vault_role_id)
        .bind(tenant_id)
        .execute(pool)
        .await
        .map(|_| ())
}
```

#### Task 5 — `src/tenant/provision.rs`: wire Vault provisioning into `provision_tenant`

New error/outcome types:

```rust
#[derive(Debug)]
pub struct ProvisionOutcome {
    pub tenant_id: Uuid,
    pub vault_role_id: String,
    /// Present only on a fresh provision (decision 5). Print once; never persist.
    pub vault_wrapped_secret_id: Option<String>,
}

#[derive(Debug)]
pub enum ProvisionError {
    Database(sqlx::Error),
    Vault(crate::keystore::KeyStoreError),
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "provisioning failed (database): {err}"),
            Self::Vault(err) => write!(f, "provisioning failed (vault): {err}"),
        }
    }
}

impl std::error::Error for ProvisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ProvisionError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<crate::keystore::KeyStoreError> for ProvisionError {
    fn from(err: crate::keystore::KeyStoreError) -> Self {
        Self::Vault(err)
    }
}
```

`provision_tenant`'s signature:

```rust
pub async fn provision_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    slug: &str,
    region: &str,
    database_name: &str,
    profile: Profile,
    actor: &str,
    vault_client: &vaultrs::client::VaultClient,
) -> Result<ProvisionOutcome, ProvisionError> {
```

Body changes:
- The existing `match repo::find_by_slug(...)` arms stay as-is for the `Some(tenant) if ... =>`
  (idempotent) and rejected branches, except: the rejected branch's early `return Err(...)` now
  wraps its `sqlx::Error::Configuration(...)` in `ProvisionError::Database(...)` (via `?`/`.into()`
  — the `From` impl above makes `?` work unchanged if the branch still returns a `sqlx::Error`
  internally and converts at the return).
- Track whether this run should mint a SecretID: `let mint_secret_id = matches!(outcome, "created");`
  right after the existing three-way match that sets `(tenant_id, outcome)` — keep `vault_mount`
  in `format!("transit/{slug}")` per decision 1.
- Immediately after that match (before `ensure_database_exists`), call:
  ```rust
  let vault_outcome = crate::tenant::vault::provision_vault(vault_client, slug, mint_secret_id).await?;
  repo::record_vault_role_id(control_pool, tenant_id, &vault_outcome.role_id).await?;
  ```
  (Placed before database provisioning so a retry after a Vault-side failure finds the tenant row
  and its `vault_mount` already in place — idempotent re-entry, same spirit as the existing
  `find_by_slug` check.)
- `platform_audit::record`'s `detail` JSON for the `"tenant.provision"` action gains
  `"vault_role_id": vault_outcome.role_id` — **never** the wrapped secret id (decision 2).
- Final `Ok(...)` becomes:
  ```rust
  Ok(ProvisionOutcome {
      tenant_id,
      vault_role_id: vault_outcome.role_id,
      vault_wrapped_secret_id: vault_outcome.wrapped_secret_id,
  })
  ```

#### Task 6 — `src/bin/control.rs`: construct the admin Vault client, print the credential once

```rust
use messgr::keystore::VaultKeyStore;
// ... existing imports ...

// in main(), after `config` is loaded and before dispatching `cli.command`:
let vault_keystore = VaultKeyStore::connect(config.profile)
    .expect("failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)");
```

`Command::Provision` arm:

```rust
Command::Provision { slug, region, database_name, actor } => {
    let outcome = provision_tenant(
        &control_pool,
        &config.control_database_url,
        &slug,
        &region,
        &database_name,
        config.profile,
        &actor,
        vault_keystore.client(),
    )
    .await
    .unwrap_or_else(|err| {
        panic!(
            "failed to provision tenant {slug:?} (has `messgr-control migrate` been run \
             against the control database, and is Vault reachable and unsealed?): {err}"
        )
    });

    println!("{}", outcome.tenant_id);
    println!("vault_role_id={}", outcome.vault_role_id);
    if let Some(token) = outcome.vault_wrapped_secret_id {
        println!(
            "vault_wrapped_secret_id={token}  # single-use, 10m TTL \u2014 unwrap once at the \
             tenant's dispatcher deployment (`vault unwrap`); do not store this line anywhere"
        );
    }
}
```

#### Task 7 — `justfile` and CI: enable the `approle` auth backend once (decision 3)

Add one more idempotent bootstrap line to `vault-dev-init` in the `justfile`:

```
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"approle"}' http://localhost:8200/v1/sys/auth/approle || true
```

And the matching step in `.github/workflows/ci.yml`'s Vault bootstrap (alongside the existing
`transit`/`transit-other` curl calls):

```yaml
          curl -sf --header "X-Vault-Token: $VAULT_TOKEN" --request POST \
            --data '{"type":"approle"}' "$VAULT_ADDR/v1/sys/auth/approle"
```

#### Task 8 — `tests/tenancy.rs`: update every `provision_tenant` call site

Add a test helper next to `control_database_url()`:

```rust
fn vault_client() -> vaultrs::client::VaultClient {
    messgr::keystore::VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed")
        .client_owned() // see note below
}
```

**Note:** `VaultKeyStore::client()` (Task 2) returns `&VaultClient` borrowed from a temporary
here, which will not compile as a one-liner. Either (a) change the test helper to return
`VaultKeyStore` itself and call `.client()` at each use site, or (b) derive `Clone` for
`VaultClient`'s constituent fields is not available upstream, so prefer (a) —
`fn vault_keystore() -> VaultKeyStore` returning the connected store, then pass
`vault_keystore().client()` inline, or construct one `let vault = vault_keystore();` per test
and pass `vault.client()` to every `provision_tenant(...)` call in that test. Use whichever reads
cleaner per test; do not add a `Clone` impl to `VaultClient` upstream-side to work around it.

Every one of the ten `provision_tenant(...)` call sites across the six tests
(`two_tenants_are_isolated_by_database`,
`work_on_tenant_as_pool_never_reads_or_writes_tenant_bs_database`,
`a_mis_wired_pool_trips_the_current_database_assertion`, `provisioning_writes_a_platform_audit_row`,
`idempotent_reprovision_writes_a_second_platform_audit_row`,
`rejected_reprovision_writes_a_platform_audit_row`) needs two mechanical changes:
1. Add `vault.client()` (or equivalent, per the note above) as the final argument.
2. Where the return value is bound (`let tenant_a = ...`, `let tenant_id = ...`), the type is now
   `ProvisionOutcome`, not `Uuid` — every later use of that binding as a bare id becomes
   `tenant_a.tenant_id` / `tenant_id.tenant_id` (e.g. `assert_ne!(tenant_a.tenant_id,
   tenant_b.tenant_id)`, `.bind(tenant_id.tenant_id)`).

#### Task 9 — `tests/tenant_vault.rs` (new file): prove per-tenant Vault isolation by mutation

The property this ticket exists to establish is that tenant A's Vault credentials cannot decrypt
tenant B's data. Prove it by actually logging in as tenant A's AppRole and attempting a
cross-tenant call — not by asserting the provisioning code merely *ran*, the exact failure shape
T-003/F1's review caught once already:

```rust
//! Proves the Vault-side isolation T-004 introduces: a tenant's own AppRole
//! credentials can operate on its own Transit mount and are rejected — by
//! Vault's ACL, not merely by Transit's own cross-mount key mismatch
//! (already covered by tests/keystore.rs) — on any other tenant's mount.

use uuid::Uuid;
use vaultrs::client::{Client as _, VaultClient, VaultClientSettingsBuilder};
use vaultrs::transit::generate;
use vaultrs::api::transit::requests::DataKeyType;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::provision::provision_tenant;

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL").expect("CONTROL_DATABASE_URL must be set for tests")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

/// Logs in as `role_id`/`secret_id` and returns a Vault client scoped to
/// whatever policies that AppRole login grants — not the admin client.
async fn login_as_tenant(
    admin: &VaultClient,
    role_id: &str,
    secret_id: &str,
) -> VaultClient {
    let auth = vaultrs::auth::approle::login(admin, "approle", role_id, secret_id)
        .await
        .expect("AppRole login failed");
    let mut settings = admin.settings().clone();
    settings.token = auth.client_token;
    VaultClient::new(settings).expect("building scoped client failed")
}

#[tokio::test]
async fn tenant_a_vault_credentials_cannot_read_tenant_bs_dek() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug_a = unique_name("test_tenant_vault_a");
    let db_a = unique_name("test_db_vault_a");
    let slug_b = unique_name("test_tenant_vault_b");
    let db_b = unique_name("test_db_vault_b");

    let outcome_a = provision_tenant(
        &control_pool, &control_url, &slug_a, "eu", &db_a, Profile::Dev, "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant A failed");
    let outcome_b = provision_tenant(
        &control_pool, &control_url, &slug_b, "eu", &db_b, Profile::Dev, "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning tenant B failed");

    let wrapped_a = outcome_a
        .vault_wrapped_secret_id
        .expect("a fresh provision must mint a SecretID");
    let secret_id_a: vaultrs::api::auth::approle::responses::GenerateNewSecretIDResponse =
        vaultrs::sys::wrapping::unwrap(admin.client(), Some(&wrapped_a))
            .await
            .expect("unwrapping tenant A's SecretID failed");

    let scoped_a = login_as_tenant(admin.client(), &outcome_a.vault_role_id, &secret_id_a.secret_id).await;

    // Tenant A, on its own mount: must succeed.
    let own_mount = format!("transit/{slug_a}");
    generate::data_key(&scoped_a, &own_mount, "messgr-dek", DataKeyType::Plaintext, None)
        .await
        .expect("tenant A must be able to create a DEK on its own mount");

    // Tenant A's credentials, on tenant B's mount: must be rejected by Vault's
    // ACL (permission denied), not merely fail for some other reason.
    let other_mount = format!("transit/{slug_b}");
    let result = generate::data_key(&scoped_a, &other_mount, "messgr-dek", DataKeyType::Plaintext, None).await;
    assert!(
        result.is_err(),
        "tenant A's Vault credentials must not be able to operate on tenant B's mount"
    );

    // Same mutation standard as T-003/F1: prove it's really the tenant-scoped
    // policy doing the rejecting, not an accident of the fixture, by
    // confirming the *admin* client (unscoped) can reach the same path fine.
    generate::data_key(admin.client(), &other_mount, "messgr-dek", DataKeyType::Plaintext, None)
        .await
        .expect("the admin client must still be able to reach tenant B's mount directly");
}

#[tokio::test]
async fn idempotent_reprovision_does_not_mint_a_second_secret_id() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_vault_idempotent");
    let db_name = unique_name("test_db_vault_idempotent");

    let first = provision_tenant(
        &control_pool, &control_url, &slug, "eu", &db_name, Profile::Dev, "test-actor",
        admin.client(),
    )
    .await
    .expect("first provisioning failed");
    assert!(first.vault_wrapped_secret_id.is_some(), "a fresh provision must mint a SecretID");

    let second = provision_tenant(
        &control_pool, &control_url, &slug, "eu", &db_name, Profile::Dev, "test-actor",
        admin.client(),
    )
    .await
    .expect("idempotent re-provisioning failed");
    assert!(
        second.vault_wrapped_secret_id.is_none(),
        "an idempotent re-provision must not mint a second SecretID (decision 5)"
    );
    assert_eq!(
        first.vault_role_id, second.vault_role_id,
        "re-provisioning the same tenant must return the same RoleID"
    );
}
```

**Note for the implementer:** `GenerateNewSecretIDResponse`'s exact field name for the unwrapped
secret (`secret_id` above) should be confirmed against `vaultrs::api::auth::approle::responses`
at implementation time — read from source during this refinement but not compiled. Cleanup
(`drop_test_tenant`-equivalent) is omitted from this sketch for brevity; add the same
best-effort teardown `tests/tenancy.rs` uses (Postgres rows + `DROP DATABASE`), plus a Vault-side
teardown (`mount::disable`, `policy::delete`, `approle_role::delete`) so repeated CI runs don't
accumulate tenant mounts in the shared dev-mode Vault container.

### Acceptance test

```
docker compose up -d
just vault-dev-init
cargo run --bin messgr-control -- migrate
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build
cargo test
```

All green, including specifically:

- `tenant::vault::tests::policy_hcl_names_exactly_the_two_keystore_paths`
- every existing `tests/keystore.rs` test unaffected (T-003's generic `transit`/`transit-other`
  fixture mounts are untouched by this ticket's tenant-scoped `transit/<slug>` mounts)
- every existing `tests/tenancy.rs` test, updated per Task 8, still green
- `tenant_vault::tenant_a_vault_credentials_cannot_read_tenant_bs_dek` — the isolation proof
- `tenant_vault::idempotent_reprovision_does_not_mint_a_second_secret_id`

Manual smoke check: run `messgr-control provision` twice for the same `--slug` and confirm the
second run's output has no `vault_wrapped_secret_id` line while the first run's does, and that
`SELECT vault_role_id FROM tenant WHERE slug = '<slug>'` returns the same value both times.

### Docs update (mandatory when user-facing)

`README.md`'s Local development section: document the new `approle` auth-method bootstrap line
in `just vault-dev-init`, and add a short paragraph on `messgr-control provision`'s new output
(RoleID persisted; SecretID printed once, response-wrapped, and must be captured immediately —
it cannot be retrieved again). `DESIGN.md` §7.6 is already corrected as part of this refinement
(see the Description) — no further design-doc change needed at implementation time unless the
implementer deviates from a confirmed decision above, in which case update both the code and
the doc together.

### Finish (mandatory)

1. Acceptance test green; `cargo fmt --check` / `cargo clippy --all-targets --all-features -D
   warnings` / `cargo build` clean.
2. `README.md` updated per the Docs update step above.
3. Write a summary: files touched, decisions made (especially any place the implementer had to
   deviate from this plan's unverified-against-live-Vault code, per the caveat above), anything
   deferred.
4. Suggest a Conventional Commit message, ticket id in brackets, e.g.:
   ```
   feat(vault): provision per-tenant Transit mount, policy, and AppRole (T-004)
   ```
5. This is a root-path child (`path = "."`) — interactive-rebase WIP commits into a small
   number of atomic, correctly typed/scoped commits before presenting them.
6. Commit locally on
   `feat/T-004-per-tenant-transit-mount-approle-creation-wired-into-the-provisioning-command`.
   Do not push or open a merge request without user approval. Present the commit message; only
   after approval, verify the remote base is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` must print nothing), then push and open
   the merge request. Merging is always the human's. Hand back to the user.

## Review

- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a, if the project ships docs)
- [x] Docs-readability pass on the ticket's changed `.adoc`/`.md` files, or a conscious skip recorded (step 4b, optional) — skipped: no docs-readability reviewer configured in this session
- [x] Findings recorded in the ticket's `## Review` with severity, **class**, **and** disposition per the rules §5; disposition summary line present, and a `cost: estimated …, actual …` line beneath it (step 5)
- [x] Remaining-tickets impact sweep done (step 8)

**Diffed** `main..feat/T-004-per-tenant-transit-mount-approle-creation-wired-into-the-provisioning-command`
(commits `c0c9769`, `d540f05`, `c91fa31`, `214f5f3`, `31179ca`, `70c723b`). Read the ticket from
`main` throughout (in-tree layout — the feature branch's own copy of `tickets/` is stale, cut
before the ticket moved to `4-in-review/`).

**Implementation audit.** Prerequisite gate (T-003 done, merged `fef2dac`) confirmed. All 10
confirmed design decisions honoured in the shipped code:

| # | Decision | Verdict | Evidence |
|---|---|---|---|
| 1 | `tenant.vault_mount` = mount path only | Met | `provision.rs:129`; live: provisioned `acme-review`, `vault_mount` = `transit/acme-review`, not doubled with `/messgr-dek` |
| 2 | New `vault_role_id` column; SecretID never in Postgres | Met | `migrations/control/0002_tenant_vault_role_id.sql`; grepped for secret-id persistence — none; live-checked `platform_audit.detail` across two provisioning runs, only `vault_role_id` present |
| 3 | Shared `approle` backend, one role per tenant, enabled once in bootstrap | Met | `src/tenant/vault.rs` never enables an auth backend; `justfile`/CI do it once, idempotently |
| 4 | Policy scoped to exactly the two `KeyStore` paths | Met | `policy_hcl_for` (`src/tenant/vault.rs:93-99`); unit test green; **mutation-verified live** — wildcarding the policy makes the isolation test fail, reverting makes it pass |
| 5 | SecretID minted only on fresh provision | Met | `provision.rs:149`; live-verified twice, plus `idempotent_reprovision_does_not_mint_a_second_secret_id` |
| 6 | Shared `connect_client`, admin path reuses it | Met (wording nit, F1) | `keystore.rs:96-107`; admin path goes through `VaultKeyStore::connect` in `bin/control.rs`, not `provision.rs` as the plan's own prose implied — same effective invariant, ticket's own Task 6 code sample already placed it there |
| 7 | `KEY_NAME` `pub(crate)`, shared not duplicated | Met | `keystore.rs:22`, `tenant/vault.rs:23` |
| 8 | New `src/tenant/vault.rs` module | Met | present, registered in `tenant/mod.rs` |
| 9 | Mount idempotency via `mount::list`, not error-text | Met | `ensure_transit_mount` (`tenant/vault.rs:101-111`) |
| 10 | `provision_tenant` signature/return type change, all call sites updated | Met | `provision.rs:85-95`; all 10 call sites across `tests/tenancy.rs` updated (counted: 2+2+1+1+2+2=10) |

All 9 tasks present and correct in the files they name, cross-checked against actual file
content, not the plan's prose.

**Acceptance test re-run, live** (fresh Postgres 18-alpine + dev-mode Vault via Docker/OrbStack,
port 5432 remapped locally to 55432 for the run, `compose.yml`/`.env` reverted after — same
workaround T-003's review used):

```
docker compose up -d                                        # both healthy
just vault-dev-init                                         # transit-fixture, transit-fixture-other, approle bootstrapped
cargo run --bin messgr-control -- migrate                   # clean
cargo fmt --check                                           # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo build                                                  # clean
cargo test                                                   # 21 passed, 0 failed
```

9 lib unit + 3 `tests/keystore.rs` + 6 `tests/tenancy.rs` + 3 `tests/tenant_vault.rs`, all green.
Manual smoke check (ticket's own acceptance criterion): ran `messgr-control provision` twice for
the same slug — first run printed `vault_wrapped_secret_id=...`, second did not; `vault_role_id`
identical both times.

**Both bugs the ticket's History records as found live during implementation, independently
re-verified:** (1) the `transit-fixture`/`transit-fixture-other` rename — grepped the whole tree,
no stray `transit`/`transit-other` mount references remain in `tests/keystore.rs`, `justfile`, or
CI; (2) the lazy Vault-client construction — ran `messgr-control migrate` with `VAULT_ADDR`/
`VAULT_TOKEN` unset entirely; succeeded, confirming `Migrate` has zero Vault dependency.

**Isolation-proof test, mutation-tested** (same standard T-001/F13 and T-003/F1 established for
this codebase — verify by breaking the property, not by reading the assertion):
`tests/tenant_vault.rs::tenant_a_vault_credentials_cannot_read_tenant_bs_dek` was put through a
real mutation: `policy_hcl_for` was temporarily wildcarded to grant every tenant's policy access
across all tenant mounts — reintroducing the exact cross-tenant bug this ticket exists to
prevent. The test **failed** as expected. Reverted; test and the pure-function policy-HCL unit
test both green again. **The test survives mutation — it genuinely catches a regression of the
property it claims to test.** Live-captured the actual rejection: `APIError { code: 403, errors:
["...permission denied..."] }` — a real Vault ACL denial, not an accident of the fixture (the
test's own admin-client-succeeds-on-the-same-path check already rules out "route not found" as
the cause, the exact false-pass mode T-003/F1 was blocking on).

**Quality/consistency/docs audits.** No SecretID leakage anywhere — grepped code, logs, and
`platform_audit`'s JSON detail; only `vault_role_id` (public) is ever persisted or logged.
`README.md`'s new prose checked against live CLI output. `DESIGN.md` §7.6's correction reads
correctly and doesn't contradict anything else in the document.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | stale-xref | noted | No `plan amended inline` History line for two real, good deviations from the Implementation Plan's own code samples: (a) Task 6's snippet builds the admin `VaultKeyStore` unconditionally in `main()`; shipped code builds it lazily inside the `Provision` arm only (the fix for the lazy-Vault-client bug, explained in commit `d540f05`'s body but not mirrored into ticket History); (b) Task 7's snippet references "the existing `transit`/`transit-other` curl calls", but the shipped fix renamed both to `transit-fixture`/`transit-fixture-other` (commit `31179ca`, found live). | Ticket `## History` (no amendment lines); commits `d540f05`, `31179ca`. | Add two `plan amended inline` lines summarizing these two live-found deviations. |
| F2 | non-blocking | test-gap | noted | The isolation test's cross-mount assertion is `result.is_err()`, not an inspection of error content — unlike this same codebase's own established fix for the identical failure shape (`tests/keystore.rs`, T-003/F1), which asserts the error names the specific mechanism. Confirmed by mutation the test does catch a real regression (survives), and the admin-success check already rules out the "broken fixture" false-pass mode — so this is a narrower gap than T-003/F1's, not the same defect, but the local precedent isn't applied consistently. | `tests/tenant_vault.rs:178-187` vs. `tests/keystore.rs:94-100`. Live error: `APIError { code: 403, errors: ["...permission denied..."] }`. | Tighten to assert the error `Debug` output contains `"permission denied"` (or `"403"`), matching `tests/keystore.rs`'s pattern. |
| F3 | non-blocking | docs-gap | noted | `DESIGN.md` §4.11's canonical `CREATE TABLE tenant` block doesn't list `vault_role_id`, unlike `vault_mount`'s inline comment on the same block — §4.11 is the schema-of-record AGENTS.md points readers at for any schema change. | `DESIGN.md:586-596`; `migrations/control/0002_tenant_vault_role_id.sql`. | Add `vault_role_id text UNIQUE, -- public AppRole RoleID (§7.6)` to §4.11's table block. |
| F4 | non-blocking | design | noted | `tests/tenancy.rs`'s `drop_test_tenant` cleans up Postgres rows only, not the Vault mount/policy/role each of its 10 `provision_tenant` calls now creates — unlike `tests/tenant_vault.rs`'s parallel helper, which does. Harmless in CI (ephemeral Vault per run); a long-lived local dev-mode Vault accumulates fixtures across `tenancy.rs` runs. | `tests/tenancy.rs:39-77` vs. `tests/tenant_vault.rs:29-79`. | Fold into a themed follow-up if `tests/tenancy.rs` is touched again; not worth its own ticket at this scale. |
| F5 | non-blocking | docs-gap | noted | README's illustrative `provision` output shows `vault_wrapped_secret_id=eyJhbGciOi...` (looks like a JWT); the real token is Vault's own `hvs.<base64>` format (live: `hvs.CAESIAf...`). Cosmetic only. | `README.md:23` vs. live output. | Fix the placeholder to `hvs.CAESI...` next time this section is touched. |

**Disposition summary:** 5 non-blocking findings, all `noted` (F1–F5). No `fixed inline`,
`folded`, or `new ticket` dispositions — none passed the promotion test on its own.

cost: estimated L, actual L

**Verdict: no blocking findings. Ticket proceeds to `tickets/6-done/`.** Acceptance test fully
green, all ten decisions honoured, both live-found bugs independently re-verified fixed, the
`vault_mount` fix confirmed against a real provisioned row, no SecretID leakage anywhere, and the
isolation-proof test survived a real mutation attempt (reintroducing the exact bug it exists to
catch). The five findings are genuine but small — none breaks the golden path or contradicts a
locked decision.

**Impact sweep (step 8).** No ticket in `tickets/1-to-do/` or `tickets/2-ready/` lists T-004 in
`depends-on:` (the board is otherwise empty of TO DO/READY tickets). Nothing to patch.

## History

- 2026-08-29 — created (TO DO). source: chat: decomposed from PLAN.md's build-order breakdown of DESIGN.md (build step 0, the seam T-003 left open).
- 2026-08-29 — TO DO → READY: plan complete
- 2026-08-29 — READY → IN DEVELOPMENT: picked up
- 2026-08-29 — plan amended inline: Task 6's admin `VaultKeyStore` is constructed lazily inside the `Provision` arm only, not unconditionally in `main()` as the plan's code sample showed — an unconditional connect would have made `messgr-control migrate` depend on Vault too, with no functional need, and raced CI's Vault-readiness retry loop (which runs after the migrate step). Found and fixed during implementation, before any test ran.
- 2026-08-29 — plan amended inline: Task 7's dev/CI bootstrap renames the generic Transit fixture mounts from `transit`/`transit-other` (T-003) to `transit-fixture`/`transit-fixture-other`. Found live during acceptance testing: provisioning failed with "path is already in use at transit/" because Vault refuses to nest a new mount under an already-mounted path, and T-003's dev fixture already mounts a bare `transit` engine — every real `transit/<slug>` per-tenant mount collided with it. Renamed both fixtures (and `tests/keystore.rs`'s references) to names that don't start with `transit/`.
- 2026-08-29 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-08-29 — review: 0 blocking, 5 non-blocking (F1–F5) all noted
- 2026-08-29 — merged to main (PR #4, 8f8233d)
- 2026-08-29 — IN REVIEW → DONE: review clean; 5 non-blocking findings noted
