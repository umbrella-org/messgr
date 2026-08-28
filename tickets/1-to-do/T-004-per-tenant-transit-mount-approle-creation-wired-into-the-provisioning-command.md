---
id: T-004
title: Per-tenant Transit mount + AppRole creation wired into the provisioning command
project: messgr
depends-on: [T-003]
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-004 — Per-tenant Transit mount + AppRole creation wired into the provisioning command

## Outcome

Provisioning a new tenant (the command T-001 built) creates that tenant's own Transit mount
and AppRole in Vault as part of provisioning itself — not as a manual step run afterward. Every
tenant has its own key-management identity in Vault from the moment it exists.

## Description

T-003 built the key-management substrate — the `KeyStore` trait, a Transit client, and a
dev-mode Vault in Compose — but deliberately left one seam open: nothing yet creates a
per-tenant Transit mount or AppRole. Today that would be a manual step. T-004 fills the seam by
wiring that creation into the provisioning command from T-001, so provisioning a tenant is once
again one command that leaves the tenant fully ready — control-database row, physical database,
and now its Vault identity together.

Concretely: on provisioning a tenant, create a Transit mount scoped to that tenant and an
AppRole (with SecretID delivery) authorized against it, and persist whatever reference the
rest of the system needs to address them (e.g. mount path / role id) alongside the tenant's
other provisioning-time records. Per DESIGN.md §7.6 / §11.4.

**Constraint carried from T-003 (§7.6):** `KeyStore` must keep `wrapped_dek` opaque. The
deferred "wrapped DEKs in Vault KV" migration only stays available if the column never leaks
into queries or the API — this ticket must not add any path that exposes it.

**Open question (§ "Still open", item 12):** Vault edition (Community vs Enterprise) is
unresolved and shapes what per-tenant Transit mount isolation looks like operationally (e.g.
namespaces are Enterprise-only). Flagging here since PLAN.md cites this ticket as the one it
gates; the Implementation Plan will need this answered, or an explicit interim assumption
signed off by the user, to pass the READY gate.

Soft coupling: T-005 (production Vault topology, 3-node Raft + Shamir unseal) is the
operational hardening this ticket's dev/single-node story defers to; no hard dependency.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-29 — created (TO DO). source: chat: decomposed from PLAN.md's build-order breakdown of DESIGN.md (build step 0, the seam T-003 left open).
