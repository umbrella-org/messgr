---
id: T-008
title: Per-customer DEK lifecycle: customer_dek, LRU cache, pre-provisioning, HMAC pepper
project: messgr
depends-on: [T-003, T-004]
spawned-by: []
family: T-007
impact: critical
complexity: high
cost: L
---

# T-008 — Per-customer DEK lifecycle: customer_dek, LRU cache, pre-provisioning, HMAC pepper

## Outcome

After this ships, every payload written to the ledger is encrypted under a key unique to its
customer, pulled from a bounded, zeroizing in-memory cache rather than fetched from Vault on
every write, and destination values carry a per-tenant HMAC for lookup. Per-customer DEKs from
the first write is the one design invariant that cannot be retrofitted (design §7, §14).

## Description

Build the per-customer DEK lifecycle: `customer_dek` table, Vault Transit datakey creation,
a bounded zeroizing LRU cache, a pre-provisioning batch path, and a per-tenant HMAC pepper for
destination lookups (design §7.1, §7.6, §4.5). This ticket also finishes the seam T-004
deliberately left open: `T-004` created each tenant's Vault identity (Transit mount, AppRole,
response-wrapped SecretID) but wired nothing to *authenticate* as it — `VaultKeyStore` still
connects with a single environment `VAULT_TOKEN`. This ticket performs the AppRole login
(unwrap the SecretID, log in, use the resulting tenant-scoped token for `create_dek`/
`unwrap_dek`) — do not treat that as already done just because the per-tenant credentials
exist (`PLAN.md` note under build step 0).

Part of the step-2 ticket family (`family: T-007`, see T-007). `wrapped_dek` must stay opaque
so the deferred "wrapped DEKs in Vault KV" migration (§7.6) remains available — never let it
leak into queries or the API.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
