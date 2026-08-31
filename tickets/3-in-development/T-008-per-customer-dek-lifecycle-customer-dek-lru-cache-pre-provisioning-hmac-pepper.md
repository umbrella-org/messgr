---
id: T-008
title: Per-customer DEK lifecycle: customer_dek, LRU cache, pre-provisioning, HMAC pepper
project: messgr
depends-on: [T-003, T-004]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: XL
---

# T-008 — Per-customer DEK lifecycle: customer_dek, LRU cache, pre-provisioning, HMAC pepper

## Outcome

After this ships, every payload written to the ledger is encrypted under a key unique to its
customer, pulled from a bounded, zeroizing in-memory cache rather than fetched from Vault on
every write, and destination values carry a per-tenant HMAC for lookup. Per-customer DEKs from
the first write is the one design invariant that cannot be retrofitted (design §7, §14).

## Description

Build the per-customer DEK lifecycle: `customer_dek` table, Vault Transit datakey creation,
a bounded zeroizing LRU cache, a pre-provisioning batch path, and a per-tenant HMAC pepper for
destination lookups (design §7.1, §7.6, §4.5). This ticket also finishes the seam T-004
deliberately left open: `T-004` created each tenant's Vault identity (Transit mount, AppRole,
response-wrapped SecretID) but wired nothing to *authenticate* as it — `VaultKeyStore` still
connects with a single environment `VAULT_TOKEN`. This ticket performs the AppRole login
(unwrap the SecretID, log in, use the resulting tenant-scoped token for `create_dek`/
`unwrap_dek`) — do not treat that as already done just because the per-tenant credentials
exist (`PLAN.md` note under build step 0).

Part of the step-2 ticket family (`family: T-007`, see T-007). `wrapped_dek` must stay opaque
so the deferred "wrapped DEKs in Vault KV" migration (§7.6) remains available — never let it
leak into queries or the API.

**Scope note found in refinement.** "DEK pre-provisioning ahead of the customer base" (§7.6)
presumes a customer base to iterate over — the `customer` table (§4.6) doesn't exist until the
customer-projection ticket (step 3, not yet filed). This ticket builds the pre-provisioning
*mechanism* (idempotent, given an explicit list of customer ids) and an operator CLI to drive
it manually; wiring it to a live trigger from the real customer base is deferred to whichever
step-3+ ticket has one. See Implementation Plan decision 6.

**Re-graded L → XL at refinement.** PLAN.md's own sizing note already flagged T-008 as the
likely underestimate on this list. The shipped scope — a new table, a generic bounded/zeroizing
cache with TTL, a lazy get-or-create + a batch pre-provisioning path, a per-tenant pepper with
its own lazy-mint lifecycle, a pure HMAC module, and promoting T-004's test-only AppRole login
into a production `KeyStore` code path — is comparable to T-004's own M→L overrun, at roughly
double the surface.

## Implementation Plan

> **Live-Vault caveat.** Same as T-004's own refinement: no Docker/Vault container was reachable
> in this refinement session. Every Vault call below was verified by reading `vaultrs` 0.8.0's
> own source directly (`~/.local/share/cargo/registry/.../vaultrs-0.8.0/src/{sys,transit,auth/approle}.rs`),
> not compiled or run. The riskiest piece is the AppRole-login path (Task 8): it depends on
> `sys::wrapping::unwrap`'s documented dual-mode behaviour (unwrap using the wrapping token
> itself as the client's own token, passing `None`, rather than an admin token with the wrapped
> value in the request body — the mode T-004's own test used). Confirm this against a live
> dev-mode Vault before trusting it; the acceptance test re-runs everything live, same as every
> prior ticket.

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-008-per-customer-dek-lifecycle
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged, interactive-rebased into
atomic, correctly scoped commits before the summary is presented (rules §0). Do not push and do
not open a merge request without explicit user approval. Ticket and board bookkeeping is
committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-003` and `T-004` are in `6-done/`, merged to `main` (confirmed: `KeyStore`/`VaultKeyStore`
  exist and work; every tenant has a Transit mount, key, ACL policy, and AppRole from
  provisioning).
- Clean working tree before branching.
- Local stack up: `just db-up`, `just control-migrate`, `just vault-dev-init`.

### Confirmed design decisions (do not deviate without asking)

1. **`customer_dek` schema copied verbatim from DESIGN.md §4.5** — `customer_id uuid PRIMARY
   KEY`, `wrapped_dek text NOT NULL`, `created_at timestamptz NOT NULL`, `shredded_at
   timestamptz` (nullable). No `tenant_id` column, matching the `producer`/`tenant_config`
   precedent (T-005/T-007) — the tenant already is the database (§2.1). `shredded_at` is written
   only by the crypto-shredding erasure operation, which is step 15 (not yet filed) — this
   ticket creates the column and never writes it.
2. **The HMAC pepper is a per-tenant secret, minted through Vault once and cached
   in-process — not computed by calling Vault's `transit/hmac` endpoint per lookup.** §7.6 states
   the pepper must be "derived from that tenant's own Transit mount" but doesn't fix the
   mechanism. Calling Vault's native HMAC operation on every destination lookup would put Vault
   back on the hot path for every single message — exactly what §7.6's "Vault must not be on the
   hot path" / cached-DEK / pre-provisioning story exists to avoid, and it would mean a sealed
   Vault blocks *all* ingestion again, not just for uncached customers. Instead: mint the pepper
   as an ordinary Transit datakey (`KeyStore::create_dek`, the exact same primitive as a DEK,
   just not tied to a `customer_id`), persist the wrapped ciphertext on the tenant row
   (`tenant.vault_pepper_wrapped`, Task 3), and hold the unwrapped plaintext in the same bounded
   zeroizing cache type as DEKs. HMAC-SHA256 is then computed locally (Task 7) — Vault is
   contacted once per tenant process lifetime (cache miss), never per message. This requires **no
   change to T-004's Vault ACL policy** — `create_dek`/`unwrap_dek` are the same two paths
   (`datakey/plaintext`, `decrypt`) the policy already grants.
3. **New cache type `KeyCache` (`src/key_cache.rs`, top-level) is generic over `Uuid` keys, not
   customer-specific**, and is reused for both the DEK cache and the tenant-pepper cache rather
   than writing two near-identical bounded/zeroizing/TTL structures. Bounded via the `lru` crate
   (new dependency, Task 1); TTL is enforced lazily on `get` (check-and-evict), not by a
   background sweep — simpler, and correct because a cache is read far more often than it grows
   stale.
4. **Cache sizing has no number in DESIGN.md** ("bounded size, TTL" is stated without values) —
   picked here as an implementation default, not a business decision: DEK cache capacity
   `100_000` entries, pepper cache capacity `8` entries (realistically one tenant per process,
   §9 decision 16, with headroom), both TTL `1 hour`. Both are constructor parameters, not
   hardcoded constants, so a future ticket sizing this against real load (T-011/T-013, the real
   callers) can change them without touching this module.
5. **`get_or_create_dek` is the lazy path (first message for a customer); `pre_provision_deks` is
   the batch path.** Both go through the same `repo::insert_if_absent` (an `INSERT ... ON
   CONFLICT (customer_id) DO NOTHING`), so a race between the two (a batch job pre-provisioning a
   customer at the same moment their first message arrives) is resolved by re-reading the row
   the loser of the race didn't win, never by two DEKs existing for one customer.
6. **`pre_provision_deks` takes an explicit `&[Uuid]` of customer ids — it does not query any
   customer table.** See the Description's scope note: the customer projection doesn't exist yet.
   `messgr-control customer-dek pre-provision` (Task 9) is the operator-facing surface this
   ticket ships; wiring an automatic trigger from the real customer base is explicitly left to
   whichever step-3+ ticket has one.
7. **`erasure`/crypto-shredding itself is out of scope** (step 15, DESIGN.md §14) — no function
   in this ticket ever sets `shredded_at` or nulls `wrapped_dek`.
8. **The AppRole-login seam T-004 left open is promoted from `tests/tenant_vault.rs`'s
   test-local `login_as_tenant` helper into production code**, `VaultKeyStore::login_as_tenant`
   (explicit args, `pub` — needed cross-crate by integration tests, same reason `create_dek`/
   `unwrap_dek` are `pub`) plus a thin env-reading wrapper `VaultKeyStore::connect_as_tenant`
   (mirrors `connect()`'s own env-var style). Unwrapping the SecretID uses a client whose own
   token *is* the wrapped value (`sys::wrapping::unwrap(client, None)`), not an admin token with
   the wrapped value passed in the request body (`Some(&wrapped)`) — T-004's test used the latter
   only because it happened to already hold an admin client for other reasons; a real tenant
   runtime process must not need one at all. `tests/tenant_vault.rs`'s own `login_as_tenant` is
   deleted in favour of calling the new production function, so there is exactly one
   implementation of this logic, not two.
9. **No Vault ACL/policy change.** Confirmed by decision 2 — pepper minting reuses
   `create_dek`/`unwrap_dek` verbatim, and AppRole login (decision 8) uses the `approle` auth
   backend, unrelated to Transit's ACL paths.
10. **`hmac`/`sha2` are pinned to the exact versions already resolved transitively** (`0.12.1`/
    `0.10.9` per `Cargo.lock` at refinement time, pulled in via `vaultrs`'s own dependency tree) —
    same "reuse the transitive version, don't add a second copy" pattern as T-004 decision 6/
    T-003 decision 6. Verify with `cargo tree --duplicates` at implementation time.

### Tasks

#### Task 1 — Dependencies, `Cargo.toml`

```toml
lru = "0.18"
hmac = "0.12"
sha2 = "0.10"
```

#### Task 2 — Tenant migration, `migrations/tenant/0003_customer_dek.sql` (new file)

```sql
-- Per-customer data-encryption keys (DESIGN.md §4.5, §7.1, §7.6). No
-- tenant_id column, matching 0001_producer.sql / 0002_tenant_config.sql
-- (§2.1) -- the tenant already is the database. shredded_at is written
-- only by the crypto-shredding erasure operation (step 15, not yet
-- built); this ticket creates the column, never writes it.
CREATE TABLE customer_dek (
    customer_id  uuid PRIMARY KEY,
    wrapped_dek  text NOT NULL,       -- Vault Transit ciphertext, "vault:v1:..." (§7.6); opaque -- never expose via queries or the API
    created_at   timestamptz NOT NULL,
    shredded_at  timestamptz          -- set when key destroyed; row retained as tombstone (step 15)
);
```

#### Task 3 — Control migration, `migrations/control/0004_tenant_vault_pepper_wrapped.sql` (new file)

```sql
-- Per-tenant HMAC pepper, wrapped by that tenant's own Transit key
-- (DESIGN.md §7.6: "the destination_hmac pepper must be per-tenant,
-- derived from that tenant's own Transit mount"). Minted lazily on first
-- use (tenant_pepper::ensure_tenant_pepper, Task 6), not at provisioning
-- time -- mirrors tenant_config's own "no auto-seeding" precedent (T-007
-- decision 4). Opaque ciphertext, same discipline as
-- customer_dek.wrapped_dek -- never expose via queries or the API.
ALTER TABLE tenant ADD COLUMN vault_pepper_wrapped text;
```

`src/tenant/model.rs`: add `pub vault_pepper_wrapped: Option<String>` to `Tenant`.
`src/tenant/repo.rs`: extend `find_by_slug`'s `SELECT` column list to include
`vault_pepper_wrapped`, and add:

```rust
pub async fn record_vault_pepper_wrapped(
    pool: &PgPool,
    tenant_id: Uuid,
    vault_pepper_wrapped: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tenant SET vault_pepper_wrapped = $1 WHERE id = $2")
        .bind(vault_pepper_wrapped)
        .bind(tenant_id)
        .execute(pool)
        .await
        .map(|_| ())
}
```

#### Task 4 — `src/key_cache.rs` (new file): bounded, zeroizing, TTL cache

```rust
//! A bounded, zeroizing, TTL-lazy-expiry cache of `Uuid`-keyed secrets
//! (DESIGN.md §7.6: "an in-process LRU cache holds unwrapped DEKs — bounded
//! size, TTL, zeroized on eviction"). Generic over what the `Uuid` keys
//! (`customer_dek::lifecycle` keys by `customer_id`; `tenant_pepper` keys by
//! `tenant_id`) — one implementation, not two near-identical ones.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lru::LruCache;
use uuid::Uuid;
use zeroize::Zeroizing;

pub struct KeyCache {
    inner: Mutex<LruCache<Uuid, (Zeroizing<Vec<u8>>, Instant)>>,
    ttl: Duration,
}

impl KeyCache {
    pub fn new(capacity: NonZeroUsize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(capacity)),
            ttl,
        }
    }

    /// A hit within `ttl` returns a clone of the cached plaintext (small —
    /// 32 bytes — and cheap to clone rather than hold a lock across the
    /// caller's use of it). A stale hit is evicted and treated as a miss.
    pub fn get(&self, key: Uuid) -> Option<Zeroizing<Vec<u8>>> {
        let mut inner = self.inner.lock().expect("KeyCache mutex poisoned");
        match inner.get(&key) {
            Some((plaintext, cached_at)) if cached_at.elapsed() < self.ttl => {
                Some(plaintext.clone())
            }
            Some(_) => {
                inner.pop(&key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: Uuid, plaintext: Zeroizing<Vec<u8>>) {
        let mut inner = self.inner.lock().expect("KeyCache mutex poisoned");
        inner.put(key, (plaintext, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn evicts_the_least_recently_used_entry_past_capacity() {
        let cache = KeyCache::new(NonZeroUsize::new(2).unwrap(), Duration::from_secs(60));
        cache.put(key(1), Zeroizing::new(vec![1]));
        cache.put(key(2), Zeroizing::new(vec![2]));
        cache.put(key(3), Zeroizing::new(vec![3])); // evicts key(1), the LRU entry

        assert!(cache.get(key(1)).is_none());
        assert_eq!(*cache.get(key(2)).unwrap(), vec![2]);
        assert_eq!(*cache.get(key(3)).unwrap(), vec![3]);
    }

    #[test]
    fn a_stale_entry_past_ttl_is_treated_as_a_miss() {
        let cache = KeyCache::new(NonZeroUsize::new(4).unwrap(), Duration::from_millis(1));
        cache.put(key(1), Zeroizing::new(vec![1]));
        std::thread::sleep(Duration::from_millis(10));

        assert!(cache.get(key(1)).is_none(), "entry must expire past its TTL");
    }

    #[test]
    fn a_fresh_entry_within_ttl_hits() {
        let cache = KeyCache::new(NonZeroUsize::new(4).unwrap(), Duration::from_secs(60));
        cache.put(key(1), Zeroizing::new(vec![9, 9]));

        assert_eq!(*cache.get(key(1)).unwrap(), vec![9, 9]);
    }
}
```

Register in `src/lib.rs`: `pub mod key_cache;`.

#### Task 5 — `src/customer_dek/` (new module): model, repo

`src/customer_dek/model.rs`:

```rust
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Mirrors the `customer_dek` table (DESIGN.md §4.5) in the tenant database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomerDek {
    pub customer_id: Uuid,
    pub wrapped_dek: String,
    pub created_at: DateTime<Utc>,
    pub shredded_at: Option<DateTime<Utc>>,
}
```

`src/customer_dek/repo.rs`, following `src/tenant_config/repo.rs`'s shape:

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::CustomerDek;

pub async fn find(pool: &PgPool, customer_id: Uuid) -> Result<Option<CustomerDek>, sqlx::Error> {
    sqlx::query_as::<_, CustomerDek>(
        "SELECT customer_id, wrapped_dek, created_at, shredded_at FROM customer_dek WHERE customer_id = $1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await
}

/// Inserts a fresh DEK row unless one already exists for this customer.
/// Returns `true` if this call's row won (was actually inserted), `false` if
/// a concurrent caller (the lazy path racing the batch path, decision 5)
/// already had — the caller is expected to `find` and use the winner's row
/// instead of the `Dek` it just discarded.
pub async fn insert_if_absent(
    pool: &PgPool,
    customer_id: Uuid,
    wrapped_dek: &str,
    created_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO customer_dek (customer_id, wrapped_dek, created_at) VALUES ($1, $2, $3) ON CONFLICT (customer_id) DO NOTHING",
    )
    .bind(customer_id)
    .bind(wrapped_dek)
    .bind(created_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}
```

#### Task 6 — `src/customer_dek/lifecycle.rs`: get-or-create, pre-provisioning, errors

```rust
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::key_cache::KeyCache;
use crate::keystore::{KeyStore, KeyStoreError};

use super::repo;

#[derive(Debug)]
pub enum CustomerDekError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for CustomerDekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "customer_dek operation failed (database): {err}"),
            Self::Vault(err) => write!(f, "customer_dek operation failed (vault): {err}"),
        }
    }
}

impl std::error::Error for CustomerDekError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for CustomerDekError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for CustomerDekError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

/// Returns `customer_id`'s plaintext DEK: a cache hit if warm, otherwise an
/// unwrap of the persisted row if one exists, otherwise a fresh Transit
/// datakey — created and persisted before this call returns, so a second
/// call (even from a different process) never mints a second DEK for the
/// same customer (decision 5's race is resolved by `repo::insert_if_absent`,
/// not by this function's own control flow).
pub async fn get_or_create_dek(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    cache: &KeyCache,
    mount: &str,
    customer_id: Uuid,
) -> Result<Zeroizing<Vec<u8>>, CustomerDekError> {
    if let Some(plaintext) = cache.get(customer_id) {
        return Ok(plaintext);
    }

    let plaintext = match repo::find(pool, customer_id).await? {
        Some(row) => keystore.unwrap_dek(mount, &row.wrapped_dek).await?,
        None => {
            let dek = keystore.create_dek(mount).await?;
            if repo::insert_if_absent(pool, customer_id, &dek.wrapped, Utc::now()).await? {
                dek.plaintext
            } else {
                // Lost the race to a concurrent creator (decision 5) — use
                // the winner's row, not the key we just generated and
                // discarded.
                let row = repo::find(pool, customer_id).await?.expect(
                    "a row must exist immediately after losing insert_if_absent's race",
                );
                keystore.unwrap_dek(mount, &row.wrapped_dek).await?
            }
        }
    };

    cache.put(customer_id, plaintext.clone());
    Ok(plaintext)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreProvisionOutcome {
    pub created: usize,
    pub already_existed: usize,
}

/// Ensures every id in `customer_ids` has a `customer_dek` row, creating one
/// for whichever don't (decision 6: caller-supplied ids only — this
/// function never queries a customer table). Idempotent: re-running with the
/// same ids reports everything as `already_existed` and mints nothing new.
pub async fn pre_provision_deks(
    pool: &PgPool,
    keystore: &dyn KeyStore,
    mount: &str,
    customer_ids: &[Uuid],
) -> Result<PreProvisionOutcome, CustomerDekError> {
    let mut outcome = PreProvisionOutcome {
        created: 0,
        already_existed: 0,
    };

    for &customer_id in customer_ids {
        if repo::find(pool, customer_id).await?.is_some() {
            outcome.already_existed += 1;
            continue;
        }

        let dek = keystore.create_dek(mount).await?;
        if repo::insert_if_absent(pool, customer_id, &dek.wrapped, Utc::now()).await? {
            outcome.created += 1;
        } else {
            outcome.already_existed += 1;
        }
    }

    Ok(outcome)
}
```

Register `src/customer_dek/mod.rs` (`pub mod lifecycle; pub mod model; pub mod repo;`) and
`pub mod customer_dek;` in `src/lib.rs`.

#### Task 7 — `src/destination_hmac.rs` (new file): the pure HMAC computation

```rust
//! Computes the keyed HMAC used for `destination_hmac`/`value_hmac` lookup
//! columns (DESIGN.md §4.1, §4.6, §7.6). Pure function — the pepper itself
//! comes from `tenant_pepper::ensure_tenant_pepper` (Task 8); this module
//! never touches Vault or Postgres.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn compute(pepper: &[u8], destination: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(pepper)
        .expect("HMAC accepts a key of any length");
    mac.update(destination.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_for_the_same_pepper_and_destination() {
        assert_eq!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-a", "+15550100")
        );
    }

    #[test]
    fn differs_across_peppers_for_the_same_destination() {
        assert_ne!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-b", "+15550100")
        );
    }

    #[test]
    fn differs_across_destinations_for_the_same_pepper() {
        assert_ne!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-a", "+15550199")
        );
    }
}
```

Register in `src/lib.rs`: `pub mod destination_hmac;`.

#### Task 8 — `src/tenant_pepper.rs` (new file): lazy pepper mint/persist/unwrap

```rust
//! The per-tenant HMAC pepper's lifecycle (DESIGN.md §7.6; T-008 decision 2):
//! minted once via the tenant's own Transit key, the wrapped ciphertext
//! persisted on `tenant.vault_pepper_wrapped`, unwrapped plaintext handed to
//! the caller to cache (`key_cache::KeyCache`, keyed by `tenant_id`) — never
//! persisted or logged in plaintext.

use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::keystore::{KeyStore, KeyStoreError};
use crate::tenant::model::Tenant;
use crate::tenant::repo as tenant_repo;

#[derive(Debug)]
pub enum TenantPepperError {
    Database(sqlx::Error),
    Vault(KeyStoreError),
}

impl std::fmt::Display for TenantPepperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "tenant pepper operation failed (database): {err}"),
            Self::Vault(err) => write!(f, "tenant pepper operation failed (vault): {err}"),
        }
    }
}

impl std::error::Error for TenantPepperError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Vault(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for TenantPepperError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<KeyStoreError> for TenantPepperError {
    fn from(err: KeyStoreError) -> Self {
        Self::Vault(err)
    }
}

/// Returns `tenant`'s HMAC pepper, minting and persisting a wrapped one on
/// first call and unwrapping the persisted one on every call after —
/// mirrors `tenant_config`'s "no auto-seeding, lazy on first use" precedent
/// (T-007 decision 4), applied to a Vault-backed secret instead of a config
/// row. Uses `tenant.vault_mount` — the exact same Transit key DEKs use, no
/// second key and no ACL change (decision 9).
pub async fn ensure_tenant_pepper(
    control_pool: &PgPool,
    keystore: &dyn KeyStore,
    tenant: &Tenant,
) -> Result<Zeroizing<Vec<u8>>, TenantPepperError> {
    match &tenant.vault_pepper_wrapped {
        Some(wrapped) => Ok(keystore.unwrap_dek(&tenant.vault_mount, wrapped).await?),
        None => {
            let secret = keystore.create_dek(&tenant.vault_mount).await?;
            tenant_repo::record_vault_pepper_wrapped(control_pool, tenant.id, &secret.wrapped)
                .await?;
            Ok(secret.plaintext)
        }
    }
}
```

Register in `src/lib.rs`: `pub mod tenant_pepper;`.

**Note for the implementer:** `Uuid` import above is unused if nothing in this file references
it directly (the tenant id comes in via `tenant.id` on the `&Tenant` parameter) — drop the
`use uuid::Uuid;` line if `cargo clippy` flags it unused; kept in this sketch only because the
doc comment above references tenant ids by concept.

#### Task 9 — `src/keystore.rs`: promote AppRole login to production code

Refactor the existing `connect_client` into two pieces (env+guard → settings; settings → client),
so the new tenant-login path can build settings and then override the token before constructing
the client:

```rust
// Replaces the current connect_client body — same behaviour, split so a
// caller can get the settings and mutate them before building a client.
pub(crate) fn connect_settings(profile: Profile) -> Result<VaultClientSettings, KeyStoreError> {
    dotenvy::dotenv().ok();

    let settings: VaultClientSettings = VaultClientSettingsBuilder::default()
        .build()
        .unwrap_or_else(|err| panic!("failed to build Vault client settings: {err}"));

    assert_tls_outside_dev(&settings.address, profile);

    Ok(settings)
}

pub(crate) fn connect_client(profile: Profile) -> Result<VaultClient, KeyStoreError> {
    Ok(VaultClient::new(connect_settings(profile)?)?)
}
```

Add to `impl VaultKeyStore`:

```rust
/// Authenticates as a tenant's own AppRole instead of the admin
/// `VAULT_TOKEN` — the seam T-004 deliberately left unwired (PLAN.md's note
/// under build step 0). Reads `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID` from
/// the environment: the RoleID and the response-wrapped SecretID a tenant's
/// dispatcher deployment received once, out of band, at deploy time (§7.6).
/// The wrapping token is single-use — calling this twice against the same
/// environment fails the second time (already unwrapped elsewhere), which
/// is correct for a one-shot startup login, not a reusable connector.
pub async fn connect_as_tenant(profile: Profile) -> Result<Self, KeyStoreError> {
    dotenvy::dotenv().ok();
    let role_id = std::env::var("VAULT_ROLE_ID")
        .unwrap_or_else(|_| panic!("VAULT_ROLE_ID must be set"));
    let wrapped_secret_id = std::env::var("VAULT_WRAPPED_SECRET_ID")
        .unwrap_or_else(|_| panic!("VAULT_WRAPPED_SECRET_ID must be set"));
    Self::login_as_tenant(&role_id, &wrapped_secret_id, profile).await
}

/// Explicit-argument version of `connect_as_tenant` — `pub` (not
/// `pub(crate)`) so integration tests can drive it directly without
/// mutating process-global environment variables, the exact hazard T-003
/// decision 4 already avoids for `assert_tls_outside_dev`.
///
/// Unwraps using a client whose own token *is* the wrapped SecretID
/// (`sys::wrapping::unwrap(client, None)`) rather than an admin client with
/// the wrapped value in the request body (`Some(&wrapped)`) — the latter is
/// what T-004's test used, only because a test conveniently already had an
/// admin client sitting around; a real tenant runtime process must not need
/// an admin token to log in as itself.
pub async fn login_as_tenant(
    role_id: &str,
    wrapped_secret_id: &str,
    profile: Profile,
) -> Result<Self, KeyStoreError> {
    let mut wrapping_settings = connect_settings(profile)?;
    wrapping_settings.token = wrapped_secret_id.to_string();
    let wrapping_client = VaultClient::new(wrapping_settings)?;

    let secret_id: vaultrs::api::auth::approle::responses::GenerateNewSecretIDResponse =
        vaultrs::sys::wrapping::unwrap(&wrapping_client, None).await?;

    let auth =
        vaultrs::auth::approle::login(&wrapping_client, "approle", role_id, &secret_id.secret_id)
            .await?;

    let mut scoped_settings = wrapping_client.settings().clone();
    scoped_settings.token = auth.client_token;
    Ok(Self {
        client: VaultClient::new(scoped_settings)?,
    })
}
```

**Note for the implementer:** `GenerateNewSecretIDResponse`'s exact field name for the unwrapped
secret (`secret_id` above, per T-004's own unresolved-until-implementation note) and whether
`sys::wrapping::unwrap(client, None)` really does treat `client`'s own configured token as the
wrapping token (rather than requiring a non-empty body) should both be confirmed against a live
dev-mode Vault before trusting this task — see the Live-Vault caveat at the top of this plan.

`tests/tenant_vault.rs`: delete the file-local `login_as_tenant` free function and its one
caller's construction path; replace both call sites
(`tenant_a_vault_credentials_cannot_read_tenant_bs_dek`'s `login_as_tenant(admin.client(), ...)`
call) with `VaultKeyStore::login_as_tenant(&outcome_a.vault_role_id, &wrapped_a, Profile::Dev)`
directly (no `admin` client needed for this step any more — only for the isolation checks that
follow it, which still use `admin.client()` exactly as before). Add a new test to this file
proving the promoted path actually authenticates end-to-end (not just that the isolation
assertions still pass with a differently-constructed scoped client):

```rust
#[tokio::test]
async fn login_as_tenant_authenticates_and_can_create_a_dek_on_its_own_mount() {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("failed to connect to control database");
    let admin = VaultKeyStore::connect(Profile::Dev)
        .expect("connecting to dev-mode Vault failed");

    let slug = unique_name("test_tenant_approle_login");
    let db_name = unique_name("test_db_approle_login");

    let outcome = provision_tenant(
        &control_pool, &control_url, &slug, "eu", &db_name, Profile::Dev, "test-actor",
        admin.client(),
    )
    .await
    .expect("provisioning failed");
    let wrapped = outcome
        .vault_wrapped_secret_id
        .expect("a fresh provision must mint a SecretID");

    let scoped = VaultKeyStore::login_as_tenant(&outcome.vault_role_id, &wrapped, Profile::Dev)
        .await
        .expect("AppRole login must succeed with a freshly minted RoleID/wrapped SecretID");

    let dek = scoped
        .create_dek(&format!("transit/{slug}"))
        .await
        .expect("a tenant-scoped login must be able to create a DEK on its own mount");
    assert_eq!(dek.plaintext.len(), 32);

    drop_test_tenant(&control_pool, admin.client(), &db_name, &slug).await;
}
```

(Requires `use messgr::keystore::KeyStore;` in scope for `.create_dek` — add alongside the
existing `use messgr::keystore::VaultKeyStore;` import.)

#### Task 10 — `messgr-control customer-dek pre-provision` subcommand

`src/customer_dek/lifecycle.rs`, add the CLI-facing wrapper (tenant-slug resolution + pool +
audit, following `tenant_config::configure::set_tenant_config`'s shape):

```rust
pub async fn pre_provision_for_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    customer_ids: &[Uuid],
    profile: crate::profile::Profile,
    actor: &str,
) -> Result<PreProvisionOutcome, CustomerDekError> {
    let tenant = crate::tenant::repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            CustomerDekError::Database(sqlx::Error::Configuration(
                format!("no tenant registered with slug {tenant_slug:?}").into(),
            ))
        })?;

    let tenant_pool = crate::tenant::pool::connect_tenant_pool(
        base_db_url,
        &tenant.database_name,
        5,
        profile,
    )
    .await?;

    let result =
        pre_provision_deks(&tenant_pool, keystore, &tenant.vault_mount, customer_ids).await;
    tenant_pool.close().await;
    let outcome = result?;

    crate::platform_audit::record(
        control_pool,
        actor,
        "customer_dek.pre_provision",
        Some(tenant.id),
        serde_json::json!({
            "requested": customer_ids.len(),
            "created": outcome.created,
            "already_existed": outcome.already_existed,
        }),
    )
    .await?;

    Ok(outcome)
}
```

`src/bin/control.rs`: new `Command::CustomerDek { command: CustomerDekCommand }` variant
(no Vault client is conditionally skipped here — this command needs one, same as `Provision`):

```rust
#[derive(Subcommand)]
enum CustomerDekCommand {
    /// Ensures each given customer id has a customer_dek row, creating one
    /// where missing (DESIGN.md §7.6). Caller-supplied ids only — see
    /// T-008's Description for why this cannot yet query a live customer
    /// base.
    PreProvision {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
        #[arg(long = "customer-id", required = true)]
        customer_id: Vec<uuid::Uuid>,
        #[arg(long)]
        actor: String,
    },
}
```

Handler (goes alongside the `Command::Provision` arm, connecting Vault the same way):

```rust
Command::CustomerDek { command } => {
    let vault_keystore = VaultKeyStore::connect(config.profile).expect(
        "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
    );
    match command {
        CustomerDekCommand::PreProvision {
            tenant_slug,
            customer_id,
            actor,
        } => {
            let outcome = messgr::customer_dek::lifecycle::pre_provision_for_tenant(
                &control_pool,
                &config.control_database_url,
                &tenant_slug,
                &vault_keystore,
                &customer_id,
                config.profile,
                &actor,
            )
            .await
            .unwrap_or_else(|err| {
                panic!("failed to pre-provision DEKs for tenant {tenant_slug:?}: {err}")
            });

            println!("created={} already_existed={}", outcome.created, outcome.already_existed);
        }
    }
}
```

#### Task 11 — Integration tests, `tests/customer_dek.rs` (new file)

Following `tests/tenant_config.rs`'s conventions (real provisioning, best-effort cleanup). Cover:

1. `pre_provision_deks` against a fresh tenant + 3 fresh customer ids creates exactly 3 rows;
   re-running with the same ids reports `created: 0, already_existed: 3` and the row count stays
   3 (no re-minted DEKs — assert by reading `wrapped_dek` before/after and comparing).
2. `get_or_create_dek` for a customer with no row creates one (assert the row now exists, and the
   returned plaintext is 32 bytes).
3. `get_or_create_dek` called twice for the same customer, second time with a `KeyCache` **cache
   hit**, must not need Vault to be reachable for that mount to succeed. Mutation-style proof
   (same standard as T-001/F13, T-003/F1, T-004's review): after the first call warms the cache,
   disable the tenant's Transit mount (`vaultrs::sys::mount::disable`), then call
   `get_or_create_dek` again for the *same* customer_id with the *same* cache — it must still
   return the identical plaintext, proving the cache is genuinely serving the second call, not
   silently re-reaching Vault. Re-enable (re-provision) the mount afterward if the test needs
   further Vault access before cleanup.
4. `get_or_create_dek` for a *different* customer_id after the mount is disabled must fail (this
   is the negative control the mutation in (3) needs — without it, a `get_or_create_dek` that
   ignored the cache entirely and always hit a live mount would still spuriously pass (3) if the
   mount disable itself failed silently).
5. `pre_provision_for_tenant` (the CLI-facing wrapper) against an unknown `--tenant-slug` returns
   an error and does not write a `customer_dek.pre_provision` platform_audit row with a non-null
   `tenant_id` (it should fail before any audit call, matching the early `?` on `find_by_slug`).

#### Task 12 — Integration tests, `tests/tenant_pepper.rs` (new file)

1. `ensure_tenant_pepper` on a freshly provisioned tenant (whose `vault_pepper_wrapped` is
   `None`) mints one, persists the wrapped ciphertext (assert
   `tenant::repo::find_by_slug(...).vault_pepper_wrapped.is_some()` afterward), and returns
   32 bytes of plaintext.
2. Calling it again for the same tenant returns **byte-identical** plaintext and leaves
   `vault_pepper_wrapped` **unchanged** (compare the wrapped string before/after) — proves it's
   reusing the persisted pepper, not minting a second one silently.
3. `destination_hmac::compute` fed that tenant's real pepper against two different destination
   strings produces two different digests (a live sanity check layered on top of the pure unit
   tests in Task 7).

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green
cargo tree --duplicates   # confirm hmac/sha2/lru resolve to one copy each, per decision 10
```

All green, including specifically:

- `key_cache::tests::{evicts_the_least_recently_used_entry_past_capacity,
  a_stale_entry_past_ttl_is_treated_as_a_miss, a_fresh_entry_within_ttl_hits}`
- `destination_hmac::tests::{is_deterministic_for_the_same_pepper_and_destination,
  differs_across_peppers_for_the_same_destination, differs_across_destinations_for_the_same_pepper}`
- `tests/customer_dek.rs`'s 5 cases, specifically the mutation-tested cache-avoids-vault case
- `tests/tenant_pepper.rs`'s 3 cases
- `tests/tenant_vault.rs::login_as_tenant_authenticates_and_can_create_a_dek_on_its_own_mount`,
  plus every existing test in that file still green using the promoted production helper
- every existing test in `tests/keystore.rs`, `tests/tenancy.rs`, `tests/producer.rs`,
  `tests/tenant_config.rs`, `tests/dev_pki.rs` unaffected

Manual smoke check:

```
just provision acme eu tenant_acme operator@example.com
cargo run --bin messgr-control -- customer-dek pre-provision --tenant-slug acme \
    --customer-id 11111111-1111-1111-1111-111111111111 \
    --customer-id 22222222-2222-2222-2222-222222222222 \
    --actor operator@example.com
```

Expected: `created=2 already_existed=0`; re-running the identical command prints
`created=0 already_existed=2`; `SELECT count(*) FROM customer_dek` on `tenant_acme` is 2 both
times.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control customer-dek pre-provision` subcommand.

- `README.md` — add a "### Customer DEK pre-provisioning" section after "### Tenant
  configuration", documenting the command, that it takes explicit customer ids (not a live
  customer base — link to the Description's scope note), and that it's idempotent.
- `justfile` — add a `customer-dek-pre-provision` recipe in the `control-plane` group, mirroring
  `tenant-config-set`'s shape (accepting a variable number of customer ids is awkward for a
  `just` recipe's fixed positional parameters — document invoking the `cargo run` form directly
  for more than one id, same as `producer-register`'s multi-flag commands already require).
- `DESIGN.md` §4.5 — no correction needed; the shipped `customer_dek` schema matches the
  documented one exactly (decision 1). §7.6 — add one sentence after the existing pepper
  paragraph naming the concrete mechanism this ticket chose (mint-once via Transit, cache
  in-process, compute locally) now that it's no longer just "must be derived from the mount" —
  matching how T-004 corrected §7.6's `tenant.vault_mount` paragraph once the concrete mechanism
  was settled.

### Finish (mandatory)

1. Acceptance test green; `just fmt`/`just lint`/`just test` all clean; `cargo tree --duplicates`
   confirms no duplicate `hmac`/`sha2`/`lru` in the tree.
2. README, justfile, and DESIGN.md §7.6 updated per the docs step.
3. Write a summary: files touched, decisions honoured (especially any place the AppRole-login
   task's unverified-against-live-Vault code (Task 9) had to change once actually compiled/run —
   flag it the same way T-004 flagged its own builder-ergonomics uncertainty), anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(dek): per-customer DEK lifecycle, cache, pre-provisioning, HMAC pepper, AppRole login (T-008)

   Adds customer_dek (DESIGN.md §4.5), a bounded zeroizing TTL cache shared
   by DEKs and the per-tenant HMAC pepper, a lazy get-or-create path plus an
   explicit-id batch pre-provisioning CLI, local HMAC-SHA256 computation
   keyed by a Vault-minted per-tenant pepper, and promotes T-004's test-only
   AppRole login into a production KeyStore code path
   (VaultKeyStore::connect_as_tenant/login_as_tenant). No Vault ACL change.
   Crypto-shredding (step 15) and wiring pre-provisioning to a live customer
   base (step 3+) are both explicitly out of scope.
   ```

5. Root-path child: interactive-rebase WIP commits into atomic, correctly scoped commits (a
   natural split: dependencies+migrations / key_cache / customer_dek / destination_hmac+tenant_pepper
   / AppRole-login promotion / CLI+tests / docs) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-08-31 — TO DO → READY: plan complete
- 2026-08-31 — READY → IN DEVELOPMENT: picked up
