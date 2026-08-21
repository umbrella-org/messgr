# messgr

Centralized communications orchestration and audit ledger for customer messaging across
SMS, email, and WhatsApp. See [`DESIGN.md`](DESIGN.md) for the full design; feature work is
tracked as tickets under [`tickets/`](tickets/BOARD.md).

## Local development

```
cp .env.example .env
docker compose up -d          # Postgres, database `control`
just control-migrate          # apply control-database migrations
just provision acme eu tenant_acme operator@example.com
```

`messgr-control provision` creates the tenant's database, applies its (currently empty)
migrations, and registers it in the control database. Re-running it for the same `slug` is
safe.

Run `just --list` for the rest of the available recipes (build, test, lint, db-shell, ...).

## Status

Design-driven, from-scratch rebuild in progress against `DESIGN.md`. Ticket `T-001`
establishes the control database, tenant registry, and this provisioning CLI — the
foundation everything else (producer identity, the ledger, dispatchers) builds on.
