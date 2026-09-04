---
id: T-023
title: Read provider credentials from Vault instead of environment variables
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-023 — Read provider credentials from Vault instead of environment variables

## Outcome

`messgr-dispatcher` resolves each channel's provider credential from `provider_config.credential_path` against Vault, matching §13's "all secrets come from Vault — none in config files, none in environment variables". `DISPATCHER_<CH>_API_KEY` environment variables are no longer read anywhere, and `provider_config` (shipped in T-012, unread since) has a first real reader. Reading it actually works end to end: the tenant's own scoped AppRole can read its own provider secret and is rejected reading another tenant's, mirroring the isolation `tests/tenant_vault.rs` already proves for Transit.

## Description

`src/bin/dispatcher.rs` currently builds each channel's `HttpSender` from `env_var("DISPATCHER_{CH}_API_KEY")`. This was an explicit, acknowledged deferral at the time — `src/sender/http.rs::HttpSender::new`'s own doc comment says so, citing "T-012 decision 3" — not a silent regression, but it is still the one place the shipped system violates §13's secrets rule, and `provider_config` (channel, priority, provider, `credential_path`, rate limit) has existed since T-012 with nothing reading `credential_path` at all.

Scope:

1. `messgr-dispatcher` startup queries `provider_config` for the tenant's channel/priority list (the "ordered list even at length 1" §12.1 already requires) and resolves each entry's `credential_path` via the tenant's `KeyStore`/Vault mount (`src/keystore.rs`, wired since T-004/T-008), rather than an env var per channel.
2. `HttpSender::new`'s signature stays the same (`api_key: String`) — the caller now sources that string from Vault instead of the environment; no change needed to the `Sender` trait itself.
3. Remove the `DISPATCHER_<CH>_API_KEY` environment-variable path from `src/bin/dispatcher.rs` once the Vault path is proven; keep the direct-string constructor for tests (already used throughout `src/sender/http.rs`'s test module with a literal `"test-key"`). `scripts/e2e.sh` also sets these two env vars for its dispatcher run — replace them with real Vault KV writes at the same `secret/data/<slug>/<channel>` paths `just provider-config-set` already configures there.
4. **Resolved at refinement: real failover-on-priority is out of scope.** DESIGN.md §12.1 states plainly that the launch mitigations for single-provider risk are "minimum mitigations..., none of which require building failover now" — the ordered-list schema exists precisely so a second provider is a config change later, not something this ticket wires up. This ticket resolves only the highest-priority (`ORDER BY priority` ascending, first row) `provider_config` entry per channel.
5. **Found at refinement: the tenant AppRole has no Vault permission to read anything this ticket needs.** `tenant/vault.rs::policy_hcl_for` (T-004, already `6-done/`) grants only `create`/`update` on the tenant's own Transit paths — no `read` capability exists anywhere on the KV mount `credential_path` points into (Vault's default `secret/` engine, auto-mounted as KV v2 by dev-mode Vault, confirmed already in use by `scripts/e2e.sh`'s `secret/data/$TENANT_SLUG/<channel>` paths). Without a policy change, every dispatcher startup would get Vault `permission denied`. In scope for this ticket (user-confirmed): extend `policy_hcl_for` to also grant `read` on `secret/data/<tenant_slug>/*`, scoped per-tenant like the existing Transit paths. New tenants get it automatically via `provision_tenant`; no backfill exists for already-provisioned tenants (dev/test only, no production data yet — same reasoning as T-022's in-place migrations).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-023-read-provider-credentials-from-vault-instead-of-environment-variables
```

Local WIP commits as you go. Do not push or open a merge request without explicit user
approval (publish-gated, root-path child — tidy WIP into atomic commits before presenting,
rules §0/§4 item 7).

### Prerequisite gate (hard)

None. `depends-on: []`.

### Confirmed design decisions (do not deviate without asking)

1. **`credential_path` is the raw Vault HTTP-API KV v2 form `<mount>/data/<path>`** — e.g.
   `secret/data/acme/sms`, exactly what `scripts/e2e.sh`'s existing
   `just provider-config-set ... "secret/data/$TENANT_SLUG/sms" ...` calls already write, and
   what the CLI test/docs example already shows. Do not change this convention or its existing
   examples. `vaultrs::kv2::read(client, mount, path)` computes `<mount>/data/<path>` itself, so
   the reader must split `credential_path` on its first `/data/` segment into `(mount, path)`
   before calling it — passing the whole string as `path` would double the `data/` segment.
2. **The KV mount is Vault's default `secret/` engine** (dev-mode Vault auto-mounts it as KV v2;
   no `vault-dev-init`/CI bootstrap change needed) — not a per-tenant mount like Transit.
   Tenant isolation comes entirely from the path prefix (`secret/data/<tenant_slug>/...`) plus
   the scoped AppRole policy (decision 4), not from mount separation.
3. **The secret at `credential_path` is a JSON object with one field, `api_key`** — e.g.
   `{"api_key": "..."}`, written out-of-band by an operator (`vault kv put` or the raw HTTP API,
   same style as `just vault-dev-init`'s `curl` calls) after `provider-config set` stores the
   pointer. `messgr-control provider-config set` never touches the secret value itself — matches
   the schema comment "Vault path, never the credential itself."
4. **`policy_hcl_for` (`src/tenant/vault.rs`) gains a `tenant_slug` parameter** alongside its
   existing `mount_path`, and grants `read` on `secret/data/<tenant_slug>/*` in addition to its
   existing two Transit paths — least-privilege, matching the existing pattern (exactly the
   paths needed, no wildcard on Transit, a narrow wildcard here because provider paths are
   per-channel and open-ended).
5. **`read_provider_credential` is a new inherent method on `VaultKeyStore`, not a new `KeyStore`
   trait method.** `KeyStore`'s own doc comment calls it "deliberately narrow: two data-plane
   operations" (DEK-specific); this is a different concern (a generic KV secret read), so it
   follows `.client()`'s existing precedent — an inherent method for callers that need more than
   the narrow trait, called on the concrete `VaultKeyStore` *before* it is wrapped in
   `Arc<dyn KeyStore>` in `dispatcher.rs::main`.
6. **Only the highest-priority `provider_config` row per channel is resolved** (`repo::list`'s
   existing `ORDER BY priority`, first element) — no failover-on-priority (Description item 4).
   A channel with zero `provider_config` rows panics at dispatcher startup with a clear message,
   matching this file's existing `env_var`/`.expect()` misconfiguration-panic style.

### Tasks

#### Task 1 — Vault KV credential read (`src/keystore.rs`) + tenant AppRole policy (`src/tenant/vault.rs`)

In `src/keystore.rs`:
- Add a small free function `pub fn split_kv_path(path: &str) -> Option<(&str, &str)>` that
  splits `credential_path` on the first `/data/` into `(mount, path)`, returning `None` if the
  literal isn't present.
- Add an inherent method on `VaultKeyStore`:
  ```rust
  pub async fn read_provider_credential(
      &self,
      mount: &str,
      path: &str,
  ) -> Result<String, KeyStoreError> {
      #[derive(serde::Deserialize)]
      struct ProviderCredential { api_key: String }
      let secret: ProviderCredential = vaultrs::kv2::read(&self.client, mount, path).await?;
      Ok(secret.api_key)
  }
  ```
  (decisions 2, 3, 5).

In `src/tenant/vault.rs`:
- Change `policy_hcl_for(mount_path: &str)` to `policy_hcl_for(mount_path: &str, tenant_slug: &str)`,
  appending a third HCL block:
  `path "secret/data/{tenant_slug}/*" {{\n  capabilities = ["read"]\n}}\n`.
- Update `provision_vault`'s call site to pass `tenant_slug` through.
- Update the existing `policy_hcl_names_exactly_the_two_keystore_paths` test — rename it and add
  an assertion that the new path/capability is present (still asserting no `create`/`update` on
  the KV path, only `read`).

#### Task 2 — `messgr-dispatcher` credential resolution (`src/bin/dispatcher.rs`)

- Add `use messgr::provider_config::repo as provider_config_repo;` and extend the existing
  `use messgr::keystore::{KeyStore, VaultKeyStore};` to
  `use messgr::keystore::{KeyStore, VaultKeyStore, split_kv_path};`.
- Reorder the existing keystore construction: bind the concrete `VaultKeyStore` to a local
  (`let vault_keystore = VaultKeyStore::connect_as_tenant(config.profile).await.expect(...)`)
  and defer building `let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore);` (note: the
  concrete value is moved here, so do credential resolution first) until *after* the per-channel
  loop below.
- In the per-channel loop, replace `let api_key = env_var(&format!("DISPATCHER_{upper}_API_KEY"));`
  with:
  ```rust
  let configs = provider_config_repo::list(&tenant_pool, channel)
      .await
      .expect("loading provider_config failed");
  let top = configs.first().unwrap_or_else(|| {
      panic!("no provider_config row for channel {channel:?} (tenant {tenant_slug:?})")
  });
  let (kv_mount, kv_path) = split_kv_path(&top.credential_path).unwrap_or_else(|| {
      panic!(
          "provider_config.credential_path {:?} for channel {channel:?} is not in \
           <mount>/data/<path> form",
          top.credential_path
      )
  });
  let api_key = vault_keystore
      .read_provider_credential(kv_mount, kv_path)
      .await
      .unwrap_or_else(|err| {
          panic!(
              "reading Vault credential at {:?} for channel {channel:?} failed: {err}",
              top.credential_path
          )
      });
  ```
  Keep `base_url` unchanged — it is not a secret and stays env-var-sourced, per Description item 2.

#### Task 3 — `scripts/e2e.sh`

- Before starting `messgr-dispatcher`, write the two provider secrets to Vault KV (root token,
  same style as `just vault-dev-init`'s `curl` calls), matching the paths `provider-config-set`
  already configures:
  ```bash
  curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
      --data '{"data":{"api_key":"dev-key"}}' \
      "http://localhost:8200/v1/secret/data/$TENANT_SLUG/sms"
  curl -sf --header "X-Vault-Token: messgr-dev-root-token" --request POST \
      --data '{"data":{"api_key":"dev-key"}}' \
      "http://localhost:8200/v1/secret/data/$TENANT_SLUG/email"
  ```
- Remove the `DISPATCHER_SMS_API_KEY="dev-key"` and `DISPATCHER_EMAIL_API_KEY="dev-key"` lines
  from the `messgr-dispatcher` startup env block.

#### Task 4 — Tests

- `src/tenant/vault.rs`: update/rename the existing policy-shape unit test per Task 1.
- `tests/tenant_vault.rs`: add a new test, `tenant_a_vault_credentials_can_read_its_own_provider_secret_but_not_tenant_bs`,
  following `tenant_a_vault_credentials_cannot_read_tenant_bs_dek`'s exact structure: provision
  two tenants, write a KV secret at `secret/data/<slug_a>/sms` via the **admin** client
  (`vaultrs::kv2::set`), confirm tenant A's scoped login (`login_as_tenant`) can read it via
  `VaultKeyStore::read_provider_credential`, and confirm the same scoped client is rejected
  reading `secret/data/<slug_b>/sms` (permission denied, not "not found") — then confirm the
  admin client can still reach it directly, same mutation-testing standard the existing test
  uses.
- No changes needed to `tests/dispatcher.rs` (it builds `HttpSender`/`DispatcherContext`
  directly, never through `main`) or `src/sender/http.rs`'s tests (unaffected — decision in
  Description item 2).

### Acceptance test

```
just build
just lint
cargo test --test tenant_vault
cargo test --lib tenant::vault
just test
just docs-check
```

All must pass clean, including the new `tests/tenant_vault.rs` isolation test from Task 4. As a
manual end-to-end check, run the updated `scripts/e2e.sh 2-setup-env` and `3-send-sms` and
confirm `messgr-dispatcher`'s log shows no `DISPATCHER_SMS_API_KEY`/`DISPATCHER_EMAIL_API_KEY`
reads and the message reaches `final_status = sent`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/control-plane-cli.adoc`: update the "Provider configuration" section —
  `credential_path` is now read by `messgr-dispatcher` at startup (remove "not yet read by
  anything"); note the `secret/data/<slug>/<channel>` KV convention and the `{"api_key": "..."}`
  secret shape explicitly.
- `docs/user-manual/dispatcher.adoc`: update the "Requires ..." paragraph — remove
  `DISPATCHER_<CHANNEL>_API_KEY` from the required environment variables and the worked example;
  add a short note that the credential now comes from `provider_config.credential_path` via
  Vault KV, with a pointer to how to seed it locally (Task 3's `curl` form).
- `src/bin/control.rs`'s `ProviderConfig` doc comment and the `credential_path` arg's doc comment
  ("stored but not yet read") — update to reflect the new reader.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. Docs updated per the Docs update step.
3. Write a summary: files touched, decisions honoured (especially decisions 4/5 — the Vault
   policy expansion touching T-004's provisioning code), anything deferred.
4. Suggest a Conventional Commit message, e.g.:
   ```
   feat(dispatcher): read provider credentials from Vault KV (T-023)
   ```
5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits before
   presenting (root-path child, rules §0/§4 item 7) — default to preserving them on merge
   (rebase/keep-history) rather than squashing.
6. Commit locally on the ticket branch. Publish only per commit policy (no push/MR without
   explicit user approval). Under `layout = "in-tree"`, before pushing verify the remote base is
   not behind (`git fetch origin main && git diff --name-only origin/main...HEAD | grep
   '^tickets/'` must print nothing) — then push and open the merge request. Hand back to the
   user.

## Review

- [x] Reviewer independence settled (step 0): reviewing session had no hand in this branch (fresh
  session, post-`/clear`) — already independent. Audits (steps 1-4a) still delegated to a fresh,
  worktree-isolated sub-agent briefed adversarially, for context isolation and a heavier read of
  the diff; every delegated finding below was re-verified by hand against the actual files before
  being recorded here (step 0's "delegation buys independence, not accuracy").
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2): all
  6 commands (`just build`, `just lint`, `cargo test --test tenant_vault`, `cargo test --lib
  tenant::vault`, `just test`, `just docs-check`) pass clean on live Postgres + dev Vault. Every
  task (1-4) and confirmed decision (1-6) verified done exactly as specified, in the files named
  — no deviations. Manual e2e check corroborated via a live `scripts/e2e.sh` run's
  `dispatcher.log` (clean startup, no missing-env-var panic) and `comms_request` (2111 rows
  `final_status = sent`, 0 left in `outbox`) rather than a fresh script run of my own (a collision
  with an already-running e2e walkthrough against the same Docker daemon), noted rather than
  silently assumed.
- [x] Quality audit (step 3): idiomatic, matches existing panic-on-misconfiguration style. The new
  isolation test (`tenant_a_vault_credentials_can_read_its_own_provider_secret_but_not_tenant_bs`)
  mutation-tested per the addendum's standard — narrowing `policy_hcl_for`'s new KV path from
  `secret/data/{tenant_slug}/*` to `secret/data/*` turned it red with the expected
  permission-denied message; reverting turned it green again. Genuinely load-bearing, not a
  vacuous `is_err()`.
- [x] Consistency audit (step 4): found 3 stale doc-comment/config references this branch made
  false (F1-F3) and 1 wording mismatch in newly-authored docs (F4) — all fixed inline, see table.
  `policy_hcl_for`'s one call site (`provision_vault`) and `read_provider_credential`'s one call
  site (`dispatcher.rs`) both updated consistently; no other stale `DISPATCHER_<CH>_API_KEY`
  references remain in the tree after F1-F3's fixes.
- [x] Documentation audit (step 4a): `docs/user-manual/control-plane-cli.adoc` and
  `dispatcher.adoc` updated as specified and accurate against the new code (post F4 fix);
  `src/bin/control.rs`'s doc comments updated. `just docs-check` clean.
- [x] Docs-readability pass (step 4b): no docs-readability reviewer configured in this host —
  conscious skip.
- [x] Findings recorded with severity, class, disposition (step 5): see table below.
- [x] Ticket moved (step 6): no blocking findings → `tickets/6-done/`.
- [x] Other references / governing documents reconciled (step 7): grepped `DESIGN.md` and
  `development/design/*.md` for `credential_path`/`DISPATCHER_*_API_KEY`/Vault-secrets
  cross-references — none stale; decision #5 in
  `development/design/14-decisions-and-open-questions.md` already correctly states Vault owns
  provider credentials, and needed no edit. `development/design/11-failure-modes.md:39`'s
  "SMS provider config is hot-reloadable" claim remains unimplemented (config is read once at
  dispatcher startup) — see F5; this predates T-023 (`provider_config` had zero readers before
  this ticket, so the claim was equally unimplemented either side of this branch) and is not
  something this branch broke, so it is `noted`, not `fixed inline`.
- [x] Remaining-tickets impact sweep (step 8): re-read `T-024` (CI erasure-coverage check) and
  `T-025` (operability/error-handling cleanup) in `2-ready/`/`1-to-do/` — neither depends on or
  references T-023; no assumption invalidated.
- [x] Summary + commit message & MR attributes presented for approval (step 9): below.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | stale-xref | fixed inline | `HttpSender::new`'s doc comment still said Vault credential resolution was deferred to a later ticket with "no KV-secret-read plumbing" — that plumbing now exists and is wired from `dispatcher.rs` | `src/sender/http.rs:19-21` (pre-fix) | fixed inline: doc comment now points at the actual caller/mechanism |
| F2 | non-blocking | stale-xref | fixed inline | `dispatcher-run`'s justfile comment still listed `DISPATCHER_<CHANNEL>_API_KEY` as required; it's no longer read | `justfile:231-234` (pre-fix) | fixed inline: comment now names Vault KV as the credential source |
| F3 | non-blocking | stale-xref | fixed inline | `.env.example` — the file a developer actually copies to run the dispatcher locally — still documented and set `DISPATCHER_SMS_API_KEY=dev-key`; untouched by this branch's diff | `.env.example:13-22` (pre-fix) | fixed inline: comment updated, `_API_KEY` line removed |
| F4 | non-blocking | spec-unclear | fixed inline | `control-plane-cli.adoc`'s new failover paragraph called the descope "Still Open," but the ticket's own Description item 4 phrases it as a made decision ("Resolved at refinement... out of scope") — cosmetic mismatch, no functional impact | `docs/user-manual/control-plane-cli.adoc:186-187` (pre-fix) | fixed inline: wording aligned with Description item 4 |
| F5 | non-blocking | design | noted | `development/design/11-failure-modes.md:39` claims SMS provider config is hot-reloadable without a deploy/restart; `provider_config` is in fact read once at dispatcher startup (this ticket gave it its first reader at all, still a one-time read) — a real gap against the OTP-cutover story in §12.1, but pre-existing and not introduced by this branch | `development/design/11-failure-modes.md:39`; `src/bin/dispatcher.rs:154-182` | noted: a future ticket implementing config hot-reload should also close this |

Disposition summary: 4 fixed inline (F1-F4), 1 noted (F5). 0 folded, 0 new tickets.

cost: estimated M, actual M

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found messgr-dispatcher reads DISPATCHER_<CH>_API_KEY from the environment, violating §13's Vault-only secrets rule; provider_config.credential_path has had no reader since T-012.
- 2026-09-04 — TO DO → READY: implementation plan complete. Expanded at the user's direction during refinement to also fix the tenant AppRole's missing Vault KV read permission (T-004's `policy_hcl_for` had no path for this at all) — found while confirming the dispatcher's scoped login could actually reach `credential_path` end to end.
- 2026-09-04 — TO DO → READY: plan complete
- 2026-09-04 — READY → IN DEVELOPMENT: picked up
- 2026-09-04 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-04 — IN REVIEW → DONE: review clean: no blocking findings, 4 stale-xref/spec-unclear findings fixed inline, 1 noted (F5)
- 2026-09-04 — pushed, PR #35 opened (https://github.com/umbrella-org/messgr/pull/35). Not yet merged.
- 2026-09-04 — MERGED: PR #35 merged to main (28e0b3a).
