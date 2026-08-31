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
path consumes this identity yet — `messgr::producer::resolve` (below) is the first reader.

Re-running `register` with identical inputs for the same `--name` is a safe no-op. Re-running it
with different inputs for the same `--name`, or with a `--cert-subject` already bound to another
producer (in this tenant or a different one), is rejected with a non-zero exit.

`producer disable` sets **two** copies to disabled, control database first: `producer_cert.enabled`
(control) — the column mTLS resolution actually reads, since resolution never opens a tenant
database — and then `producer.enabled` (tenant) — the source of truth `producer list` displays.
Neither row is ever deleted, so a disabled producer's certificate fails resolution with a
distinct, diagnosable "disabled" error instead of looking unregistered. Reversible by registering
the same `--name`/`--cert-subject` again (which never re-enables `producer_cert.enabled` — only
`disable` and the initial `register` ever touch that column). Disabling an already-disabled
producer is a safe no-op.

Every `register`/`disable` attempt, including a rejected one, writes a `platform_audit` row
(`producer.register` / `producer.disable`).

### Tenant configuration

```
cargo run --bin messgr-control -- tenant-config set --tenant-slug acme \
    --retention-years 7 --default-timezone Europe/London --default-locale en-GB \
    --quota-day-boundary-tz Europe/London --staleness-max-age-seconds 7200 \
    --actor operator@example.com
cargo run --bin messgr-control -- tenant-config show --tenant-slug acme
```

`tenant_config` (DESIGN.md §4.10) is a **singleton per tenant** — one row, in the tenant's own
database, enforced by a `singleton boolean PRIMARY KEY` rather than by convention. A freshly
provisioned tenant has **no** row until `tenant-config set` is run at least once: several of
these fields (the staleness bound, the quota day-boundary timezone) have no platform-wide
default DESIGN.md has settled on, so there is nothing sensible to seed automatically.

`set` is a plain upsert — safe to re-run. Its outcome is `created` (no prior row), `updated`
(a prior row existed with different values), or `idempotent` (identical to what's already
there); every call, including a rejected one against an unknown `--tenant-slug`, writes a
`tenant_config.set` `platform_audit` row. `--schedule-horizon-days` and `--verification-mode`
are optional, defaulting to DESIGN.md's own `90`/`observe`. This ticket (T-007) scopes the
table to the six fields above — `display_name` and the `oidc_*` columns from DESIGN.md's full
§4.10 table arrive later, when a ticket actually reads them (OIDC: T-035).

### mTLS resolution and dev PKI

`messgr::producer::resolve::resolve_producer(control_pool, cert_subject)` (DESIGN.md §4.9, §11.1,
T-006) is the shared layer future ingest binaries compose to turn a client certificate's subject
into `(tenant_id, producer_id)`: one control-database query, distinguishing an unknown cert from a
known-but-disabled one. No TLS-terminating binary exists yet to call it — that is `T-011`
(`messgr-ingest`).

To exercise it locally without a real CA:

```
just dev-pki-bootstrap                                  # one-time: mount pki, root CA, producer-dev role
just dev-pki-issue-cert fraud-alerts.internal /tmp/cert  # writes cert.pem, key.pem, ca.pem
openssl x509 -noout -subject -in /tmp/cert/cert.pem      # CN=fraud-alerts.internal
```

`dev-pki bootstrap`/`dev-pki issue-cert` are backed by Vault's own PKI secrets engine (the same
dev-mode Vault `docker compose up -d` already starts) and refuse to run outside
`MESSGR_PROFILE=dev` — minting a trusted producer client certificate ad hoc must never be
reachable against a real Vault.

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
