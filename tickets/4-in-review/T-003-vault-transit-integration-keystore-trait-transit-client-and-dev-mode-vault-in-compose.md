---
id: T-003
title: Vault Transit integration: KeyStore trait, Transit client, and dev-mode Vault in compose
project: messgr
depends-on: [T-001]
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-003 — Vault Transit integration: KeyStore trait, Transit client, and dev-mode Vault in compose

## Outcome

After this ships, messgr can create a customer-scoped data-encryption key and get its plaintext
back from a real Vault Transit backend (dev-mode locally and in CI) through one narrow
`KeyStore` trait, instead of that trait not existing yet — the seam every later encryption
ticket (per-tenant mounts in T-004, the DEK lifecycle and cache in T-009, payload encryption in
T-012) is built against rather than each reinventing its own Vault client.

## Description

Build step 0's code half (DESIGN.md §14, §7.6): a `KeyStore` trait backed by a real Vault
Transit client, plus a dev-mode Vault reachable from `compose.yml` and CI so the trait has
something to talk to. This is deliberately **not** the ops half of step 0 (the 3-node Raft
cluster, Shamir unseal runbook — that is T-005, sized separately as "needed before go-live, not
before code") and deliberately **not** per-tenant mount/AppRole wiring into provisioning (T-004
fills that seam, the same way T-001 left `vault_mount` recorded but unused). This ticket's whole
job is: prove the Transit round-trip end to end, behind a trait narrow enough that swapping in
real per-tenant mounts (T-004) or Vault KV-backed wrapped DEKs (the deferred §7.6 hardening)
later doesn't touch call sites.

**Why this is next, not optional.** AGENTS.md's hard invariant #7 (§7, §14): per-customer DEKs
from the first write, cannot be retrofitted. T-009 (DEK lifecycle) and T-012 (`messgr-ingest`,
which must encrypt every payload before its first `INSERT`) both sit behind whatever this ticket
ships. Nothing in the ledger/ingest path can start until a `KeyStore` exists.

**Client: `vaultrs` 0.8** (new dependency — pulls in `reqwest`/`hyper`; no other maintained
pure-Rust Vault client exists). Covers everything this ticket needs: `transit::generate::data_key`
for `POST transit/datakey/plaintext/messgr-dek`, `transit::data::decrypt` for the unwrap path,
and `sys::mount`/`transit::key::create` for the dev-mode bootstrap (and later T-004's per-tenant
mounts).

**`KeyStore` trait, `src/keystore.rs`** (top-level module, not tenant-scoped — same placement
rationale as `src/platform_audit.rs` from T-002). Two methods for this ticket:
`create_dek(mount: &str) -> Result<Dek, KeyStoreError>` (returns both the plaintext and the
wrapped ciphertext to store in `customer_dek.wrapped_dek`) and `unwrap_dek(mount: &str, wrapped:
&str) -> Result<Zeroizing<Vec<u8>>, KeyStoreError>`. The bounded zeroizing LRU cache and the
pre-provisioning batch job (§7.6) are explicitly T-009's scope, not this ticket's — this proves
the primitive, not the caching story built on top of it.

**Dev-mode Vault.** A `vault` service in `compose.yml` (`hashicorp/vault` image, `server -dev`,
a fixed dev root token) plus a `justfile` recipe that bootstraps one Transit mount and key for
local testing — manual, not baked into any binary, since per-tenant mount creation is T-004's
job. CI (`.github/workflows/ci.yml`) gets a matching `vault` service in the `test` job, alongside
the existing `postgres` one, so the two-tenant integration suite's CI run (§14's "from step 0b
onward") has a real Transit backend to exercise against, not a mock.

**The non-dev guard (§7.6: "same trait-and-guard pattern as `MockProvider`", §11.1).** Vault's
HTTP API exposes no "this is a dev-mode server" flag to query — checked against `vaultrs`'
`ReadHealthResponse` and `sys::status`, neither carries one. So the guard cannot literally
detect dev-mode Vault the way it's phrased; it mirrors `MockProvider`'s *pattern* (fail loudly at
startup on a config combination that must never reach production) rather than its *mechanism*.
Concretely: at config-load time, if `profile != Dev` and the configured `VAULT_ADDR` does not
start with `https://`, the binary panics naming `VAULT_ADDR` as the offending key — the same
fail-loud shape `MockProvider`'s guard uses, applied to the one signal actually available
(dev-mode Vault has no TLS; a real deployment always does, per §7.6's AppRole/TLS posture).

Soft coupling, no hard dependency beyond `T-001` (need `Config`/`Profile` to extend, and the
control database's `tenant.vault_mount` column T-001 already created): T-004 (per-tenant mounts)
and T-009 (DEK lifecycle) both build directly on the `KeyStore` trait this ticket introduces,
and should not start before it lands.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-003-vault-transit-integration-keystore-trait-transit-client-and-dev-mode-vault-in-compose
```

All work on this branch, in this repo (`project: messgr`, `path = "."`, `layout = "in-tree"`).
Commit locally as you go. Do not push or open a merge request without explicit user approval
(this project's commit policy). Before pushing, verify the remote base is not behind:
`git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` must print
nothing.

### Prerequisite gate (hard)

`T-001` is in `6-done/` with a `MERGED` History line (`main`) — confirmed. Working tree must be
clean before starting. No other precondition.

### Confirmed design decisions (do not deviate without asking)

All five decisions from refinement, plus implementation-level ones surfaced while validating the
plan against a real `vaultrs` build and a real dev-mode Vault container (see Tasks — every code
snippet below was actually compiled and, for Task 2 and Task 7, actually run against a live
`hashicorp/vault:latest` dev server before being written here):

1. **Client crate: `vaultrs` 0.8.** Default features (`rustls`) already match `sqlx`'s
   `runtime-tokio-rustls` — no feature-flag changes needed.
2. **`KeyStore` trait uses `#[async_trait::async_trait]`, not a bare native `async fn` in a
   trait.** Verified: a bare `async fn` in a public trait trips rustc's `async_fn_in_trait`
   lint, which `cargo clippy --all-targets --all-features -- -D warnings` (this project's CI)
   turns into a hard failure. `async-trait` is already a transitive dependency of `vaultrs`
   (confirmed in `Cargo.lock`), so pinning it directly adds no new crate to the dependency tree
   — only a direct `Cargo.toml` line for a crate already being compiled. It also makes
   `KeyStore` object-safe (`Box<dyn KeyStore>`) for whenever a second implementation (a test
   double, or the Vault-KV-backed variant §7.6 defers) needs runtime selection, the same pattern
   `AuthProvider`/`Sender` are documented to use.
3. **No separate `VaultConfig` struct.** `vaultrs`'s own `VaultClientSettingsBuilder::default()`
   already reads `VAULT_ADDR`/`VAULT_TOKEN` from the process environment (confirmed by reading
   `vaultrs`' source, `src/client.rs`'s `default_address`/`default_token`) — a second, hand-rolled
   env-loading struct would just duplicate that. `VaultKeyStore::connect` calls
   `dotenvy::dotenv().ok()` first, matching every other `from_env`-style constructor in this
   codebase, then builds settings with no explicit `.address()`/`.token()` calls.
4. **The guard checks the parsed `Url`'s scheme, not a raw string, and lives in its own pure,
   unit-testable function.** `assert_tls_outside_dev(address: &url::Url, profile: Profile)`
   panics when `!profile.is_dev() && address.scheme() != "https"`, naming `VAULT_ADDR` in the
   message. Kept separate from `connect()` specifically so it can be unit-tested with a
   hand-built `Url` and no environment mutation, network call, or live Vault — `connect()` is
   sync and reads a process-global env var internally, so mutating `VAULT_ADDR` inside a test
   would race every other test in the same binary (Rust runs `#[test]`s in parallel by default);
   this decision exists to avoid that hazard entirely rather than work around it with
   `#[serial]`-style test harnesses.
5. **Key naming is fixed, not parameterized.** Both trait methods take only `mount: &str`; the
   key name inside that mount is always the constant `"messgr-dek"`, matching DESIGN.md §7.6's
   `transit/<tenant_slug>/messgr-dek` convention exactly (mount varies per tenant, key name never
   does). No tenant-mount parsing here — that is T-004's job.
6. **Base64: the `base64` crate, pinned to `0.22`** to match the version already resolved
   transitively through `reqwest`/`vaultrs` (confirmed via `cargo tree`) rather than pulling in a
   second copy at the newer `0.23` — Vault's Transit API returns/expects base64-encoded
   plaintext on both `generate::data_key` and `data::decrypt`, confirmed against a live dev
   server (Task 7).
7. **`zeroize`'s `Zeroizing<Vec<u8>>` wraps every plaintext DEK the trait returns.** The bounded
   zeroizing *cache* is T-009's scope; this ticket only guarantees the primitive itself never
   leaves a plaintext key unzeroized on drop.
8. **Dev-mode Vault bootstrap (the Transit mount + key) is a manual/CI step, not code.** A
   `justfile` recipe and a CI step both run the same two `curl` calls against Vault's HTTP API
   directly (`POST /v1/sys/mounts/transit`, `POST /v1/transit/keys/messgr-dek`) rather than
   requiring the `vault` CLI to be present on the host or writing Rust bootstrap code — verified
   against a live container in refinement.

### Tasks

#### Task 1 — Dependencies, `Cargo.toml`

```toml
vaultrs = "0.8"
async-trait = "0.1"
zeroize = "1"
base64 = "0.22"
url = "2"
```

#### Task 2 — `KeyStore` trait and Vault implementation, `src/keystore.rs` (new file)

Validated end to end against a live `hashicorp/vault:latest` dev container in refinement
(create → wrap → unwrap round-trip, and the guard panicking correctly) — this is the exact code:

```rust
//! Per-customer data-encryption keys via Vault Transit (DESIGN.md §7.6).
//!
//! `KeyStore` is deliberately narrow: two data-plane operations against a
//! mount that is assumed to already exist. Per-tenant mount/AppRole creation
//! is T-004's job; the bounded zeroizing LRU cache and DEK pre-provisioning
//! batch are T-009's. This ticket proves the Transit round-trip the rest of
//! the encryption story is built on.

use async_trait::async_trait;
use base64::Engine as _;
use url::Url;
use vaultrs::api::transit::requests::DataKeyType;
use vaultrs::client::{VaultClient, VaultClientSettings, VaultClientSettingsBuilder};
use vaultrs::transit::{data, generate};
use zeroize::Zeroizing;

use crate::profile::Profile;

/// The key name used inside every tenant's own Transit mount —
/// `transit/<tenant_slug>/messgr-dek` (DESIGN.md §7.6). Mount varies per
/// tenant; this never does.
const KEY_NAME: &str = "messgr-dek";

/// One data-encryption key. `plaintext` is zeroized on drop; `wrapped` is the
/// opaque Vault ciphertext to persist as `customer_dek.wrapped_dek` (a later
/// ticket) and hand back to `unwrap_dek`. Never log or persist `plaintext`.
pub struct Dek {
    pub plaintext: Zeroizing<Vec<u8>>,
    pub wrapped: String,
}

#[derive(Debug)]
pub struct KeyStoreError(vaultrs::error::ClientError);

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vault key store error: {}", self.0)
    }
}

impl std::error::Error for KeyStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<vaultrs::error::ClientError> for KeyStoreError {
    fn from(err: vaultrs::error::ClientError) -> Self {
        Self(err)
    }
}

/// A source of per-customer data-encryption keys, backed by Vault Transit.
/// `mount` is the tenant's own Transit mount (`tenant.vault_mount`, §4.11).
#[async_trait]
pub trait KeyStore: Send + Sync {
    async fn create_dek(&self, mount: &str) -> Result<Dek, KeyStoreError>;
    async fn unwrap_dek(
        &self,
        mount: &str,
        wrapped: &str,
    ) -> Result<Zeroizing<Vec<u8>>, KeyStoreError>;
}

pub struct VaultKeyStore {
    client: VaultClient,
}

/// Panics if `profile` is not `Dev` and `address` is not TLS-protected.
/// A dev-mode Vault always serves plain HTTP; a real deployment always uses
/// TLS (§7.6's AppRole/TLS posture) — this is the one signal actually
/// available, since Vault's own API exposes no "this is dev mode" flag to
/// query (checked against `ReadHealthResponse` and `sys::status`). Kept as
/// a free function, not a `VaultKeyStore` method, so it is unit-testable
/// with a hand-built `Url` — no environment mutation, no network call.
fn assert_tls_outside_dev(address: &Url, profile: Profile) {
    if !profile.is_dev() && address.scheme() != "https" {
        panic!(
            "VAULT_ADDR {:?} is not TLS-protected outside profile=dev — refusing to start \
             (a dev-mode Vault reachable in production is a full encryption bypass)",
            address.as_str()
        );
    }
}

impl VaultKeyStore {
    /// Connects using `VAULT_ADDR`/`VAULT_TOKEN` from the environment
    /// (`vaultrs`'s own default — see `VaultClientSettingsBuilder`).
    pub fn connect(profile: Profile) -> Result<Self, KeyStoreError> {
        dotenvy::dotenv().ok();

        let settings: VaultClientSettings = VaultClientSettingsBuilder::default()
            .build()
            .unwrap_or_else(|err| panic!("failed to build Vault client settings: {err}"));

        assert_tls_outside_dev(&settings.address, profile);

        Ok(Self {
            client: VaultClient::new(settings)?,
        })
    }
}

#[async_trait]
impl KeyStore for VaultKeyStore {
    async fn create_dek(&self, mount: &str) -> Result<Dek, KeyStoreError> {
        let response =
            generate::data_key(&self.client, mount, KEY_NAME, DataKeyType::Plaintext, None)
                .await?;
        let plaintext_b64 = response
            .plaintext
            .expect("DataKeyType::Plaintext must return plaintext");
        let plaintext = base64::engine::general_purpose::STANDARD
            .decode(plaintext_b64)
            .expect("vault returned invalid base64 plaintext");
        Ok(Dek {
            plaintext: Zeroizing::new(plaintext),
            wrapped: response.ciphertext,
        })
    }

    async fn unwrap_dek(
        &self,
        mount: &str,
        wrapped: &str,
    ) -> Result<Zeroizing<Vec<u8>>, KeyStoreError> {
        let response = data::decrypt(&self.client, mount, KEY_NAME, wrapped, None).await?;
        let plaintext = base64::engine::general_purpose::STANDARD
            .decode(response.plaintext)
            .expect("vault returned invalid base64 plaintext");
        Ok(Zeroizing::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_panics_for_non_https_address_outside_dev() {
        let addr = Url::parse("http://localhost:8200").unwrap();
        let result =
            std::panic::catch_unwind(|| assert_tls_outside_dev(&addr, Profile::Production));
        assert!(
            result.is_err(),
            "a non-https VAULT_ADDR outside profile=dev must panic"
        );
    }

    #[test]
    fn guard_allows_non_https_address_in_dev() {
        let addr = Url::parse("http://localhost:8200").unwrap();
        assert_tls_outside_dev(&addr, Profile::Dev); // must not panic
    }

    #[test]
    fn guard_allows_https_address_outside_dev() {
        let addr = Url::parse("https://vault.example.com:8200").unwrap();
        assert_tls_outside_dev(&addr, Profile::Production); // must not panic
    }
}
```

Register it in `src/lib.rs`: add `pub mod keystore;`.

#### Task 3 — Dev-mode Vault in `compose.yml`

```yaml
  vault:
    image: hashicorp/vault:latest
    container_name: messgr-vault
    restart: unless-stopped
    environment:
      VAULT_DEV_ROOT_TOKEN_ID: messgr-dev-root-token
      VAULT_DEV_LISTEN_ADDRESS: 0.0.0.0:8200
    ports:
      - "8200:8200"
    healthcheck:
      test: ["CMD-SHELL", "VAULT_ADDR=http://127.0.0.1:8200 vault status"]
      interval: 5s
      timeout: 5s
      retries: 5
```

#### Task 4 — `.env.example` / `.env`

Add:

```
VAULT_ADDR=http://localhost:8200
VAULT_TOKEN=messgr-dev-root-token
```

#### Task 5 — `justfile`: dev bootstrap recipe

```
vault-dev-init:
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit/keys/messgr-dek || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        --data '{"type":"transit"}' http://localhost:8200/v1/sys/mounts/transit-other || true
    curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
        http://localhost:8200/v1/transit-other/keys/messgr-dek || true
```

(`just`'s own `{{ }}` interpolation syntax only fires around an actual expression; a literal
JSON body in single braces passes through unmodified — confirmed with `just --dry-run
vault-dev-init` before writing this. Do **not** double the braces; doubling produces malformed
JSON `{{"type":"transit"}}}}` since `}}}}` is not consumed as a closer with no matching `{{`
open.) The second pair of mounts (`transit-other`) is Task 7's fixture for the
cross-mount-rejection test. Add a line to `README.md`'s Local development steps: after
`docker compose up -d`, run `just vault-dev-init` once (idempotent — the `|| true` absorbs
Vault's "path is already in use" on a second run, same rationale as `provision_tenant`'s own
idempotence).

#### Task 6 — CI: `.github/workflows/ci.yml`

Add a `vault` service to the `test` job (alongside `postgres`) and a bootstrap step before
`cargo test`:

```yaml
    env:
      CONTROL_DATABASE_URL: postgres://messgr:messgr@localhost:5432/control
      DATABASE_MAX_CONNECTIONS: 20
      MESSGR_PROFILE: dev
      VAULT_ADDR: http://localhost:8200
      VAULT_TOKEN: messgr-dev-root-token
    services:
      postgres:
        # ... unchanged ...
      vault:
        image: hashicorp/vault:latest
        env:
          VAULT_DEV_ROOT_TOKEN_ID: messgr-dev-root-token
          VAULT_DEV_LISTEN_ADDRESS: 0.0.0.0:8200
        ports:
          - 8200:8200
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build
      - run: cargo run --bin messgr-control -- migrate
      - name: Bootstrap Vault Transit mount + key (dev fixture, matches `just vault-dev-init`)
        run: |
          for i in $(seq 1 30); do curl -sf "$VAULT_ADDR/v1/sys/health" && break; sleep 1; done
          curl -sf --header "X-Vault-Token: $VAULT_TOKEN" --request POST \
            --data '{"type":"transit"}' "$VAULT_ADDR/v1/sys/mounts/transit"
          curl -sf --header "X-Vault-Token: $VAULT_TOKEN" --request POST \
            "$VAULT_ADDR/v1/transit/keys/messgr-dek"
          curl -sf --header "X-Vault-Token: $VAULT_TOKEN" --request POST \
            --data '{"type":"transit"}' "$VAULT_ADDR/v1/sys/mounts/transit-other"
          curl -sf --header "X-Vault-Token: $VAULT_TOKEN" --request POST \
            "$VAULT_ADDR/v1/transit-other/keys/messgr-dek"
      - run: cargo test
```

#### Task 7 — Integration tests, `tests/keystore.rs` (new file)

Validated end to end against a live dev-mode Vault in refinement (create_dek returned a 32-byte
plaintext and a `vault:v1:`-prefixed wrapped ciphertext; unwrap_dek round-tripped it exactly):

```rust
//! Integration coverage for `messgr::keystore` against a real dev-mode
//! Vault (see `just vault-dev-init` / the CI bootstrap step in
//! `.github/workflows/ci.yml` — both must have already run against this
//! process's `VAULT_ADDR` before this suite runs).

use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::profile::Profile;

fn store() -> VaultKeyStore {
    dotenvy::dotenv().ok();
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

#[tokio::test]
async fn create_dek_and_unwrap_dek_round_trip() {
    let store = store();

    let dek = store
        .create_dek("transit")
        .await
        .expect("create_dek failed");
    assert_eq!(dek.plaintext.len(), 32, "Transit's default data key is 256 bits");
    assert!(
        dek.wrapped.starts_with("vault:v1:"),
        "wrapped ciphertext must carry Vault's own versioned prefix, got {:?}",
        dek.wrapped
    );

    let unwrapped = store
        .unwrap_dek("transit", &dek.wrapped)
        .await
        .expect("unwrap_dek failed");
    assert_eq!(
        *unwrapped, *dek.plaintext,
        "unwrap_dek must return exactly the plaintext create_dek produced"
    );
}

#[tokio::test]
async fn create_dek_returns_a_distinct_key_each_call() {
    let store = store();

    let first = store
        .create_dek("transit")
        .await
        .expect("first create_dek failed");
    let second = store
        .create_dek("transit")
        .await
        .expect("second create_dek failed");

    assert_ne!(
        *first.plaintext, *second.plaintext,
        "two calls to create_dek must not return the same key"
    );
    assert_ne!(
        first.wrapped, second.wrapped,
        "two calls to create_dek must not return the same wrapped ciphertext"
    );
}

#[tokio::test]
async fn unwrap_dek_rejects_a_ciphertext_from_a_different_mount() {
    // Vault Transit binds a ciphertext to the key that wrapped it; asking a
    // *different* key (a second mount, same fixed "messgr-dek" name inside
    // it — see `just vault-dev-init`) to decrypt it must fail, not silently
    // return garbage. Requires a second bootstrapped mount, "transit-other";
    // extend the dev bootstrap (Task 5/6) to create it alongside "transit"
    // if it does not already exist.
    let store = store();

    let dek = store
        .create_dek("transit")
        .await
        .expect("create_dek failed");

    let result = store.unwrap_dek("transit-other", &dek.wrapped).await;
    assert!(
        result.is_err(),
        "decrypting under the wrong mount's key must fail, not succeed"
    );
}
```

Task 5's `justfile` recipe and Task 6's CI bootstrap step already enable this second mount
`transit-other` with its own `messgr-dek` key, so
`unwrap_dek_rejects_a_ciphertext_from_a_different_mount` has a real second key to fail against.

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

- `keystore::tests::guard_panics_for_non_https_address_outside_dev`
- `keystore::tests::guard_allows_non_https_address_in_dev`
- `keystore::tests::guard_allows_https_address_outside_dev`
- `create_dek_and_unwrap_dek_round_trip`
- `create_dek_returns_a_distinct_key_each_call`
- `unwrap_dek_rejects_a_ciphertext_from_a_different_mount`
- every existing test in `tests/tenancy.rs` and `src/db.rs`/`src/profile.rs` unit tests
  unaffected (no signature in this ticket's scope touches them)

Manual smoke check (not part of `cargo test`, mirrors T-002's shape): with `VAULT_ADDR` still
pointed at the local `http://localhost:8200` dev server, run a one-off binary or `cargo test --
--ignored` style check that calls `VaultKeyStore::connect(Profile::Production)` and confirm it
panics naming `VAULT_ADDR` — the unit tests already cover the pure guard function, so this step
is optional if reviewers are satisfied by the unit tests alone.

### Docs update (mandatory when user-facing)

`README.md`'s Local development section: document the new `vault` compose service, the
`VAULT_ADDR`/`VAULT_TOKEN` env vars, and the one-time `just vault-dev-init` step (Task 5). No
other user-facing surface exists yet — no binary calls `KeyStore` in production code paths until
T-004/T-009/T-012.

### Finish (mandatory)

1. Acceptance test green; `cargo fmt --check` / `cargo clippy --all-targets --all-features -D
   warnings` / `cargo build` clean.
2. `README.md` updated per the Docs update step above.
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggest a Conventional Commit message, ticket id in brackets, e.g.:
   ```
   feat(keystore): add Vault Transit KeyStore and dev-mode Vault (T-003)
   ```
5. This is a root-path child (`path = "."`) — interactive-rebase WIP commits into a small
   number of atomic, correctly typed/scoped commits before presenting them (replaces
   squash-on-merge for this repo).
6. Commit locally on
   `feat/T-003-vault-transit-integration-keystore-trait-transit-client-and-dev-mode-vault-in-compose`.
   Do not push or open a merge request without user approval. Present the commit message; only
   after approval, verify the remote base is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` must print nothing), then push and open
   the merge request. Merging is always the human's. Hand back to the user.

## Review

- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a, if the project ships docs)
- [x] Docs-readability pass on the ticket's changed `.adoc`/`.md` files, or a conscious skip recorded (step 4b, optional) — skipped: no docs-readability reviewer configured in this session; the one prose change (`README.md`) is short and unambiguous
- [x] Findings recorded in the ticket's `## Review` with severity, **class**, **and** disposition per the rules §5; disposition summary line present, and a `cost: estimated …, actual …` line beneath it (step 5)
- [x] Remaining-tickets impact sweep done (step 8)

**Diffed** `main..feat/T-003-vault-transit-integration-keystore-trait-transit-client-and-dev-mode-vault-in-compose`
(commit `58b8f09`). Every file the plan named was touched, and nothing unexpected was: `Cargo.toml`/`Cargo.lock`
(Task 1), `src/keystore.rs`/`src/lib.rs` (Task 2), `compose.yml` (Task 3), `.env.example` (Task 4), `justfile`
(Task 5), `.github/workflows/ci.yml` (Task 6), `tests/keystore.rs` (Task 7), `README.md` (docs). The ticket file
and `tickets/BOARD.md` also show a 3-line diff across the two branches — that is the documented in-tree
stale-worktree artifact (the feature branch forked at pickup, before the two later `board:` bookkeeping
commits landed on `main`), not a real change; confirmed by reading the ticket from `main` throughout this
review rather than from the feature-branch worktree copy.

**Implementation audit.** All 8 tasks present in the files they name, matching the plan's code almost
verbatim (only line-wrap differences from `cargo fmt`). Both confirmed-decision-affecting "plan amended
inline" History lines were actually applied: the `justfile` recipe (`justfile:44-51`) uses single braces
throughout (`{"type":"transit"}`, not `{{{{...}}}}`), and `url = "2"` is present in `Cargo.toml` and used
directly (`use url::Url;`, `src/keystore.rs:9`). All 8 confirmed design decisions honoured in the code:
rustls-only dependency tree (`cargo tree` shows no openssl/native-tls), `#[async_trait]` used on `KeyStore`
(avoids the `async_fn_in_trait` clippy-as-error lint), no `VaultConfig` struct (`VaultClientSettingsBuilder::default()`
reads `VAULT_ADDR`/`VAULT_TOKEN` from the environment — confirmed by reading `vaultrs` 0.8.0's own
`src/client.rs:242-266`), the guard is a pure free function taking a parsed `Url` (`src/keystore.rs:76`),
`KEY_NAME` is a fixed constant, `base64` pinned to `0.22` resolves to a single tree-wide copy at `0.22.1`
(shared with `sqlx-postgres`/`hyper-util`/`reqwest` — `cargo tree -i base64` / `cargo tree --duplicates`
show no second version), every plaintext DEK is `Zeroizing<Vec<u8>>`, and the dev bootstrap is a manual
`curl`-based `justfile`/CI step, not code.

**Acceptance test re-run, verbatim**, with a temporary port remap for `docker compose up -d` (port 5432
occupied locally by an unrelated `langfuse-postgres` container — `compose.yml`'s `postgres` port remapped
to `55432:5432` and `.env`'s `CONTROL_DATABASE_URL` updated to match, both reverted after; `git status` is
clean of the tinkering):

```
docker compose up -d                                    # postgres + vault, both healthy
just vault-dev-init                                     # both transit/messgr-dek and transit-other/messgr-dek created
cargo run --bin messgr-control -- migrate               # ok
cargo fmt --check                                       # clean
cargo clippy --all-targets --all-features -- -D warnings  # clean
cargo build                                              # clean
cargo test                                               # 8 lib + 3 keystore + 6 tenancy = 17 passed, 0 failed
```

All green, including specifically the 3 new keystore unit tests, the 3 new integration tests, and every
pre-existing test in `tests/tenancy.rs` / `src/db.rs` / `src/profile.rs` unaffected, exactly as the plan's
acceptance test lists.

**Quality/consistency audit.** `KeyStore` trait design is narrow and matches the stated seam rationale
(two data-plane methods, `mount: &str` parameter, fixed key name). `KeyStoreError` correctly wraps
`vaultrs::error::ClientError` (`From` impl, `Display`/`Error::source` delegate). Token/port/address are
consistent across `compose.yml`, `.github/workflows/ci.yml`, `justfile`, and `.env.example`
(`messgr-dev-root-token`, `8200`). `cargo tree`/`cargo tree --duplicates` confirm both version claims in
Decision 2 (`async-trait` transitive via `vaultrs`→`rustify`, one copy at `0.1.92`) and Decision 6
(`base64` transitive via `reqwest`/`sqlx-postgres`, one copy at `0.22.1`) are true, not merely asserted.
DESIGN.md §7.6/§4.11 cross-checked against the code: `messgr-dek` constant, `vault:v1:` prefix assertion,
and `wrapped_dek` opacity are all honoured; no other doc in the tree references `KeyStore`/`vault-dev-init`
in a way this ticket makes stale.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | test-gap | — | `unwrap_dek_rejects_a_ciphertext_from_a_different_mount` (`tests/keystore.rs:66-83`) asserts only `result.is_err()`. That is satisfied by *any* Vault-side failure, not specifically by the cross-mount decryption rejection the test's name and comment claim to prove — the same failure shape as T-001/F13 (an assertion too permissive to certify the mechanism under test). Concretely: if the `transit-other` mount's bootstrap silently failed (its own `curl` calls are wrapped in `\|\| true` in the `justfile`, precisely to tolerate a failure), this test would still report green while proving nothing about DEK cross-mount isolation — the property this ticket exists to establish (DESIGN.md §7.6/§4.11, "a compromised tenant credential cannot decrypt another tenant's data"). | Deleted the `transit-other` mount (`curl -X DELETE http://localhost:8200/v1/sys/mounts/transit-other`, no key present at all afterward) and re-ran `cargo test --test keystore unwrap_dek_rejects_a_ciphertext_from_a_different_mount`: **still reports `ok. 1 passed; 0 failed`.** Restored the mount afterward and confirmed the full suite still passes with it present. | Assert on the specific Vault error rather than `is_err()` alone — e.g. downcast/inspect `KeyStoreError`'s inner `vaultrs::error::ClientError` variant (or its HTTP status) and require it indicate a decrypt/ciphertext failure, not a missing-path/mount error; or, cheaper, add a preceding assertion in the same test that `transit-other` actually has its own working `messgr-dek` (e.g. a successful `create_dek("transit-other")` round-trip) before attempting the cross-mount decrypt, so a broken fixture fails loudly and separately from the property under test. |
| F2 | non-blocking | design | noted | `create_dek`/`unwrap_dek` (`src/keystore.rs:104-108, 121-124`) and `VaultKeyStore::connect`'s settings-build path (`src/keystore.rs:91-96`) use `.expect(...)`/`.unwrap_or_else(|err| panic!(...))` for base64-decode and Vault-protocol-shape failures, even though every one of these functions returns `Result<_, KeyStoreError>`. A malformed base64 payload or an unparseable `VAULT_ADDR` therefore crashes the process instead of surfacing as an `Err` the caller could log/handle, despite `KeyStoreError` existing specifically to carry Vault failures. Defensible as "this should never happen against a real Vault" (same fail-loud spirit as `assert_tls_outside_dev`), but inconsistent with the trait's own Result-based contract. | `src/keystore.rs:91-96` (settings build), `:104-108` (`create_dek` decode), `:121-124` (`unwrap_dek` decode). | Worth reconsidering once a real caller (T-009's DEK cache, T-012's ingest path) has to decide how to react to a Vault hiccup in production — not worth a dedicated ticket on its own. |
| F3 | non-blocking | docs-gap | noted | `README.md`'s new Local development paragraph documents `just vault-dev-init` as "enable Transit + create the `messgr-dek` key" (singular) but doesn't mention that it also creates a second `transit-other` mount + key, which exists solely as `tests/keystore.rs`'s cross-mount fixture. A reader following the README alone would not know why a second mount appears in their local Vault. | `README.md:12,21-28` vs. `justfile:44-51`. | Not worth a dedicated ticket; fold into the same README paragraph the next time this section is touched (e.g. alongside T-004's per-tenant mounts, which will make the mount story more complex anyway). |

**Disposition summary:** 1 blocking (F1), 2 non-blocking both `noted` (F2, F3). No `folded`/`new ticket` dispositions — nothing here passes the promotion test on its own.

cost: estimated M, actual M

**Verdict: rework required.** F1 is the acceptance test's own required cross-mount-rejection case, sitting
exactly in the failure class (T-001/F13) this review was explicitly asked to check for, and it fails the
same mutation test (delete the fixture the assertion depends on; the test still passes). The shipped
`KeyStore`/`VaultKeyStore` implementation itself is correct — behavior was verified directly (the
round-trip and distinct-key tests are real, and manually confirmed the wrong-mount decrypt does fail with
the fixture present) — this is a test-credibility defect, not a shipped-behavior defect, but per §5 it is
still blocking: an acceptance-required test that cannot fail for the reason it claims to test is a broken
acceptance gate, and a later regression in real cross-mount isolation would ship silently green. Ticket
moves to `tickets/5-rework/` for a scoped fix to F1 only (F2/F3 stand as recorded, non-blocking, `noted`);
a scoped re-review then verifies F1 alone.

**Impact sweep (step 8).** No ticket in `tickets/1-to-do/` or `tickets/2-ready/` currently lists T-003 in
`depends-on:` (T-004/T-009/T-012, the tickets this one unblocks per its own Description, are not yet filed).
Nothing to patch.

### Rework (commit `a9c1308`)

| id | fix |
|---|---|
| F1 | `unwrap_dek_rejects_a_ciphertext_from_a_different_mount` (`tests/keystore.rs`) now does two things the original didn't: (1) round-trips a DEK through `transit-other`'s own key first (`create_dek` + `unwrap_dek` on `transit-other` itself), so a broken/missing fixture fails loudly on that assertion with a diagnostic naming the fixture, before the property under test is even attempted; (2) asserts on the actual error content — `format!("{error:?}")` on the returned `KeyStoreError` (its derived `Debug` surfaces `vaultrs::error::ClientError::APIError`'s `errors` field even though the field itself is private) — requiring it contain `"cipher"` or `"authentication"`, not merely that *some* error occurred. Verified by re-running the exact mutation the review used: deleting the `transit-other` mount now fails the test with `"transit-other's own create_dek failed — the cross-mount fixture is broken, not the property this test exists to check: KeyStoreError(APIError { code: 404, errors: [\"no handler for route \\"transit-other/datakey/plaintext/messgr-dek\\". route entry not found.\"] })"` — the exact case that previously passed silently. Restoring the mount returns the suite to green. |

Acceptance test re-run in full after the fix (fresh Postgres 18 + dev-mode Vault, temporary local port
remap for Postgres only): `cargo fmt --check` clean, `cargo clippy --all-targets --all-features -- -D
warnings` clean, `cargo build` clean, `cargo test` → 17 passed, 0 failed (8 lib + 3 keystore + 6 tenancy).

F2 and F3 untouched, as scoped — F1 was the entire rework.

## History

- 2026-08-22 — created (TO DO). source: chat: PLAN.md's build-order decomposition of DESIGN.md §14 step 0 (Vault, code half) — the next unblocked ticket after T-001/T-002, foundational for the per-customer-DEK invariant (§7.6, AGENTS.md #7). Renumbered from the plan's original provisional `T-002` after that id was consumed by an unplanned ticket (T-002, spawned from T-001's review).
- 2026-08-22 — TO DO → READY: plan complete
- 2026-08-22 — plan amended inline: applicability-gate audit (fresh sub-agent) found Task 5's `justfile` recipe doubled the JSON body's braces (`{{{{...}}}}`), which `just` does not un-escape symmetrically — `{{{{` becomes a literal `{{`, but the unmatched `}}}}` passes through unmodified, producing malformed JSON that Vault would 400 on while `|| true` hid the failure. Fixed to single braces (verified with `just --dry-run`); folded Task 7's `transit-other` second-mount fixture directly into Task 5's recipe and Task 6's CI step instead of leaving it as a separate to-do note. No other finding; dependency resolution (`vaultrs` 0.8.0, `async-trait` 0.1.92, `base64` 0.22.1, `zeroize` 1.9.0, none yanked) and every file the plan touches re-verified unchanged since refinement.
- 2026-08-22 — plan amended inline: Task 1's dependency list omitted `url`, which Task 2's `src/keystore.rs` code imports directly (`use url::Url;`) for the guard function's signature. `url` is only a transitive dependency of `vaultrs` and is not re-exported by it, so a direct `use url::Url` does not compile without declaring it in `Cargo.toml`. Added `url = "2"` to Task 1; caught while executing Task 1, before any code was written.
- 2026-08-22 — READY → IN DEVELOPMENT: picked up
- 2026-08-22 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-08-22 — IN REVIEW → REWORK: 1 blocking finding, F1 — `unwrap_dek_rejects_a_ciphertext_from_a_different_mount` is not falsifiable against the property it claims to test (passes with the `transit-other` fixture deleted entirely); 2 non-blocking findings noted (F2, F3)
- 2026-08-22 — IN REVIEW → REWORK: review: F1 blocking (unwrap_dek_rejects_a_ciphertext_from_a_different_mount asserts only is_err(), passes with the fixture mount deleted); F2, F3 noted
- 2026-08-22 — REWORK → IN REVIEW: findings fixed
