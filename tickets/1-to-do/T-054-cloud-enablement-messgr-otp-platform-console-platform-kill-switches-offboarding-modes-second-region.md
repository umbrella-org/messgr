---
id: T-054
title: Cloud enablement: messgr-otp, platform console, platform kill switches, offboarding modes, second region
project: messgr
depends-on: [T-052]
spawned-by: []
impact: critical
complexity: high
cost: XL
---

# T-054 — Cloud enablement: messgr-otp, platform console, platform kill switches, offboarding modes, second region

## Outcome

After this ships, messgr becomes a hosted multi-tenant product from the same codebase the
on-prem launch customer already runs — cloud tenants can be provisioned, their staff can OTP
without linking a Rust library, provider staff have a console instead of `psql`, an abusive or
non-paying tenant can be suspended without touching its data, a departing tenant can be
offboarded under either retention obligation, and a second region proves the region-boundary
isolation claims hold rather than being assumed at N=1.

## Description

Build-order step 19 (§14), last, explicitly gated on "once the on-prem customer is live" —
everything before this point already runs multi-tenant at N=1 (decision 19,
`14-decisions-and-open-questions.md`: "one codebase, on-prem is N=1... no compile-time tenancy
flag, no second deployment path"), so this step adds surfaces rather than reworking the core.
Five components, per build-order's own line and `01-overview-architecture.md`/`12-deployment.md`:

- **`messgr-otp`** (§3.1) — the cloud-only variant of T-052's OTP mechanism: a dedicated
  minimal endpoint per region, synchronous, mTLS-authenticated, no queue/gate-chain/Postgres
  write on the request path, audit record written async/best-effort exactly like the on-prem
  library. Fetches its provider credential from Vault **at startup**, holds it in memory for
  the process lifetime, refreshed on a background timer — not per-request — per the correction
  already recorded in §3.1 (an earlier draft implied a per-request Vault call with no stated
  behaviour for a sealed Vault; fixed to match §7.6's DEK-cache discipline).
- **Platform console** (§11.4) — separate binary/auth realm from the tenant admin panel (T-049)
  by design, "because the failure mode of conflating them is an operator accidentally acting
  inside a tenant." Shows tenant lifecycle, `tenant_schema_version` drift, per-tenant
  health/volume, platform kill switches, `platform_audit`. Cannot read message content
  (operators hold no Transit policy for any tenant mount); can see metadata, which must be
  disclosed to tenants as such. T-028 already shipped a `messgr-control stats` CLI subcommand —
  narrower and not this console; this ticket does not duplicate it.
- **Platform kill switches** (`platform_kill_switch`, control database, §4.11/§4 gate-chain doc)
  — explicitly deferred by T-016 ("Cloud-only, unread until step 19"). Two-tier: a platform
  switch overrides a tenant's switch, never the reverse; propagation fans out per-tenant since
  there's no single `NOTIFY` target across databases, with the dispatcher's 30s periodic re-read
  as the guaranteed path and `NOTIFY` as the fast path. Tenant admin panel must show a platform
  suspension as a distinct, non-actionable state.
- **Offboarding modes** (§7.7) — `tenant.status` already carries `offboarding_archive` /
  `offboarding_destroy` in the schema (T-001), but no tooling enforces either mode yet.
  **Terminate and destroy**: destroy the Transit key, `DROP DATABASE`, revoke AppRoles — O(1),
  minutes, immediate unreadability subject to the §7.3 backup window. **Terminate and archive**:
  database and Transit key retained for the tenant's remaining retention period; producers and
  UI disabled; dispatchers stopped; read access only via a restricted export path; needs an
  explicit end date after which it converts to destroy. §7.7 is explicit the archive mode "needs
  pricing rather than engineering" — a business decision for refinement, not this ticket to
  assume.
- **Second region** — proves the region-boundary assertions (independent control DB, Vault,
  Postgres, binaries; tenants pinned to one region; no cross-region dependency, decision 15)
  rather than leaving them asserted at N=1. Still-open item #10 (launch regions/jurisdictions)
  needs answering first — determines how many independent stacks and keyholder sets this
  actually stands up.

**Strongly recommend splitting at refinement.** These five items are independently schedulable
— separate binaries/surfaces with no shared implementation beyond already-existing per-tenant
patterns — and build-order groups them under one step only because they share a common
precondition (on-prem live) and a common theme (cloud launch), not because they're one unit of
work. Filed as a single TO DO ticket here to match the build-order line item; per rules §3
("split only what is independently schedulable... otherwise it stays a task in this plan"),
refinement should very likely split this into its own family rather than write one seven-part
Implementation Plan.

Hard dependency on T-052 (`depends-on:`, user-confirmed): `messgr-otp` is the cloud variant of
T-052's OTP mechanism and reuses its `sms-sender` audit-write/Vault-caching pattern rather than
inventing a second one — this ticket cannot be picked up until T-052 is done and merged. Platform
kill switches separately reuse T-016's tenant-switch propagation shape, two-tiered (T-016 is
already done — no additional gating from that side).

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 19, remaining gap identified when auditing unticketed steps against the board
- 2026-09-19 — added hard depends-on: [T-052], user-confirmed (messgr-otp reuses T-052's sms-sender pattern, §3.1)
