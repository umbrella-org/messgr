---
id: T-032
title: Health endpoints for messgr-ingest and messgr-dispatcher
project: messgr
depends-on: []
spawned-by: [T-025]
impact: medium
complexity: medium
cost: M
---

# T-032 — Health endpoints for messgr-ingest and messgr-dispatcher

## Outcome

An orchestrator or load balancer can ask `messgr-ingest` and `messgr-dispatcher` "are you up"
against a plain, unauthenticated `/healthz` without needing a client certificate — short of
today's only option, a real mTLS `POST /comms` request.

## Description

Split out of T-025's item 8 at refinement: "add a minimal `/healthz` per binary" turns out to
need an actual architecture decision, not a one-line route addition, because the two existing
long-running binaries have no plain HTTP surface to hang a health route off:

- `messgr-ingest`'s entire `axum::Router` is served behind `ClientCertAcceptor`
  (`src/bin/ingest.rs:82-93`) — mTLS is mandatory for the whole listener. A `/healthz` route
  added to that same router would still require a valid client cert to reach, which defeats the
  point for a plain orchestrator/LB probe.
- `messgr-dispatcher` runs no HTTP server at all (`src/bin/dispatcher.rs`) — it's a bare claim
  loop. Adding health here means introducing a small HTTP listener where none exists, not adding
  a route to one.

Scope:

1. `messgr-ingest`: add a second, plain (non-TLS) `axum` listener on its own port
   (e.g. `INGEST_HEALTH_LISTEN_ADDR`, default `0.0.0.0:8080`) serving only `GET /healthz`,
   spawned alongside the existing mTLS listener in `main`. No tenant/DB/Vault dependency in the
   handler — liveness only ("the process is up and serving"), not readiness against downstream
   dependencies.
2. `messgr-dispatcher`: add the same minimal plain `axum` listener + `/healthz` route
   (`DISPATCHER_HEALTH_LISTEN_ADDR`, same default), spawned alongside its existing claim loop.
3. **Out of scope:** `messgr-control` (a one-shot CLI, not a long-running process — a health
   endpoint doesn't apply) and `messgr-query`/`messgr-webhook` (build-order steps 12–13, not
   built yet — can't add a health route to a binary that doesn't exist). A future ticket for
   either of those binaries should add its own `/healthz` at build time, following this ticket's
   pattern, rather than this ticket reaching forward to them.
4. Confirm at refinement: response body/content for `/healthz` (bare 200 OK is likely sufficient
   given liveness-only scope) and whether the two new env vars need `.env.example`/user-manual
   documentation (yes if operators are expected to set them, per the docs step).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-05 — created (TO DO). source: review: split out of T-025's item 8 at refinement — needs a real architecture decision (a second unauthenticated listener per binary), not a one-line route addition, a scope big enough to warrant its own ticket.
