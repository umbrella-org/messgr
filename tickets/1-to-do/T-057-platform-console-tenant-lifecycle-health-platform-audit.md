---
id: T-057
title: Platform console: tenant lifecycle, health, platform_audit
project: messgr
depends-on: []
spawned-by: [T-054]
impact: high
complexity: medium
cost: L
---

# T-057 — Platform console: tenant lifecycle, health, platform_audit

## Outcome

After this ships, provider staff operate cloud tenants through a console instead of `psql`:
tenant lifecycle, `tenant_schema_version` drift, per-tenant health/volume, platform kill
switches, and the `platform_audit` trail are all visible in one authenticated screen, with no
path to message content.

## Description

`01-overview-architecture.md`/`12-deployment.md` §11.4: a separate binary and separate auth realm
from the tenant admin panel (T-049, done), by explicit design — "because the failure mode of
conflating them is an operator accidentally acting inside a tenant." Shows: tenant lifecycle,
`tenant_schema_version` drift, per-tenant health/volume, platform kill switches (T-058), and the
`platform_audit` trail.

**Metadata only, never content.** Operators hold no Transit policy for any tenant mount, so the
console cannot read message content by construction, not by application-level check. It can see
metadata (volume, health, lifecycle state), and that visibility must be disclosed to tenants as
such.

T-028 already shipped a `messgr-control stats` CLI subcommand (per-tenant message volume) — this
console is a strict superset in surface (a web UI, tenant lifecycle, drift, kill switches, audit
trail) and does not duplicate it; T-028's subcommand can stay as the CLI-only path.

Soft coupling: displays platform kill switches (T-058) as one of its panes. The two are
independently buildable — this console can ship its other panes first and add the kill-switch
pane once T-058 lands — so this is a soft coupling, not a hard `depends-on:`.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-22 — created (TO DO). source: review: split out of T-054 at refinement — one of five
  independently-schedulable components bundled under build-order step 19; user confirmed
  splitting
