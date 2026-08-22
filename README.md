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
just provision acme eu tenant_acme operator@example.com
```

`messgr-control provision` creates the tenant's database, applies its (currently empty)
migrations, and registers it in the control database. Re-running it for the same `slug` is
safe.

`docker compose up -d` also starts a dev-mode Vault (`VAULT_ADDR=http://localhost:8200`,
`VAULT_TOKEN=messgr-dev-root-token`, both in `.env.example`) backing the `KeyStore` trait
(`src/keystore.rs`) that per-customer data-encryption keys go through (DESIGN.md §7.6).
`just vault-dev-init` is idempotent and only needs to run once per fresh Vault container — it
enables the Transit secrets engine and creates the `messgr-dek` key that `tests/keystore.rs`
and later tickets (per-tenant mounts, the DEK lifecycle) exercise. A dev-mode Vault is for local
development and CI only: `VaultKeyStore::connect` refuses to start against a non-TLS
`VAULT_ADDR` outside `MESSGR_PROFILE=dev`.

Run `just --list` for the rest of the available recipes (build, test, lint, db-shell, ...).

## Status

Design-driven, from-scratch rebuild in progress against `DESIGN.md`. Ticket `T-001`
establishes the control database, tenant registry, and this provisioning CLI — the
foundation everything else (producer identity, the ledger, dispatchers) builds on.
