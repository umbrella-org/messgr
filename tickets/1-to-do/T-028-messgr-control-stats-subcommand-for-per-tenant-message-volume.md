---
id: T-028
title: messgr-control: stats subcommand for per-tenant message volume
project: messgr
depends-on: []
spawned-by: [T-027]
impact: medium
complexity: low
cost: S
---

# T-028 — messgr-control: stats subcommand for per-tenant message volume

## Outcome

After this ships, running `messgr-control stats --tenant-slug <slug>` prints a table of
message counts by channel (SMS, email) — ingested, dispatched, delivered, failed — for that
tenant, optionally filtered by `--since <date>`, without standing up any new server.

## Description

Spawned by T-027 (dropped): the original ask was a web dashboard with message-traffic KPIs,
but that meant a brand-new binary + unauthenticated HTTP server, ahead of open
correctness/security tickets (T-018, T-020, T-021, T-023, T-024) and directly against
DESIGN.md §11.1's guard on shipping any UI over this system without `AuthProvider`. This
ticket gets the same numbers a much cheaper way: a `stats` subcommand on the existing
`messgr-control` binary.

**Mechanism.** Add `Stats { tenant_slug: String, since: Option<NaiveDate>, channel:
Option<...> }` to `control.rs`'s `Command` enum, following the same shape every other
subcommand already uses (`--tenant-slug`, per src/bin/control.rs). Resolve the tenant's
pool with `connect_tenant_pool` (`src/tenant/pool.rs`) — already used by other subcommands,
no new connection machinery, no Vault client (counts only, no decryption of any payload).
Run count queries against that tenant's `comms_request` (ingested, i.e. what
`messgr-ingest` accepted at `POST /comms`/`/comms/bulk`) and `outbox`/`comms_event`
(dispatched, delivered, failed), `GROUP BY` channel, restricted to SMS + email, optionally
filtered by `--since`. Print as a plain table to stdout; a `--format json` flag is a cheap
add if useful for scripting, not required for the first cut.

**Why this doesn't need auth.** `messgr-control`'s trust boundary is already "whoever can
run this binary has DB/Vault access" — same as every other subcommand it has today. No
listener, no new attack surface, no exception to record against §11.1 (that guard is about
UIs; this is an operator CLI, same class as the rest of `messgr-control`).

**Why this stays inside existing design boundaries.** DESIGN.md §11.4 already draws the
line for platform-console-style tooling: counts/metadata are fine to show ("per-tenant
health and volume" is explicitly listed), message *content* is not (no Transit policy, no
decryption). This ticket only ever runs `count(*)`/`GROUP BY` — it never touches
`payload_ciphertext` or any DEK.

**Explicitly out of scope:** any HTTP server, any browser UI, any trend chart, any
cross-tenant aggregation in one call (loop the registry yourself, or pass `--tenant-slug`
per tenant) — those stay with whatever eventually picks up DESIGN.md's real
`messgr-query`/§11.2/§11.3 build-order step, not this ticket.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: review: T-027 (unauthenticated new web binary,
  wrong shape for the actual need) was dropped in favor of this cheaper CLI-only
  alternative that reuses `connect_tenant_pool` and stays inside §11.4's metadata-only
  boundary.
