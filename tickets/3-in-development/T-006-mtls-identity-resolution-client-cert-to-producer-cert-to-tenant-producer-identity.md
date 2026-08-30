---
id: T-006
title: mTLS identity resolution: client cert to producer_cert to tenant/producer identity
project: messgr
depends-on: [T-005]
spawned-by: []
family: T-005
impact: medium
complexity: medium
cost: M
---

# T-006 — mTLS identity resolution: client cert to producer_cert to tenant/producer identity

## Outcome

Every future ingest path can resolve a client certificate to `(tenant_id, producer_id)` with a
single control-database query, before any tenant database is touched: `resolve_producer` looks
up `cert_subject` in `producer_cert`, and an unknown cert vs. a known-but-disabled producer come
back as distinguishable errors. In dev, `messgr-control`'s new `dev-pki` subcommands issue real
client certificates from Vault's PKI secrets engine to exercise that path without a real CA. No
ingest binary exists yet (that is T-011) — this ticket ships the shared resolution layer and the
dev tooling, ready for that binary to compose.

## Description

Reads what T-005 writes. T-005 shipped both halves of producer identity — the tenant-side
`producer` row and the control-database `producer_cert` mapping — but nothing yet consumes
`producer_cert` at request time; this ticket is that first reader (§4.9, §11.1, §2.2).

**Resolution stays entirely inside the control database — it never opens a tenant pool.** §4.11
says so directly: "mTLS producer certs resolve to a tenant BEFORE any tenant database is
opened." But `producer_cert` as T-005 shipped it (and as §4.11 currently documents it) carries
no `enabled` column — only the tenant-side `producer` row does — so telling "unknown cert" from
"known but disabled" apart would otherwise require opening the tenant database mid-resolution,
contradicting that stated ordering and putting a tenant-DB connection on the admission path
before a request is even let in. This ticket closes that gap by **denormalizing `enabled` onto
`producer_cert`** (control DB, migration `0003`), making resolution a single-query, tenant-DB-
independent operation, and updates §4.11's schema to match — a refinement decided now, not a
correction discovered later. `disable_producer` (T-005) is extended to write both copies, and
the crash-window ordering T-005 decision 2 established for *register* is inverted for *disable*:
see confirmed decision 2 below.

Resolution: client cert subject (CN/SAN, presented to this layer as a plain string — extracting
it from an actual TLS handshake is T-011's job, not this ticket's) → look up `cert_subject` in
`producer_cert` → `(tenant_id, producer_id)`, or a distinguishable `UnknownCert` /
`Disabled { tenant_id, producer_id }` rejection. Built as a shared layer (`src/producer/resolve.rs`)
so every future ingest binary composes it rather than re-implementing cert lookup.

Producer identity is never read from the request body (§11, §4.9) — this ticket is where that
rule is enforced structurally: `resolve_producer`'s only input is the cert subject string, so no
caller can thread an asserted `tenant_id`/`producer_id` through it. Every later ingest ticket
inherits this rather than re-asserting the rule.

Also in scope: internal PKI issuance for dev, so the resolution path has real certificates to
test against without standing up an external CA. Vault already runs in dev (Compose, T-003) and
already carries this project's only Vault dependency (`vaultrs`, which supports the PKI secrets
engine); production is described only in passing as using "Vault ... internal PKI" (DESIGN.md,
build order table row 5) with no engine detail, so dev reuses Vault's PKI engine rather than
inventing a second, divergent internal-CA mechanism. Gated behind the same
refuse-to-run-outside-`profile=dev` pattern as `MockProvider` (§11.1) and dev-mode Vault (§7.6):
minting client certificates on demand must not be reachable against a real Vault.

Explicitly out of scope: wiring resolution into an actual TLS-terminating edge (no such binary
exists yet — T-011 is where `messgr-ingest` first exists and first calls this layer); tenant
status (suspended/offboarding) as a resolution input — that is a separate future gate, not
conflated with cert resolution here; per-tenant pool caching for the request path — resolution
itself never opens a tenant pool at all now, and whether/how a future ingest binary caches tenant
pools for the rest of a request is that ticket's decision, not this one's.

Same family as T-005 (build step 1, "Producer registry and mTLS identity", §4.9/§11.1) — T-005
is the write side, this is the read side; together they are the step's whole outcome.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-006-mtls-identity-resolution
```

This child is root-path (`path = "."`, pickle.toml), so WIP commits are encouraged during the
work and then interactive-rebased into atomic, correctly scoped commits before the summary is
presented (rules §0). Do not push and do not open a merge request without explicit user
approval. Ticket and board bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-005` is in `6-done/` and merged to `main` (PR #5, `12ec4581`) — confirmed.
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init`. The
  integration tests provision real tenants, register real producers, and (new in this ticket)
  bootstrap a real Vault PKI mount, so all three must be up.

### Confirmed design decisions (do not deviate without asking)

1. **`producer_cert` gets its own `enabled` column (control DB); resolution reads only this
   column and never opens a tenant pool.** §4.11 states resolution happens "BEFORE any tenant
   database is opened" — T-005's `producer_cert` has no `enabled` column, so honouring that
   ordering while still distinguishing "unknown" from "disabled" requires a denormalized copy.
   `migrations/control/0003_producer_cert_enabled.sql` adds it, `NOT NULL DEFAULT true` (so
   every already-registered producer_cert row from T-005 stays enabled unless explicitly
   disabled). DESIGN.md §4.11's `producer_cert` SQL block is updated to match, as a refinement
   made now rather than a correction discovered later — say so in the docs step, not silently.

2. **Disable writes control-first, then tenant — the inverse of T-005 decision 2's register
   ordering, because the *authoritative-for-admission* copy has moved.** T-005 decision 2 wrote
   the tenant `producer` row before the control `producer_cert` row for register, because a
   crash between the two must not leave a cert that resolves to a producer that doesn't exist.
   Disable's safety direction is the opposite: the copy resolution now actually reads is
   `producer_cert.enabled` (decision 1), so a crash between the two disable writes must leave
   the *fail-closed* side landed first. `disable_producer_inner` therefore calls
   `cert_repo::set_cert_enabled(control_pool, cert_subject, false)` **before**
   `repo::set_enabled(tenant_pool, existing.id, false)`. A crash in between leaves resolution
   already rejecting the producer and the tenant-side `producer.enabled` flag (cosmetic for
   `producer list`) briefly stale-true — repaired by re-running disable, same as T-005's
   idempotent-repair pattern.

3. **`cert_repo::upsert_producer_cert`'s `ON CONFLICT` clause must not touch `enabled`.** It is
   called by `register_producer`'s idempotent-reconfirm path (T-005 decision 3), which has no
   business re-enabling a disabled producer. Only the `INSERT`'s column default (`true`) and
   `set_cert_enabled` (decision 2) may ever set this column. Get this wrong and a duplicate
   `register` call with identical inputs silently un-disables a producer — write the regression
   test in task 8 to lock this down, not just the happy path.

4. **`resolve_producer`'s only parameter is the cert subject string** (`&PgPool`, `&str`) — no
   `tenant_id`/`producer_id`/tenant slug parameter exists for a caller to pass instead. This is
   the structural enforcement of "producer identity is never read from the request body"
   (§11, §4.9): there is nothing in this function's signature a body-derived value could be
   smuggled through.

5. **Two distinct rejections, never collapsed into one.** `ResolutionError::UnknownCert` (no
   `producer_cert` row) vs. `ResolutionError::Disabled { tenant_id, producer_id }` (row exists,
   `enabled = false`) — T-005 decision 5 kept the row on disable specifically so this
   distinction is possible; collapsing them back into one generic rejection here would waste
   that.

6. **Dev PKI is gated behind `profile = dev`, refusing to run otherwise, in the same commit that
   introduces it** — the same non-negotiable pattern as `MockProvider` (§11.1) and dev-mode Vault
   (§7.6, `keystore::assert_tls_outside_dev`). A `dev-pki` subcommand reachable against a real
   Vault would let an operator mint a trusted producer client certificate outside the
   registration/audit path entirely. Reuse `keystore::connect_client(profile)` (already asserts
   TLS outside dev) for the Vault connection, and add an explicit `profile.is_dev()` check on
   top of it before either `dev-pki` subcommand does anything — the TLS guard alone does not
   stop a non-dev *dev-mode* Vault instance someone left reachable.

7. **Dev PKI mount and role are cluster-wide, one-time bootstrap state, not per-tenant** — unlike
   the per-tenant Transit mounts T-004 creates. One `pki` mount, one root CA, one role
   (`allow_any_name = true`, `enforce_hostnames = false`, since `cert_subject` values are opaque
   identifiers like `CN=fraud-alerts.internal`, not real DNS domains) shared by every producer
   cert issued for dev testing.

### Tasks

#### Task 1 — `producer_cert.enabled` migration

Add `migrations/control/0003_producer_cert_enabled.sql`, following `0002_tenant_vault_role_id.sql`'s
comment style:

```sql
-- Denormalizes producer.enabled (tenant DB) onto producer_cert (control DB), so mTLS
-- resolution (DESIGN.md §4.9, §11.1, T-006) can distinguish "unknown cert" from "known but
-- disabled" with a single control-database query, honouring §4.11's stated ordering that
-- resolution happens before any tenant database is opened. Kept in sync by
-- src/producer/register.rs's disable_producer (control-first write — T-006 decision 2).
ALTER TABLE producer_cert ADD COLUMN enabled bool NOT NULL DEFAULT true;
```

#### Task 2 — `cert_repo` changes

In `src/producer/cert_repo.rs`:

- Add `enabled: bool` to `ProducerCert` and to `find_producer_cert`'s `SELECT` list.
- Add `set_cert_enabled(pool: &PgPool, cert_subject: &str, enabled: bool) -> Result<(), sqlx::Error>`
  (`UPDATE producer_cert SET enabled = $1 WHERE cert_subject = $2`), doc-commented as the only
  writer of this column besides the insert default, per decision 3.
- Confirm `upsert_producer_cert`'s `ON CONFLICT ... DO UPDATE SET` still lists only `tenant_id`
  and `producer_id` — do not add `enabled` to it (decision 3).

#### Task 3 — `disable_producer` writes both copies, control-first

In `src/producer/register.rs`'s `disable_producer_inner`: call
`cert_repo::set_cert_enabled(control_pool, &existing.cert_subject, false)` before
`repo::set_enabled(tenant_pool, existing.id, false)`, on both the first-disable and the
already-disabled idempotent branches (decision 2). No change to `register_producer`'s ordering
or to the `platform_audit` shape (T-005 decision 4 is untouched).

#### Task 4 — `resolve_producer` shared layer

Add `src/producer/resolve.rs`, registered as `pub mod resolve;` in `src/producer/mod.rs`:

```rust
pub struct ResolvedIdentity {
    pub tenant_id: Uuid,
    pub producer_id: Uuid,
}

pub enum ResolutionError {
    UnknownCert,
    Disabled { tenant_id: Uuid, producer_id: Uuid },
    Database(sqlx::Error),
}
```

`resolve_producer(control_pool: &PgPool, cert_subject: &str) -> Result<ResolvedIdentity, ResolutionError>`:
call `cert_repo::find_producer_cert`; `None` → `UnknownCert`; `Some(cert)` with `!cert.enabled` →
`Disabled { tenant_id: cert.tenant_id, producer_id: cert.producer_id }`; else →
`Ok(ResolvedIdentity { tenant_id: cert.tenant_id, producer_id: cert.producer_id })`. Give
`ResolutionError` `Display`/`Error`/`From<sqlx::Error>` impls in the shape of `ProducerError`
(`src/producer/register.rs`).

#### Task 5 — Dev PKI: bootstrap and issuance

Add `src/producer/dev_pki.rs`, registered as `pub mod dev_pki;`:

- A private `assert_dev_profile(profile: Profile)` guard (panics with a message naming the
  offending profile if not `Profile::Dev`), called first by both functions below (decision 6).
- `bootstrap(client: &VaultClient, profile: Profile) -> Result<(), KeyStoreError>`: idempotently
  ensures a `pki` mount exists (list `sys/mounts`, same pattern as `tenant::vault::ensure_transit_mount`,
  `mount::enable(client, "pki", "pki", None)` if absent), generates a root CA if the mount has
  none yet (`pki::cert::ca::generate(client, "pki", "internal", Some(builder.common_name("messgr
  dev root")))` — check via `pki::issuer::list` first, since re-generating a root when one exists
  is not what "idempotent" should mean here), and upserts a `producer-dev` role
  (`pki::role::set(client, "pki", "producer-dev", Some(builder.allow_any_name(true).enforce_hostnames(false)))`
  — decision 7).
- `issue_cert(client: &VaultClient, profile: Profile, common_name: &str) -> Result<GenerateCertificateResponse, KeyStoreError>`:
  `pki::cert::generate(client, "pki", "producer-dev", Some(builder.common_name(common_name)))`.
- Reuse `crate::keystore::KeyStoreError` as the error type (it already wraps `ClientError`) rather
  than inventing a third error enum for two functions.

#### Task 6 — `messgr-control dev-pki` subcommands

Extend `src/bin/control.rs` with a `DevPki` subcommand group: `Bootstrap` (no args) and
`IssueCert --common-name <name> --out-dir <dir>` (writes `<dir>/cert.pem`, `<dir>/key.pem`,
`<dir>/ca.pem` from the response's `certificate`, `private_key`, and `issuing_ca` fields;
creates `<dir>` if absent). Both connect Vault via `crate::keystore::connect_client(config.profile)`
(same as `Provision`'s Vault connection) and let `dev_pki`'s internal guard reject a non-dev
profile with a clear non-zero exit — do not duplicate the guard check in `control.rs` itself.

#### Task 7 — `justfile` recipes

Add `dev-pki-bootstrap` and `dev-pki-issue-cert common_name out_dir`, mirroring the existing
`producer-register`/`producer-list` recipes' shape.

#### Task 8 — Tests

Extend `tests/producer.rs` (real stack, no mocks, following its existing conventions) with:

1. Register a producer, then `resolve::resolve_producer(&control_pool, &cert_subject)` resolves
   to the correct `(tenant_id, producer_id)`.
2. An unregistered `cert_subject` → `ResolutionError::UnknownCert`.
3. Disable the producer, then resolve → `ResolutionError::Disabled { tenant_id, producer_id }`
   with the correct ids (not `UnknownCert` — this is the whole point of decision 1).
4. **Regression guard for decision 3:** disable a producer, then call `register_producer` again
   with the identical original inputs (the idempotent path) — assert resolution is *still*
   `Disabled` afterwards, proving the idempotent reconfirm did not silently re-enable the
   `producer_cert` row.
5. Extend two-tenant isolation: the same producer `name`/different `cert_subject` registered for
   tenant A and tenant B — resolving tenant B's `cert_subject` never returns tenant A's ids.

Add `tests/dev_pki.rs` (real dev Vault, following `tests/tenant_vault.rs`'s conventions):

1. `dev_pki::bootstrap` succeeds against the dev Vault from `just vault-dev-init` and is
   idempotent (calling it twice does not error and does not mint a second root CA — assert via
   `pki::issuer::list` returning exactly one issuer after two bootstrap calls).
2. `dev_pki::issue_cert` after bootstrap returns a `certificate` starting with
   `-----BEGIN CERTIFICATE-----` and a non-empty `private_key`.
3. `assert_dev_profile` panics when called with `Profile::Production` (a plain unit test, no
   Vault needed — same pattern as `keystore`'s `guard_panics_for_non_https_address_outside_dev`).

### Acceptance test

Run from the repository root with the local stack up:

```
just db-up
just control-migrate
just vault-dev-init
just dev-pki-bootstrap
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/producer.rs and tests/dev_pki.rs
```

Then exercise the dev PKI CLI and a real resolution round-trip end to end:

```
just provision acme eu tenant_acme operator@example.com
just producer-register acme fraud-alerts "CN=fraud-alerts.internal" fraud fraud-oncall@example.com operator@example.com
just dev-pki-issue-cert fraud-alerts.internal /tmp/t006-cert
openssl x509 -noout -subject -in /tmp/t006-cert/cert.pem
```

Expected: the `openssl` output shows `CN=fraud-alerts.internal` (or `CN = fraud-alerts.internal`
depending on OpenSSL version's formatting), confirming the issued certificate really carries the
subject a producer was registered under. Then confirm the resolution layer itself, both branches:

```
just producer-disable acme fraud-alerts operator@example.com
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT cert_subject, enabled FROM producer_cert WHERE cert_subject = 'CN=fraud-alerts.internal'"
```

Expected: `enabled` is `f` immediately after disable (control-first write, decision 2) — confirms
the column T-005's schema lacked is now the one resolution actually reads.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control dev-pki` subcommands, and a schema change to a
table DESIGN.md documents.

- `DESIGN.md` §4.11 — add the `enabled bool NOT NULL DEFAULT true` column to the `producer_cert`
  `CREATE TABLE` block, with a one-line comment pointing at this ticket's reasoning (mirroring
  how §7.6's Transit paragraph already documents its own past correction inline). Note in the
  ticket's Finish summary that this is a deliberate refinement decided at T-006's refinement, not
  a same-pattern "correction on the record" — it does not belong in that section of `AGENTS.md`.
- `README.md` — under "Local development", document `dev-pki bootstrap`/`dev-pki issue-cert`
  alongside `producer register`, including the non-dev refusal and that `producer_cert.enabled`
  is now the column mTLS resolution actually reads (the tenant-side `producer.enabled` remains
  the source of truth `producer list` displays).
- `justfile` — the two new recipes from task 7.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README, justfile, and DESIGN.md §4.11 updated per the docs step.
3. Write a summary: files touched, decisions honoured (especially the register/disable ordering
   inversion), anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(producer): add mTLS identity resolution and dev PKI issuance (T-006)

   Denormalizes producer_cert.enabled (control DB migration 0003) so resolution
   stays a single control-database query and never opens a tenant pool, per
   §4.11. Adds resolve_producer (UnknownCert vs Disabled, distinguishable) as
   the shared layer future ingest binaries will compose, and dev-pki
   bootstrap/issue-cert subcommands backed by Vault's PKI secrets engine,
   gated to profile=dev. disable_producer now writes producer_cert.enabled
   before the tenant-side flag, inverting T-005's register ordering because
   the authoritative-for-admission copy has moved.
   ```

5. Root-path child: interactive-rebase the WIP commits into a small number of atomic, correctly
   scoped commits (migration+cert_repo / disable-ordering fix / resolve layer / dev PKI module+CLI
   / tests / docs is a natural split) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-30 — created (TO DO). source: chat: filed from PLAN.md build-step-1 row; same family as T-005 (producer registry write side / mTLS resolution read side)
- 2026-08-30 — TO DO → READY: plan complete
- 2026-08-30 — READY → IN DEVELOPMENT: picked up
