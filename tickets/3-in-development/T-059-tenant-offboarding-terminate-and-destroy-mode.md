---
id: T-059
title: Tenant offboarding: terminate-and-destroy mode
project: messgr
depends-on: []
spawned-by: [T-054]
impact: critical
complexity: medium
cost: M
---

# T-059 — Tenant offboarding: terminate-and-destroy mode

## Outcome

After this ships, a departing tenant can be fully offboarded via terminate-and-destroy: its
Transit key destroyed, its database dropped, and its AppRoles revoked — O(1), minutes, immediate
unreadability subject only to the §7.3 backup window.

## Description

§7.7: `tenant.status` already carries `offboarding_archive` / `offboarding_destroy` in the schema
(T-001, done), but no tooling enforces either mode yet. This ticket builds **terminate and
destroy** only: destroy the Transit key, `DROP DATABASE`, revoke AppRoles.

**Terminate-and-archive is explicitly out of scope for this ticket.** §7.7 states it "needs
pricing rather than engineering" — design-doc still-open item #13 (is archive commercially
offered, and at what price? it carries multi-year key-custody obligations after the relationship
ends) is unresolved, and per user decision during T-054's refinement this ticket does not assume
an answer. Archive mode (database and Transit key retained for the tenant's remaining retention
period, producers/UI disabled, dispatchers stopped, restricted export-only read access, converts
to destroy after an explicit end date) is a separate, unticketed follow-up once that business
decision lands — do not fold it in here.

Applies hard invariant #7 (`AGENTS.md`, per-customer DEKs from the first write) and #6 (every
table holding customer data appears in erasure statements) at the tenant level rather than the
customer level: this is whole-tenant termination, distinct in scope from T-050's customer-level
crypto-shred/physical-redaction tooling — no duplication, but worth cross-referencing since both
touch the erasure/crypto-shred machinery. Dropping the whole tenant database removes every
customer-linkable table at once, so T-024's per-table erasure-statement coverage (§7.2) is
satisfied trivially here — nothing to add to it for this ticket.

**Inverse of `tenant::provision::provision_tenant` (T-001/T-004, done, `src/tenant/provision.rs`,
`src/tenant/vault.rs`).** Provisioning creates the Transit mount+key+policy+AppRole, then the
database, then runs migrations, then marks the tenant active — destroy runs the same steps in
reverse: destroy the Vault side first (so a database issue can never leave a live decryption key
behind), then the database, then mark the tenant row `offboarding_destroy` (already a legal value
per T-001's schema, `src/tenant/model.rs`'s `status::OFFBOARDING_DESTROY`).

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-059-tenant-offboarding-terminate-and-destroy-mode
```

### Prerequisite gate (hard)

None. T-001/T-004 (`src/tenant/provision.rs`, `src/tenant/vault.rs`) are done and merged; this
ticket only adds new functions alongside them.

### Confirmed design decisions (do not deviate without asking)

1. **Vault destroyed before the database dropped, always in that order.** §7.6: "Destroy the
   tenant's Transit key and every payload and destination in their database becomes permanently
   unreadable, immediately, without touching a single row. `DROP DATABASE` then follows at
   leisure as space reclamation rather than as the security-critical step." Reversing the order
   (dropping the database first) would leave a live key with nothing left to protect but no
   correctness reason to prefer it — keep Vault-first so a failure partway through never leaves
   readable data behind.
2. **`DROP DATABASE ... WITH (FORCE)`** (Postgres 13+; this project runs Postgres 18,
   `compose.yml`), not manual `pg_terminate_backend` — terminates any lingering connections
   atomically as part of the drop, and is the documented, simpler mechanism.
3. **Idempotent the same way `provision_tenant` is: safe to call twice.** A second call against
   an already-`offboarding_destroy` tenant is a no-op that re-confirms rather than erroring — the
   Vault delete/mount-disable/AppRole-delete calls are idempotent in Vault itself (deleting an
   already-gone key/role/mount returns cleanly), and `DROP DATABASE ... IF EXISTS` covers the
   database side.
4. **Writes exactly one `platform_audit` row** (`action = "tenant.offboard_destroy"`), same
   pattern as `provision_tenant`'s own audit write.
5. **This ticket does not stop dispatchers or disable producers first** — unlike the archive mode
   (out of scope, per the Description), destroy's whole point is that the tenant becomes
   unreadable atomically; there is no window where "cleanly draining traffic first" matters, and
   `DROP DATABASE ... WITH (FORCE)` already handles any in-flight connection.

### Tasks

#### Task 1 — `src/tenant/vault.rs`: `destroy_vault`
```rust
pub async fn destroy_vault(client: &VaultClient, tenant_slug: &str) -> Result<(), KeyStoreError>
```
- `transit::key::update(client, &mount_path, KEY_NAME, Some(&mut builder.deletion_allowed(true)))`
  — Transit keys refuse deletion unless `deletion_allowed` is set first.
- `transit::key::delete(client, &mount_path, KEY_NAME)`.
- `mount::disable(client, &mount_path)` — unmount the tenant's Transit engine entirely.
- `policy::delete(client, &policy_name)` (the `tenant-{slug}-transit` policy `provision_vault`
  created).
- `approle_role::delete(client, APPROLE_MOUNT, tenant_slug)`.
Each step tolerates "already gone" (decision 3) — call `vaultrs`'s delete/disable functions and
treat a 404-shaped `ClientError` as success, matching how `ensure_transit_mount` already
special-cases Vault's list-based idempotency check.

#### Task 2 — `src/tenant/provision.rs` (or a new sibling `src/tenant/offboard.rs`): `destroy_tenant`
```rust
pub async fn destroy_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_id: Uuid,
    actor: &str,
    vault_client: &VaultClient,
) -> Result<(), ProvisionError>
```
Looks up the tenant (`repo::find_by_id`), calls `vault::destroy_vault` (task 1), then
`sqlx::query("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)")` against `control_pool` (identifier
must be validated/quoted the same way `ensure_database_exists` already trusts `database_name` —
it comes from the `tenant` row, not user input at this call site), then
`repo::mark_status(control_pool, tenant_id, status::OFFBOARDING_DESTROY)` (add this small helper
to `repo.rs` alongside the existing `mark_active`), then the `platform_audit::record` call
(decision 4).

#### Task 3 — `messgr-control` CLI: `offboard-destroy` subcommand
`Command::OffboardDestroy { tenant_slug: String, actor: String }` in `src/bin/control.rs`,
resolving the slug to a tenant id and calling task 2's `destroy_tenant`. Require a
`--confirm-slug <slug>` flag that must repeat the slug back (a bare `--yes` is too easy to
script past by accident for a destructive, irreversible-in-practice operation) before proceeding.

### Acceptance test

1. `just build && just lint` clean.
2. `just test` green, including:
   - `destroy_vault` against a dev-mode Vault: provision a tenant, destroy it, assert the Transit
     mount no longer appears in `mount::list`, and that a subsequent `datakey`/`decrypt` call
     against the old mount path fails (mutation test: this must go red if `deletion_allowed` is
     never set, since Vault otherwise refuses the delete silently-as-an-error).
   - `destroy_tenant` end to end: provision, insert a customer row, destroy, assert
     `pg_database` no longer lists the database and the `tenant` row reads
     `status::OFFBOARDING_DESTROY`.
   - Idempotency: calling `destroy_tenant` twice does not error.
3. Manual: `messgr-control offboard-destroy --tenant-slug <slug> --confirm-slug <slug> --actor
   <name>` against the dev compose stack; confirm the tenant's database is gone
   (`\l` in `psql`) and its Transit mount is gone (`vault secrets list`).

### Docs update (mandatory when user-facing)

Add an "Offboarding" section to `docs/user-manual/control-plane-cli.adoc`: the
`offboard-destroy` subcommand, its irreversibility, the §7.3 backup-window caveat (data is
unreadable immediately but backups containing the old wrapped DEK/key material clear only after
the backup retention window elapses — still-open item #1), and that terminate-and-archive is not
yet built (cross-reference the Description). Run `just docs-check`.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated and registered.
3. Write a summary (files touched, decisions made, anything deferred — archive mode explicitly
   out of scope) and hand back for review.
4. Suggested commit message: `feat(tenant): terminate-and-destroy offboarding mode (T-059)`.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting and confirmed scope (design-doc still-open item #13: destroy mode only, defer
  terminate-and-archive until pricing is decided)
- 2026-09-22 — TO DO → READY: plan complete: destroy_vault/destroy_tenant as the inverse of T-001/T-004's provision path, DROP DATABASE ... WITH (FORCE) on Postgres 18; archive mode confirmed out of scope pending pricing (item #13)
- 2026-09-22 — READY → IN DEVELOPMENT: picked up; applicability gate passed (1 non-blocking drift: Task 1's stated idempotency precedent doesn't match ensure_transit_mount's actual list-check approach — note-and-close)
