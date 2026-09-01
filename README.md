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

See [`docs/user-manual`](docs/user-manual.adoc) for the rest: the full `messgr-control`
command reference (producers, tenant/provider configuration, customer DEK pre-provisioning,
templates, partition lifecycle), dev PKI, and the `messgr-ingest`/`messgr-dispatcher` walkthrough
for sending a message end to end. Run `just docs-build` to render it (PDF/EPUB into `dist/docs/`,
never committed).

Run `just --list` for the rest of the available recipes (build, test, lint, db-shell, ...).

## Status

Design-driven, from-scratch rebuild in progress against `DESIGN.md`. Ticket `T-001`
establishes the control database, tenant registry, and this provisioning CLI — the
foundation everything else (producer identity, the ledger, dispatchers) builds on.

Three binaries exist so far: messgr-control (provisioning/admin CLI, run with cargo run --bin
messgr-control -- <subcommand>), messgr-ingest (T-011's POST /comms service, run with cargo run
--bin messgr-ingest or `just ingest-run`), and messgr-dispatcher (T-013's per-tenant claim loop,
run with cargo run --bin messgr-dispatcher or `just dispatcher-run`).
