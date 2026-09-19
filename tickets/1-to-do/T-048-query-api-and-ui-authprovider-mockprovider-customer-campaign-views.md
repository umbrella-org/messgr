---
id: T-048
title: Query API and UI: AuthProvider, MockProvider, customer/campaign views
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: XL
---

# T-048 — Query API and UI: AuthProvider, MockProvider, customer/campaign views

## Outcome

After this ships, a role-authenticated bank employee — not just someone with `psql` — can look
up a customer's cross-channel message timeline, inspect a single message's rendered content and
event history, and run campaign-reach queries, with every access enforced server-side by role
and every compliance/`customer_service` lookup logged.

## Description

Build-order step 13 (§14): `query-api`, a REST API with a published OpenAPI spec plus a
server-rendered Askama+htmx UI in the same binary (`10-query-api-ui.md` §11), reading only from
a streaming replica so an unbounded compliance search cannot starve ingestion.

Scope, per §11/§11.1/§11.2:

- Endpoints: `GET /comms` (filtered list), `GET /comms/{id}` (detail + event history),
  `GET /customers/{id}/timeline`, `GET /producers/{id}/usage`, `GET /producers/{id}/quota`.
  (`POST /comms`, `POST /comms/bulk`, `DELETE /comms/{id}` already exist on `messgr-ingest`
  per T-011/T-041 — this ticket does not duplicate them, only reads.)
- `AuthProvider` trait with `OidcProvider` and `MockProvider`, and the **mandatory**
  production guard: refuse to start if `auth.provider = "mock"` while `profile != "dev"`. §11.1
  is explicit this guard "belongs in the first commit that introduces the trait rather than
  being retrofitted" — not a follow-up.
  Real OIDC wiring lands whenever a tenant's IdP is available; `MockProvider` unblocks the rest
  of this ticket regardless (decision 2, `14-decisions-and-open-questions.md`).
- Five roles enforced server-side, never in the UI layer: `customer_service` (single-customer
  only, no list/export), `compliance` (unrestricted search + export, every access audit-logged),
  `campaign_ops` (aggregates, no bodies), `comms_ops` and `admin` are read-only from this
  ticket's perspective — their write surfaces (kill switches, quota, producer registry, template
  approval) belong to T-049 (admin panel, step 14), not here.
- Views: customer timeline (all channels, `customer_alias`-expanded so merged customers show
  one history — §11.2), message detail (template version, rendered content, full event
  history), campaign reach summary (`(campaign_id, created_at)` index on the replica,
  §11.2 — no rollup table, no OLAP tier, cut and documented as unjustified at biweekly query
  frequency).

**History note.** An earlier, narrower attempt at this surface (T-027) was filed and dropped
before T-018/T-020/T-021/T-023/T-024 (open correctness/security tickets at the time) landed,
specifically because it proposed an *unauthenticated* web server — directly against §11.1's
guard. Those prerequisite tickets are now all in `6-done/`; this ticket is the real §11/§11.2
surface, not a repeat of that shortcut.

Coupling: T-049 (admin panel) is explicitly "same binary, same server-rendered stack" (§11.3)
— it extends whatever this ticket stands up and carries a hard `depends-on: [T-048]` (this
ticket) for that reason.

Out of scope: `messgr-control` / platform console (§11.4, already separate, different binary
and auth realm), the rollup/OLAP tier (explicitly deferred), real OIDC discovery/JWKS wiring
against a live tenant IdP (blocked on tenant onboarding, not on this ticket).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 13, remaining gap identified when auditing unticketed steps against the board
