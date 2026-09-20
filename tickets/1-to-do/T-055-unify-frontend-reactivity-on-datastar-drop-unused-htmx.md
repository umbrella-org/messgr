---
id: T-055
title: Unify frontend reactivity on Datastar, drop unused htmx
project: messgr
depends-on: []
spawned-by: [T-049]
impact: low
complexity: low
cost: S
---

# T-055 — Unify frontend reactivity on Datastar, drop unused htmx

## Outcome

After this ships, `messgr-query-api` carries exactly one client-side reactivity library —
Datastar — across every view it serves, and `§11`'s stack line says so plainly. A future
contributor opening any template in `templates/query_api/` no longer has to guess which of two
hypermedia vocabularies applies, and the binary no longer ships a vendored script
(`assets/htmx.min.js`) that nothing references.

## Description

Found while refining T-049 (admin panel, spawned-by). §11 of the design doc states the UI stack
as "server-rendered Askama templates plus htmx" — T-048 (done) treated that literally and
vendored `assets/htmx.min.js`, served it via a plain `GET` route, and loads it from every page
through `templates/query_api/base.html`'s one `<script>` tag. But T-048's three shipped views
(customer timeline, message detail, campaign reach) turned out to need no client-side
interactivity at all: there is exactly one `<a href>` link in the whole UI
(`templates/query_api/timeline.html`) and **zero `hx-*` attributes anywhere in the repo**. htmx
has been dead weight since the day it was vendored.

T-049 is about to introduce this binary's first real interactive view (the admin panel: a
blast-radius preview, cancel/release without a full reload) and picked Datastar for it,
user-directed. That leaves two hypermedia libraries in one binary, on disjoint route trees, one
of which (htmx) still does nothing anywhere. This ticket is the cleanup: pick one library for
the whole binary going forward and stop carrying the other.

Scope:

- Update `development/design/10-query-api-ui.md` §11's UI line to name Datastar, not htmx (and
  add a row to the decisions table if the reviewer judges this rises to that level — it wasn't
  one of the 32 existing rows when T-048 built it literally, so this may be the first time it's
  being decided rather than merely followed).
- Remove `assets/htmx.min.js`, its `GET /assets/htmx.min.js` route
  (`handlers::htmx_asset`/registration in `src/query_api/mod.rs`), and the
  `<script src="/assets/htmx.min.js">` tag in `templates/query_api/base.html` — confirm at
  refinement that nothing added between now and pickup has grown an `hx-*` attribute
  (`grep -rn 'hx-' templates/ src/` should still be empty).
- Add `<script src="/assets/datastar.js">` to `templates/query_api/base.html` so every future
  view in this binary — not just the admin panel — gets it from the one shared layout, matching
  how htmx was originally loaded site-wide rather than per-view.

**Soft coupling to T-049 (order-independent, no hard `depends-on:`):** T-049's own plan vendors
`assets/datastar.js` and adds `templates/admin/base.html` (a *second*, admin-only base template,
per that ticket's decision 10 — admin views extend `admin/base.html`, not
`query_api/base.html`). Whichever of the two tickets is picked up first does the actual
`assets/datastar.js` vendoring (`include_bytes!` + unauthenticated route, mirroring the htmx
precedent); the other reuses it instead of vendoring a second copy. If this ticket lands first,
T-049's own Task 6 ("Vendor Datastar") shrinks to "add the `<script>` tag to
`templates/admin/base.html`, pointing at the route this ticket already registered." Whoever
refines whichever ticket is picked up second should re-read the other's current state and note
in `## History` if a task got dropped as already done.

Out of scope: adding any new interactivity to T-048's three read views (search-as-you-type, live
counts) — none of that has been asked for, and this ticket is about removing an unused
dependency and stating one stack decision, not adding capability to those views.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-20 — created (TO DO). source: chat: user asked to file a ticket to unify reactivity and potentially remove the htmx dependency, after a discussion during T-049's refinement surfaced that htmx is vendored but entirely unused in the codebase
