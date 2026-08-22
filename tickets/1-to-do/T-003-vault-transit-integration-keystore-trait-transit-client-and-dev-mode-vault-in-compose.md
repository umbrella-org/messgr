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

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-22 — created (TO DO). source: chat: PLAN.md's build-order decomposition of DESIGN.md §14 step 0 (Vault, code half) — the next unblocked ticket after T-001/T-002, foundational for the per-customer-DEK invariant (§7.6, AGENTS.md #7). Renumbered from the plan's original provisional `T-002` after that id was consumed by an unplanned ticket (T-002, spawned from T-001's review).
