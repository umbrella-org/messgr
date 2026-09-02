---
id: T-020
title: Make tenant pool identity unrepresentable to mis-wire
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-020 — Make tenant pool identity unrepresentable to mis-wire

## Outcome

A tenant pool's expected database can no longer be derived from the same input used to build its own connection string, so the `current_database()` isolation assertion (DESIGN.md §2.1, decision 14) can actually fail when a pool is mis-wired. A mutation test that re-couples the two proves the point: it turns the isolation test red, which it does not today.

## Description

`src/tenant/pool.rs::connect_tenant_pool` takes one `database_name: &str` argument and feeds it to *both* `db::with_database_name` (which builds the connection `options`) and `db::connect_with_expected_database`'s `expected_db` parameter (which the post-connect assertion checks against). Both values trace back to the same variable, so the assertion always compares a value to itself — it cannot observe a real mis-wiring where the caller intended tenant A but a bug (wrong registry lookup, copy-paste in a call site, a stale cached pool) supplied tenant B's name to the connection builder while some other path still labels it "A". This is exactly the failure mode §2.1 removed RLS in favour of catching cheaply; as written, nothing catches it.

`src/db.rs::connect_with_expected_database` additionally offers a `before_acquire` (checkout-time) recheck arm. Its own test doc comment (lines 136-139) already admits this can never fire in practice: "a live Postgres connection can never actually change which database it is bound to mid-life, so there is no way to make a real pool checkout observe a mismatch `after_connect` did not already catch." DESIGN.md §2.1 has been corrected (this audit) to state the assertion fires once, at creation, not at checkout — this ticket is where the code catches up: the dead `before_acquire` branch should be removed, not left as a check that looks meaningful and cannot be.

The fix is a type-level one, not a runtime one: give the tenant pool handle a type that carries its own tenant identity (e.g. a `TenantPool { tenant_id: TenantId, pool: PgPool }` or a newtype wrapper), constructed only by a single function that resolves `expected_db` from the tenant registry independently of whatever the caller passed for connection purposes — so a caller cannot construct a pool for tenant B while holding a handle that claims to be tenant A. `src/tenant/registry.rs` (which already resolves tenant metadata) is the natural place for `expected_db` to come from, rather than a bare string threaded through by the caller.

Soft coupling: the acceptance test for this ticket is the isolation-mechanism test DESIGN.md §14 now specifies — "a mutation test that deliberately re-derives both from the same input must turn this test red." No other ticket currently owns that test; this one does.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found the isolation assertion in src/tenant/pool.rs compares a value to itself, so it cannot fire on a real mis-wiring; src/db.rs's own test doc comment already admits the checkout-time arm is dead.
