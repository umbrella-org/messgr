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
4. **Background refresh interval: 60s, fixed, no jitter, no backoff** — matching the shape (not
   the number) of existing refresh loops in this codebase (`AUTH_FLAG_POLL_INTERVAL` = 5s,
   `KILL_SWITCH_POLL_INTERVAL`/`QUOTA_CONFIG_POLL_INTERVAL` = 30s — none of these is 60s).
   Credential rotation is less latency-sensitive than an auth-enable kill switch, so a slower
   poll is fine on its own merits; if a rotation needs to propagate faster than that in practice,
   that is a future tuning knob, not something to build speculatively now.
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

Reviewer independence (step 0): **independent** — this review ran in a fresh session (no memory
of authoring the branch); no delegation needed. In-tree stale-branch check (step 0a): the feature
branch's ticket copy was found stale (`3-in-development` vs. `main`'s `4-in-review`); rebased
onto `main`, `pickle doctor` then clean.

Implementation audit (step 2): every task and confirmed decision (1–6) verified against the
actual tree — new `src/bin/otp.rs`/`src/otp/` module (not a `sms_sender` flag, decision 1);
`identity.rs`/`model.rs`/`buffer.rs`/`pending.rs`/`repo.rs`/`auth_flag.rs` are byte-identical in
logic to `sms_sender`'s versions, doc comments only reworded (decision 2, diffed by hand);
`OtpProviderCache` is new code, at-startup-per-tenant + 60s background-timer refresh, never
`ProviderConfigCache` (decision 3); refresh interval fixed 60s, no jitter/backoff (decision 4);
`OTP_*` env var names mirror `SMS_SENDER_*` (decision 5); no quiet-hours check added (decision
6, grepped). `just build`/`just lint`/`just docs-check` clean; `just test` green (`tests/otp.rs`:
3/3 passed, including the mutation-test assertion that `get_or_fetch`'s second call does not
re-read Vault). One unrelated failure surfaced (`tests/query_api.rs::producers_usage_and_quota_are_comms_ops_only`,
a pre-existing minute-boundary-sensitive test from T-048, untouched by this diff) — reran in
isolation and it passed; recorded below as a noted flake, not this branch's fault.

Quality audit (step 3): mutation-test advisory satisfied (`CountingKeyStore` proves the cache
call count, not just `is_err()`). Error handling/degradation matches `sms_sender`'s
write-then-buffer-then-pending-buffer chain throughout.

Consistency/addendum audit (step 4, messgr addendum step 2): no `CREATE TABLE`/migration in this
diff (reuses `comms_request`/`comms_event` verbatim) — items 2, 5, 6 (NULL semantics, erasure
statements, new-column readers) don't apply. Item 4 (Vault via `KeyStore`, never an env var) —
confirmed, `OtpProviderCache` never reads a provider key from the environment. Item 7 (grep
against hard invariants 1/3) — no `queue`/`outbox`/gate-chain/verification/suppression/consent
reference anywhere in `src/otp/`. Item 8 (local/CI command parity) — `just lint`/`fmt-check`
match `ci.yml`'s `clippy`/`fmt-check` jobs verbatim. Port `OTP_LISTEN_ADDR` default `8446` and
`OTP_HEALTH_LISTEN_ADDR` default `8085` collide with no other binary's default.

Documentation audit (step 4a): `docs/user-manual/otp-api.adoc` added and registered in
`docs/user-manual.adoc` right after `sms-sender.adoc`; covers env vars, the request/response
shape, the Vault-caching divergence from on-prem, and the mandated cloud-tenant disclosure
(weaker than on-prem, network hop, can keep auth on-prem). `just docs-check` clean.

Docs-readability pass (step 4b): conscious skip — no docs-readability reviewer available in this
session/host.

Governing-document reconciliation (step 7 / addendum step 5): this branch resolved design-doc
Still Open #12 (per its own Description) but left the doc saying otherwise. Fixed inline, same
review, on the ticket's own branch (commit `9843746`):
`development/design/14-decisions-and-open-questions.md` — added decision 33 recording the
resolution, struck through Still Open #12, and corrected Still Open #9's now-false "`otp-api`
... is unbuilt" clause. `docs/user-manual/sms-sender.adoc` and `docs/user-manual/kill-switches.adoc`
both described `otp-api` as unbuilt/not-built-yet; both now cross-reference the shipped
`messgr-otp`.

Impact sweep (step 8): no ticket in `1-to-do/`/`2-ready/` lists T-056 in `depends-on:`. T-060
(second region) references it as a soft coupling only (already noted in this ticket's own
pickup-audit History line) — no action needed here.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | stale-xref | fixed inline | `14-decisions-and-open-questions.md` Still Open #12 said the cloud-OTP-posture question was still open after this ticket resolved it | `development/design/14-decisions-and-open-questions.md` (pre-fix) | struck through, added decision 33 |
| F2 | non-blocking | stale-xref | fixed inline | Still Open #9's clause claimed `otp-api` "is unbuilt and out of this ticket's scope" | same file | corrected to record T-056's `messgr-otp` as the second `auth_enabled` reader |
| F3 | non-blocking | stale-xref | fixed inline | `sms-sender.adoc` described cloud's `otp-api` as unbuilt (Still Open #12) | `docs/user-manual/sms-sender.adoc:13-14` (pre-fix) | now cross-references "messgr-otp" |
| F4 | non-blocking | stale-xref | fixed inline | `kill-switches.adoc` said `otp-api` "is not built yet" | `docs/user-manual/kill-switches.adoc:94` (pre-fix) | now cross-references "messgr-otp" |
| F5 | non-blocking | test-gap | noted | `tests/query_api.rs::producers_usage_and_quota_are_comms_ops_only` failed once in the full suite, passed in isolation — a pre-existing minute-boundary-sensitive assertion from T-048, not touched by this branch | full `just test` run vs. isolated `cargo test --test query_api producers_usage_and_quota_are_comms_ops_only` | not this ticket's scope; a later reviewer can promote if it recurs |

Disposition summary: 4 fixed inline (F1–F4, all governing-document/docs staleness this branch
caused), 1 noted (F5, pre-existing unrelated flake). No blocking findings.

cost: estimated M, actual M

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed cloud OTP posture (design-doc still-open item #12: build messgr-otp,
  do not defer it in favour of an on-prem-only recommendation)
- 2026-09-22 — TO DO → READY: plan complete: new messgr-otp binary/module reusing sms_sender's identity/audit/buffer shape; new at-startup+background-timer provider-credential cache per §3.1 correction (not sms_sender's per-request ProviderConfigCache)
- 2026-09-22 — plan amended inline: pickup applicability audit found decision 4 cited a false
  precedent (claimed `AUTH_FLAG_POLL_INTERVAL` = 60s; it is actually 5s, and no existing
  refresh loop in the codebase uses 60s — `KILL_SWITCH_POLL_INTERVAL`/`QUOTA_CONFIG_POLL_INTERVAL`
  are 30s). Kept 60s on its own merits (credential rotation is less latency-sensitive than an
  auth kill switch) and reworded to stop citing it as an existing-code precedent. Audit also
  flagged, informational only, that T-060's region-wiring plan will need to add `messgr-otp` as
  a 7th binary once this lands — no action here, noted for T-060's own pickup.
- 2026-09-22 — READY → IN DEVELOPMENT: picked up
- 2026-09-22 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-22 — IN REVIEW → DONE: verified: implementation matches plan exactly (decisions 1-6 confirmed, verbatim-reuse claim spot-checked), build/lint/test/docs clean (tests/otp.rs 3/3, mutation-test assertion present); 4 non-blocking findings fixed inline (design-doc + docs-tree staleness this branch caused), 1 noted (pre-existing unrelated test flake); no blocking findings
