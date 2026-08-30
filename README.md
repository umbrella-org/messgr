# messgr

Centralized communications orchestration and audit ledger for customer messaging across
SMS, email, and WhatsApp. See [`DESIGN.md`](DESIGN.md) for the full design; feature work is
tracked as tickets under [`tickets/`](tickets/BOARD.md).

## Local development

```
cp .env.example .env
docker compose up -d          # Postgres (database `control`) and a dev-mode Vault
just vault-dev-init           # one-time: enable Transit + create the messgr-dek key
just control-migrate          # apply control-database migrations
just provision acme eu tenant_acme operator@example.com  #  runs messgr-control provision
```

`messgr-control provision` creates the tenant's database, applies its (currently empty)
migrations, registers it in the control database, and provisions the tenant's own Vault
Transit mount, key, ACL policy, and AppRole (DESIGN.md §7.6, §11.4). Its output looks like:

```
7e5e6b1e-...                                     # tenant_id
vault_role_id=3c9f2b4a-...
vault_wrapped_secret_id=eyJhbGciOi...             # only on a fresh provision — see below
```

The RoleID is persisted (`tenant.vault_role_id`) and safe to log or query — it behaves like a
username, not a credential. The SecretID line, when present, is printed **exactly once**: it is
a Vault response-wrapped token (10-minute TTL, single-use `vault unwrap`) meant for out-of-band
delivery to that tenant's dispatcher deployment (§7.6: "SecretID delivered response-wrapped at
deploy time"). It is never written to Postgres anywhere — capture it immediately or re-run
provisioning is not a way to get it back (an idempotent re-provision of an already-active tenant
deliberately does not mint a new one; see the credential-rotation note below).

Re-running `provision` for the same `slug` (with the same `region`/`database_name`) is safe:
the database/migration/mount/key/policy/role steps are all idempotent no-ops on repeat, and no
second SecretID is minted. Rotating/reissuing a SecretID for an already-provisioned tenant is
not yet a supported operation — it is planned as an operational runbook, not a CLI flag.

### Producers

```
cargo run --bin messgr-control -- producer register \
    --tenant-slug acme --name fraud-alerts \
    --cert-subject "CN=fraud-alerts.internal" \
    --owner-team fraud --contact fraud-oncall@example.com --actor operator@example.com
cargo run --bin messgr-control -- producer list --tenant-slug acme
cargo run --bin messgr-control -- producer disable --tenant-slug acme --name fraud-alerts \
    --actor operator@example.com
```

`producer register` registers an upstream system (fraud, statements, onboarding, ...) against a
tenant (DESIGN.md §4.9): it writes both the `producer` row in the tenant's own database and the
`producer_cert` mapping in the control database, in that order (a producer only in the tenant
database is inert; a `producer_cert` pointing at no producer is a worse, later failure). No send
path consumes this identity yet — mTLS resolution against `producer_cert` is `T-007`.

Re-running `register` with identical inputs for the same `--name` is a safe no-op. Re-running it
with different inputs for the same `--name`, or with a `--cert-subject` already bound to another
producer (in this tenant or a different one), is rejected with a non-zero exit.

`producer disable` sets the tenant-side row to disabled but never deletes the `producer_cert`
mapping — that is what lets a disabled producer's certificate fail with a distinct, diagnosable
error at the mTLS edge instead of looking unregistered. It is reversible by registering the same
`--name`/`--cert-subject` again. Disabling an already-disabled producer is a safe no-op.

Every `register`/`disable` attempt, including a rejected one, writes a `platform_audit` row
(`producer.register` / `producer.disable`).

`docker compose up -d` also starts a dev-mode Vault (`VAULT_ADDR=http://localhost:8200`,
`VAULT_TOKEN=messgr-dev-root-token`, both in `.env.example`) backing the `KeyStore` trait
(`src/keystore.rs`) that per-customer data-encryption keys go through (DESIGN.md §7.6).
`just vault-dev-init` is idempotent and only needs to run once per fresh Vault container — it
enables the Transit secrets engine and creates the `messgr-dek` key that `tests/keystore.rs`
exercises, and now also enables the `approle` auth method that every tenant's own AppRole (one
role per tenant, inside this single shared backend) is created under. A dev-mode Vault is for
local development and CI only: `VaultKeyStore::connect` refuses to start against a non-TLS
`VAULT_ADDR` outside `MESSGR_PROFILE=dev`.

Run `just --list` for the rest of the available recipes (build, test, lint, db-shell, ...).

## Status

Design-driven, from-scratch rebuild in progress against `DESIGN.md`. Ticket `T-001`
establishes the control database, tenant registry, and this provisioning CLI — the
foundation everything else (producer identity, the ledger, dispatchers) builds on.

 Only binary that exists so far is messgr-control — run directly with cargo run --bin messgr-control -- <subcommand>.
