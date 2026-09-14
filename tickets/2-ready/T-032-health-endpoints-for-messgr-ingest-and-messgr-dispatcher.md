---
id: T-032
title: Health endpoints for messgr-ingest and messgr-dispatcher
project: messgr
depends-on: []
spawned-by: [T-025]
impact: medium
complexity: low
cost: S
---

# T-032 — Health endpoints for messgr-ingest and messgr-dispatcher

## Outcome

An orchestrator or load balancer can `curl http://<host>:8080/healthz` against `messgr-ingest`
or `messgr-dispatcher` and get a plain `200 OK` with no client certificate, in place of today's
only liveness signal — a real mTLS `POST /comms` request (impossible for `messgr-dispatcher`,
which has no HTTP surface at all).

## Description

Split out of T-025's item 8 at refinement: "add a minimal `/healthz` per binary" turns out to
need an actual architecture decision, not a one-line route addition, because the two existing
long-running binaries have no plain HTTP surface to hang a health route off:

- `messgr-ingest`'s entire `axum::Router` is served behind `ClientCertAcceptor`
  (`src/bin/ingest.rs:88-102`) — mTLS is mandatory for the whole listener. A `/healthz` route
  added to that same router would still require a valid client cert to reach, which defeats the
  point for a plain orchestrator/LB probe.
- `messgr-dispatcher` runs no HTTP server at all (`src/bin/dispatcher.rs`) — it's a bare claim
  loop. Adding health here means introducing a small HTTP listener where none exists, not adding
  a route to one.

Scope:

1. New shared module `src/health.rs`: `pub fn router() -> axum::Router` returning a router with
   one route, `GET /healthz` → bare `200 OK`, empty body (liveness only — "the process is up and
   serving", not readiness against DB/Vault/downstream dependencies). Both binaries wire this
   same router into their own plain listener; the route itself is written once.
2. `messgr-ingest`: bind `messgr::health::router()` on a second, plain (non-TLS) listener on its
   own port (`INGEST_HEALTH_LISTEN_ADDR`, default `0.0.0.0:8080`), spawned via `tokio::spawn`
   alongside the existing mTLS listener in `main` (restructuring `main` to await both tasks, the
   pattern `messgr-dispatcher` already uses for its `handles` vector).
3. `messgr-dispatcher`: same, `DISPATCHER_HEALTH_LISTEN_ADDR`, same default, pushed onto the
   existing `handles` vector alongside the refresh loop and per-channel claim loops.
4. **Out of scope:** `messgr-control` (a one-shot CLI, not a long-running process — a health
   endpoint doesn't apply) and `messgr-query`/`messgr-webhook` (build-order steps 12–13, not
   built yet — can't add a health route to a binary that doesn't exist). A future ticket for
   either of those binaries should add its own `/healthz` at build time, following this ticket's
   pattern, rather than this ticket reaching forward to them.
5. Confirmed at refinement (user sign-off): bare `200 OK` with empty body for `/healthz`; the
   two new env vars get the same `.env.example` (commented, default shown) + user-manual
   treatment `INGEST_LISTEN_ADDR` already gets, despite having defaults — operators running
   behind a firewall/orchestrator still need to know the knob exists.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-032-health-endpoints
```

### Prerequisite gate (hard)

None. No `depends-on:`; nothing else must land first.

### Confirmed design decisions (do not deviate without asking)

1. **`/healthz` returns a bare `200 OK` with an empty body.** Liveness-only, per the Outcome —
   there is nothing else worth reporting and no reason to give a probe a body to parse.
2. **One shared router, `messgr::health::router()`, used by both binaries.** The route is
   identical in both; writing it twice would be the same code drifting in two places for no
   reason.
3. **The health listener has no dependency on DB/Vault/tenant state.** It answers "the process
   is up", not "the process is ready" — matches the Outcome and keeps the handler trivial
   (no `AppState`, no shared lock).
4. **The two new env vars are documented like `INGEST_LISTEN_ADDR`** — commented in
   `.env.example` with their default shown, and named in the relevant user-manual page — even
   though both default to `0.0.0.0:8080` and the binary starts fine unset.

### Tasks

#### Task 1 — shared health router (`src/health.rs`)

New file `src/health.rs`:

```rust
use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}
```

Register it in `src/lib.rs`: add `pub mod health;` (alphabetical, between `encryption` and
`ingest`).

#### Task 2 — wire it into `messgr-ingest` (`src/bin/ingest.rs`)

- Parse `INGEST_HEALTH_LISTEN_ADDR` the same way `INGEST_LISTEN_ADDR` is parsed today
  (`src/bin/ingest.rs:49-52`): `std::env::var(...).unwrap_or_else(|_| "0.0.0.0:8080".to_string()).parse().expect(...)`.
- `main` currently ends by awaiting the single mTLS `axum_server::bind(...).serve(...)` call
  directly (`src/bin/ingest.rs:98-102`). Restructure to spawn both listeners and await both,
  mirroring `messgr-dispatcher`'s `handles: Vec<JoinHandle<_>>` pattern
  (`src/bin/dispatcher.rs:204` / the final `for handle in handles { let _ = handle.await; }`
  loop):

  ```rust
  let mut handles = Vec::new();

  handles.push(tokio::spawn(async move {
      let listener = tokio::net::TcpListener::bind(health_listen_addr)
          .await
          .expect("failed to bind INGEST_HEALTH_LISTEN_ADDR");
      axum::serve(listener, messgr::health::router())
          .await
          .expect("health server error");
  }));

  handles.push(tokio::spawn(async move {
      axum_server::bind(listen_addr)
          .acceptor(acceptor)
          .serve(app.into_make_service())
          .await
          .expect("server error");
  }));

  for handle in handles {
      let _ = handle.await;
  }
  ```

#### Task 3 — wire it into `messgr-dispatcher` (`src/bin/dispatcher.rs`)

- Parse `DISPATCHER_HEALTH_LISTEN_ADDR` the same way (default `0.0.0.0:8080`), next to the
  other `env_var`/`std::env::var` reads near the top of `main`.
- Push one more entry onto the existing `handles` vector (`src/bin/dispatcher.rs:204` declares
  it), before or after the refresh-loop spawn — order doesn't matter, they're independent tasks:

  ```rust
  handles.push(tokio::spawn(async move {
      let listener = tokio::net::TcpListener::bind(health_listen_addr)
          .await
          .expect("failed to bind DISPATCHER_HEALTH_LISTEN_ADDR");
      axum::serve(listener, messgr::health::router())
          .await
          .expect("health server error");
  }));
  ```

  No change to the existing `for handle in handles { let _ = handle.await; }` tail.

#### Task 4 — `.env.example` + user-manual

- `.env.example`: add `# INGEST_HEALTH_LISTEN_ADDR=0.0.0.0:8080` under the existing
  `messgr-ingest` block, and `# DISPATCHER_HEALTH_LISTEN_ADDR=0.0.0.0:8080` under the
  `messgr-dispatcher` block.
- `docs/user-manual/ingest.adoc`: extend the "Requires four env vars..." sentence (line 23) to
  name `INGEST_HEALTH_LISTEN_ADDR` (default `0.0.0.0:8080`) alongside the other four, one clause
  noting it's a separate plain listener for `GET /healthz`, no client cert needed.
- `docs/user-manual/dispatcher.adoc`: extend the "Requires `DISPATCHER_TENANT_SLUG`..." sentence
  (line 32) the same way for `DISPATCHER_HEALTH_LISTEN_ADDR`.

### Acceptance test

New file `tests/health.rs`, following the existing integration-test style (real listener, real
HTTP client, no mocks — `tests/ingest.rs`'s own convention, minus the mTLS/DB/Vault machinery
that route doesn't need):

```rust
use std::net::SocketAddr;

#[tokio::test]
async fn healthz_returns_200_over_plain_http() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, messgr::health::router()).await.unwrap();
    });

    let resp = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();

    assert_eq!(resp.status(), 200);
}
```

Run:

```
just build   # both binaries still compile with the restructured main()
just test    # includes tests/health.rs
just lint    # fmt + clippy clean
```

Manual smoke (not automated — full binary startup needs DB/Vault, out of scope for this route):
`INGEST_HEALTH_LISTEN_ADDR=127.0.0.1:8080 ... just ingest-run` (with the usual mTLS env vars),
then `curl -i http://127.0.0.1:8080/healthz` → `200 OK`, no `-k`/cert flags needed. Same for
`just dispatcher-run` with `DISPATCHER_HEALTH_LISTEN_ADDR`.

### Docs update (mandatory when user-facing)

`.env.example` and `docs/user-manual/ingest.adoc` + `docs/user-manual/dispatcher.adoc` — see
Task 4 above.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint` clean.
2. Docs updated per Task 4.
3. Write a summary: files touched (`src/health.rs`, `src/lib.rs`, `src/bin/ingest.rs`,
   `src/bin/dispatcher.rs`, `tests/health.rs`, `.env.example`, both `.adoc` pages), decisions
   made (the four above), anything deferred (readiness checks against DB/Vault — out of scope,
   this ticket is liveness-only per the Outcome).
4. Suggested commit message:

   ```
   feat(ops): add plain /healthz listeners to ingest and dispatcher (T-032)

   Both binaries gain a second, unauthenticated axum listener serving a
   bare 200 on GET /healthz, so an orchestrator/LB can probe liveness
   without a client certificate. ingest's route was previously reachable
   only behind mTLS; dispatcher had no HTTP surface at all.
   ```

5. Root-path child (`path = "."`) — tidy WIP commits into atomic ones before presenting.
6. Commit locally on `feat/T-032-health-endpoints`. Publish only per commit policy (no push/MR
   without user approval). Present the commit message; after approval, verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing, then push and open the MR. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-05 — created (TO DO). source: review: split out of T-025's item 8 at refinement — needs a real architecture decision (a second unauthenticated listener per binary), not a one-line route addition, a scope big enough to warrant its own ticket.
- 2026-09-14 — TO DO → READY: plan complete
