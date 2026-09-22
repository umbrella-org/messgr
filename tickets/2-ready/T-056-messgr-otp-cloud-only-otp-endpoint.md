---
id: T-056
title: messgr-otp: cloud-only OTP endpoint
project: messgr
depends-on: []
spawned-by: [T-054]
impact: critical
complexity: medium
cost: M
---

# T-056 — messgr-otp: cloud-only OTP endpoint

## Outcome

After this ships, a cloud tenant's staff can complete OTP without linking a Rust library:
`messgr-otp` gives them a dedicated, mTLS-authenticated network endpoint per region that answers
synchronously, with an async best-effort audit record, exactly matching the on-prem library's
observable behaviour and latency profile.

## Description

The cloud-only variant of T-052's OTP mechanism (`02-otp.md` §3.1, "the cloud variant" —
`otp-api`): its own binary and process pool, sharing nothing with `messgr-ingest` or the
dispatchers. Synchronous: authenticate the tenant by mTLS, look up the provider credential, call
the provider, return — no queue, no gate chain, no Postgres write on the request path. The audit
record is written asynchronously and best-effort, exactly as on-prem (T-052, done and merged, PR
#65, `8182cf7`).

**Reuses T-052's `sms-sender` shape, with one deliberate divergence.** `src/sms_sender/` already
has the mTLS/producer-identity resolution (`identity.rs`), the audit-write-then-buffer-then-
pending-buffer degrade chain (`handler.rs`, `buffer.rs`, `pending.rs`), and the `AuthEnabledCache`
fail-open check — all of that carries over unchanged in shape. **What does not carry over:**
`sms_sender::provider::ProviderConfigCache` calls Vault on every send (`read_credential`),
falling back to the last-known value only on a Vault failure — Vault is on the request path,
just gracefully degrading. §3.1's correction is explicit that `otp-api` must not do this:
"fetches its provider credential from Vault at startup and holds it in memory for the life of
the process, refreshed on a background timer rather than per request, so a sealed or unreachable
Vault degrades nothing on the OTP request path." Since tenants bring their own provider accounts
(decision 18) there is no single credential to prefetch once at boot — the cache is per-tenant,
populated the first time a tenant is seen (mirroring `TenantRegistry::get_or_open`'s lazy-open
shape) and refreshed only by its own background timer from then on, never by the request path
itself. This is new code, not a reuse of `ProviderConfigCache`.

Resolves design-doc still-open item #12 (cloud OTP posture) as: build it, per user confirmation
during T-054's refinement — cloud tenants get a hosted endpoint rather than being steered
exclusively to an on-prem auth pattern. §3.1 also asks that cloud tenants be told plainly this is
weaker than the on-prem story (a network hop and a shared regional service now sit between them
and their provider) and that latency/availability-sensitive tenants can keep auth on-prem while
using cloud for everything else — that disclosure is a docs/sales matter, not a code task, but
the Docs step below records it.

Soft coupling: shares the region concept with T-060 (second region) — `messgr-otp` must be
deployable per-region from the start, but does not depend on T-060 landing first; a single-region
deployment is a valid intermediate state.

Hard invariant #1 (`AGENTS.md`) applies in full here: this endpoint is synchronous and bypasses
the gate chain/queue entirely, same as on-prem OTP — a marketing incident must not be able to
stop cloud customers logging in either.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-056-messgr-otp-cloud-only-otp-endpoint
```

### Prerequisite gate (hard)

T-052 (`messgr-sms-sender`) is done and merged (PR #65, `8182cf7`) — `src/sms_sender/` exists on
`main` at the paths this plan references. No other precondition.

### Confirmed design decisions (do not deviate without asking)

1. **New binary, new module, not a flag on `sms_sender`.** `02-otp.md` §3.1: "its own binary and
   its own process pool, sharing nothing with `ingest-api` or the dispatchers." Build
   `src/bin/otp.rs` (binary name `messgr-otp`) and a new `src/otp/` module — do not add a
   `--cloud` mode to `messgr-sms-sender`.
2. **Reuse by copy-and-adapt, not by making `sms_sender` generic over both modes.** `identity.rs`,
   the audit/buffer/pending chain, and `AuthEnabledCache` move into `src/otp/` in the same shape;
   introducing a shared crate-level abstraction for two callers (on-prem, cloud) that will never
   need a third is the kind of premature generalization this project's working style avoids —
   two similar modules is cheaper to read than one parameterized one.
3. **Provider-credential caching is new code (`src/otp/provider.rs`), not `ProviderConfigCache`
   reused.** Per-tenant: `HashMap<Uuid, CachedCredential>` behind a lock, populated on first sight
   of a tenant (a lazy fetch, exactly once, not per-request), refreshed only by a background
   timer task per §7.6's DEK-cache discipline. The request handler never calls `KeyStore` itself
   — only reads the cache.
4. **Background refresh interval:** reuse `AUTH_FLAG_POLL_INTERVAL`'s existing precedent of a
   short, fixed poll — 60s. No jitter, no backoff (matches `auth_flag`/`kill_switch` cache
   refresh loops elsewhere in this codebase); if a rotation needs to propagate faster than that
   in practice, that is a future tuning knob, not something to build speculatively now.
5. **Listen port / env var naming mirrors `SMS_SENDER_*`:** `OTP_LISTEN_ADDR`,
   `OTP_HEALTH_LISTEN_ADDR`, `OTP_TLS_CERT_FILE`, `OTP_TLS_KEY_FILE`, `OTP_TLS_CLIENT_CA_FILE`,
   `OTP_BUFFER_PATH`, `OTP_PENDING_PATH` — same shape as `src/bin/sms_sender.rs`'s env vars, `OTP`
   in place of `SMS_SENDER`.
6. **Quiet hours are not evaluated on this path**, same as on-prem OTP (§3, "Quiet hours are not
   evaluated on this path at all — OTP is exempt by policy") — carry the exemption forward, do
   not add a quiet-hours check.

### Tasks

#### Task 1 — `src/otp/` module skeleton
Create `src/otp/mod.rs`, `identity.rs`, `model.rs`, `handler.rs`, `buffer.rs`, `pending.rs`,
`repo.rs`, `auth_flag.rs`, `provider.rs`. Copy `src/sms_sender/identity.rs`,
`model.rs`/`buffer.rs`/`pending.rs`/`repo.rs`/`auth_flag.rs` verbatim (adjusting only module
paths/doc comments to say `otp`/`messgr-otp` instead of `sms_sender`/`messgr-sms-sender`) — these
five files are identical in shape per decision 2.

#### Task 2 — `src/otp/provider.rs`: at-startup-per-tenant, background-refreshed credential cache
New type `OtpProviderCache` (not `ProviderConfigCache`):
```rust
pub struct OtpProviderCache {
    // keyed by tenant_id; each entry is the provider_config rows + resolved
    // credential(s) for that tenant, populated once and refreshed only by
    // run_refresh_loop below
}
```
- `get_or_fetch(&self, pool: &PgPool, keystore: &dyn KeyStore, tenant_id: Uuid) -> Vec<ResolvedProviderConfig>`
  — returns the cached entry; on a cold miss, loads `provider_config` rows
  (`crate::provider_config::repo::list`, same query `sms_sender::provider::send` uses) and
  resolves each `credential_path` via `keystore.read_provider_credential` once, then caches.
  Never called from a background task, only from the first request for a tenant this process has
  seen (mirrors `TenantRegistry::get_or_open`'s lazy-open shape, `src/tenant/registry.rs:151`).
- `run_refresh_loop(cache, control_pool, keystore, registry, poll_interval)` — every tick, for
  every tenant currently in the cache (not the registry — only tenants actually seen), re-reads
  `provider_config` and re-resolves credentials, replacing the cached entry. A refresh failure
  logs and keeps the existing cached entry (same "serve the last known snapshot" shape as
  `sms_sender::provider::ProviderConfigCache`, just on a timer instead of per-request).
- Send logic itself (`crate::sender::http::HttpSender`, the priority walk over `provider_config`
  rows) is unchanged from `sms_sender::provider::send` — only credential *acquisition* moves out
  of the request path; adapt `send` to take already-resolved credentials instead of a `KeyStore`
  handle.

#### Task 3 — `src/otp/handler.rs`: the `/otp` handler
Copy `sms_sender::handler::send_otp`'s structure (auth-flag fail-open check first, provider send
before any DEK/Vault/Postgres work, `persist_send_outcome` degrade chain) but call
`otp::provider::send` with the `OtpProviderCache`-resolved credential instead of
`sms_sender::provider::send`'s `(cache, pool, tenant_id, keystore, ...)` signature.

#### Task 4 — `src/bin/otp.rs`
Mirror `src/bin/sms_sender.rs` end to end: `Config::from_env`, control-pool connect, `VaultKeyStore`
connect, `TenantRegistry`, `AuthEnabledCache` + its refresh loop, mTLS server config
(`mtls::load_server_config`, `OTP_TLS_*` env vars), health listener
(`OTP_HEALTH_LISTEN_ADDR`), buffer/pending drain loops (`otp::buffer::run_drain_loop`,
`otp::pending::run_drain_loop`). Replace `sms_sender`'s `run_drain_loop`/`ProviderConfigCache`
wiring with `otp::provider::OtpProviderCache` + its `run_refresh_loop` (task 2). Register the
`[[bin]]` entry in `Cargo.toml` alongside `sms_sender`/`control`/`webhook`/`dispatcher`/`ingest`/
`query_api`.

#### Task 5 — Region awareness
Read `MESSGR_REGION`/`config.region` if T-060 has landed by the time this is implemented
(`src/config.rs`); if not, `messgr-otp` still starts and serves whatever tenants are registered
in its configured control database — region is a deployment-time fact (which control DB/Vault
this process points at), not something `otp-api`'s own code needs to branch on. No hard
`depends-on:` on T-060 (soft coupling only, per the Description).

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including new tests for:
   - `OtpProviderCache::get_or_fetch` resolves and caches on first call, and does not call
     `KeyStore` again on a second call for the same tenant within the same refresh tick (mutation
     test per the review addendum's step 3 advisory: assert the mock `KeyStore`'s call count is
     exactly 1 across two `get_or_fetch` calls).
   - `run_refresh_loop` updates a cached entry after one tick and leaves it untouched (does not
     panic, does not evict) when the reload fails.
   - `send_otp` handler: auth-disabled tenant discards without calling the provider (same
     assertion shape as `sms_sender`'s existing `AuthDisabled` test).
3. Manual/integration: start `messgr-otp` against the dev compose stack, send an mTLS-authenticated
   `POST /otp` for a registered tenant, confirm `comms_request` gets an audit row and the response
   is `201`.

### Docs update (mandatory when user-facing)

Add `docs/user-manual/otp-api.adoc` (mirroring `sms-sender.adoc`'s structure: what it does, env
vars, the `/otp` request/response shape, the Vault-credential-caching behaviour and its
background-refresh interval, the disclosure to cloud tenants that this adds a network hop
relative to on-prem). Register it in `docs/user-manual.adoc`'s `include::` list, immediately
after `sms-sender.adoc`. Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated and registered.
3. Write a summary (files touched, decisions made, anything deferred) and hand back for review.
4. Suggested commit message: `feat(otp): cloud-only messgr-otp endpoint (T-056)`.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed cloud OTP posture (design-doc still-open item #12: build messgr-otp,
  do not defer it in favour of an on-prem-only recommendation)
- 2026-09-22 — TO DO → READY: plan complete: new messgr-otp binary/module reusing sms_sender's identity/audit/buffer shape; new at-startup+background-timer provider-credential cache per §3.1 correction (not sms_sender's per-request ProviderConfigCache)
