---
id: T-027
title: messgr-query — serve command and htmx traffic-overview screen
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-027 — messgr-query — serve command and htmx traffic-overview screen

## Outcome

After this ships, running `messgr-query serve` starts a small read-only web server whose
one screen shows message-traffic KPIs (ingested vs. dispatched, by SMS/email channel, plus
delivery/failure rates) over a chosen date range, visually matching
`docs/mockups/admin-dashboard.html`, light or dark to match the browser's OS setting.

## Description

DESIGN.md §11 already specifies a `messgr-query` binary (axum + Askama + htmx, same binary
as the read API, reads never touching the primary — §11.2) but nothing has been built yet:
no binary, no `AuthProvider` trait, none of §11.2's views (customer timeline, message
detail, campaign reach) or §11.3's admin panel (quota dashboard, kill-switch console,
scheduled queue, producer registry). This ticket is a **first, narrow slice** of that
binary: one new "Overview" screen, not the views §11.2/§11.3 already describe. Those stay
out of scope here and are picked up by later tickets.

**What "incoming" and "outgoing" mean.** The ledger has no customer-reply concept — it is
strictly bank → customer (AGENTS.md). Per the requester: **incoming** = requests
`messgr-ingest` accepts at `POST /comms` / `POST /comms/bulk` (i.e. `comms_request` rows
created), and **outgoing** = messages actually dispatched to a provider (`outbox` /
`comms_event`), both counted by channel (SMS, email only — no WhatsApp on this screen) and
by day. The "delivery rate" / "failed" KPI tiles read `comms_event`'s terminal status per
DESIGN.md §6. Exact columns/query shape are for the Implementation Plan to pin down against
the current `comms_request` / `outbox` / `comms_event` schema (§4) — this Description only
fixes what the numbers *mean*.

**UI.** Server-rendered Askama templates + htmx, per §11's stated stack, matching
`docs/mockups/admin-dashboard.html` (KPI tiles, SMS/email breakdown cards, a sent/received
trend line with hover tooltip + table-view toggle, a channel-mix stacked bar, a recent-
activity table). Use
[Alex Edwards' "How I use htmx with Go"](https://www.alexedwards.net/blog/how-i-use-htmx-with-go)
as the pattern guideline — partial templates returned as htmx fragment swaps, `hx-boost`
for full-page navigation, no client-side framework — adapted to Rust/Askama; it is a
guideline for the *pattern*, not code to port. Dark/light strictly follows the browser's
`prefers-color-scheme` — no manual theme toggle in this ticket (the mockup's toggle button
was a demo affordance for comparing both themes side by side, not a requirement).

**Command shape.** A new `messgr-query` binary (`[[bin]]` in `Cargo.toml`, alongside
`messgr-control`/`messgr-ingest`/`messgr-dispatcher`), with a `clap` `Subcommand` the same
way `messgr-control` is structured (`src/bin/control.rs`): `messgr-query serve --addr
<host:port>` starts the axum server (default `127.0.0.1:8745`-style loopback address, exact
default for the plan to pick).

**Deliberate, explicit deviation from §11.1 — recorded, not silent.** §11.1 calls the
`AuthProvider` trait + mock-in-prod guard "not optional... belongs in the first commit that
introduces the trait." This ticket ships with **no auth at all**, by explicit user decision
(faster to a working screen; real OIDC wiring is separate, larger work). Mitigation the
plan must include: the server refuses to bind to any non-loopback address unless the caller
passes an explicit acknowledgement flag, and logs a prominent startup warning that the view
is unauthenticated. The Docs step must add an entry to DESIGN.md's "Still open" list noting
this screen ships without `AuthProvider` and must not be exposed beyond localhost/dev until
that lands.

Soft coupling: this is the first slice of DESIGN.md's build-order step 13 (`messgr-query` /
"Query API and UI"); §11.2's remaining views and all of §11.3's admin panel are separate,
future tickets and must not be folded in here.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: chat: requested a "serve" command and admin
  dashboard exactly matching `docs/mockups/admin-dashboard.html`; scoped down in
  conversation to a new query-api Overview screen (not §11.2/§11.3's already-specified
  views), with "incoming"/"outgoing" defined against `messgr-ingest` vs. dispatch, and an
  explicit, user-approved no-auth exception to §11.1 for this first slice.
- 2026-09-02 — TO DO → DROPPED: unauthenticated new web binary is wrong shape for the actual need; a stats subcommand on existing messgr-control (per-tenant counts, no new server/auth surface) reuses connect_tenant_pool and stays inside the metadata-only boundary already drawn for the platform console in DESIGN.md section 11.4, and does not preempt correctness/security tickets T-018/T-020/T-021/T-023/T-024
