---
id: T-025
title: Operability and error-handling cleanup across CLI and services
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: low
cost: M
---

# T-025 — Operability and error-handling cleanup across CLI and services

## Outcome

`messgr-control --help` and a bare invocation print usage instead of panicking on a missing env var; `migrate` reports success visibly regardless of `RUST_LOG`; malformed configuration fails loudly and consistently rather than half-panicking and half-silently-defaulting; a malformed Vault response is a returned `Err` rather than a panic; `TenantRegistry` evicts idle tenant contexts instead of growing forever; a health endpoint exists; and CI calls the same `just` recipes a developer runs locally.

## Description

Batches nine small, previously-`noted` findings from prior ticket reviews that individually fail the "would this actually be scheduled?" promotion test but together are worth one pass, plus two gaps this audit found directly. None are behaviour changes to the product; all are operability.

From prior reviews (all currently `noted`, standing as recorded in their tickets):

1. **T-001/F7** — `Config::from_env()` runs before `Cli::parse()`, so `messgr-control --help` and a bare invocation panic on a missing `CONTROL_DATABASE_URL` instead of printing usage (`src/bin/control.rs:37-38`). Fix: parse argv first, load config after.
2. **T-001/F9** — `Command::Migrate` reports success only via `tracing::info!`, filtered by `RUST_LOG` (unset in `.env.example` and CI), so `just control-migrate` prints nothing on success while `provision` correctly uses `println!`. Fix: match `provision`'s behaviour or set a default log filter.
3. **T-001/F12** — Two contradictory malformed-config policies: `Profile::from_env()` panics on an unrecognized value (by design); `Config::from_env()` silently substitutes a default for an unparseable `DATABASE_MAX_CONNECTIONS`, under a doc comment that claims uniform loud failure. Fix: pick one policy (loud, per `Profile`'s precedent) and make the comment match reality.
4. **T-003/F2** — `create_dek`/`unwrap_dek` and `VaultKeyStore::connect`'s settings-build path use `.expect(...)`/`panic!(...)` for base64-decode and Vault-protocol-shape failures, despite every one of these functions returning `Result<_, KeyStoreError>` (`src/keystore.rs:91-96, 104-108, 121-124`). Fix: propagate as `Err` — a malformed Vault response should not crash the dispatcher.
5. **T-007/F2, T-012/F1** — input validation gaps noted in `tenant_config` and `provider_config` set paths; confirm exact scope against those tickets' Review sections at refinement.
6. Roughly twenty `panic!`/`.expect()` sites across the CLI binaries that should be exit codes with a message instead, per the pattern F7/F9/F12/T-003-F2 above establish — audit and convert during this ticket rather than one at a time.

Found directly by this audit, not previously reviewed:

7. **`TenantRegistry` never evicts** (`src/tenant/registry.rs`) — a plain `HashMap<Uuid, Arc<TenantContext>>` behind an `RwLock`, grown on every never-before-seen `tenant_id`, with no TTL, no LRU bound, and no removal on tenant suspension/offboarding. At 20 tenants this is not yet a resource problem, but a suspended or offboarded tenant's pool and cached Vault mount stay open indefinitely, and the map has no bound at all if the platform ever grows past the ~20-tenant design target (§9). Add eviction (idle TTL, or explicit removal on `tenant.status` transitions once T-016/suspension checking exists).
8. **No health endpoint exists on any binary.** `messgr-ingest`, `messgr-query`, `messgr-dispatcher`, and `messgr-webhook` have no way for an orchestrator or load balancer to ask "are you up" short of a real request. Add a minimal `/healthz` (or equivalent) per binary.
9. **CI inlines `cargo build`/`cargo test`/raw `docker exec` commands** (`.github/workflows/ci.yml`) instead of calling the `just` recipes (`just build`, `just test`, `just vault-dev-init`, ...) a developer runs locally — confirmed by this audit's own use of those recipes when wiring `pickle.toml`'s new command keys. Two paths for the same operation drift silently; CI should call the recipes.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: batches nine noted operability findings from prior ticket reviews (T-001/F7,F9,F12; T-003/F2; T-007/F2; T-012/F1) with two found directly by the 2026-09-02 design/implementation audit (TenantRegistry has no eviction; no health endpoint on any binary), plus CI inlining cargo/docker rather than calling just recipes.
