---
id: T-020
title: Make tenant pool identity unrepresentable to mis-wire
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: L
---

# T-020 — Make tenant pool identity unrepresentable to mis-wire

## Outcome

A tenant pool opened through `connect_tenant_pool` can no longer have its `current_database()` isolation assertion (DESIGN.md §2.1, decision 14) satisfied by construction: `expected_db` is resolved fresh from the tenant registry by `tenant_id`, independent of whatever `database_name` the caller supplies to build the connection itself. A caller that mis-wires the two — the exact bug class the assertion exists to catch — now gets a panic at pool-open time instead of a comparison that always trivially matches. A mutation-sensitive test proves the point: it goes red if the two derivations are ever re-coupled, which they do not today.

## Description

`src/tenant/pool.rs::connect_tenant_pool` takes one `database_name: &str` argument and feeds it to *both* `db::with_database_name` (which builds the connection `options`) and `db::connect_with_expected_database`'s `expected_db` parameter (which the post-connect assertion checks against). Both values trace back to the same variable, so the assertion always compares a value to itself — it cannot observe a real mis-wiring where the caller intended tenant A but a bug (wrong registry lookup, copy-paste in a call site, a stale cached pool) supplied tenant B's name to the connection builder while some other path still labels it "A". This is exactly the failure mode §2.1 removed RLS in favour of catching cheaply; as written, nothing catches it.

`src/db.rs::connect_with_expected_database` additionally offers a `before_acquire` (checkout-time) recheck arm. Its own test doc comment (lines 136-139) already admits this can never fire in practice: "a live Postgres connection can never actually change which database it is bound to mid-life, so there is no way to make a real pool checkout observe a mismatch `after_connect` did not already catch." DESIGN.md §2.1 has been corrected (this audit) to state the assertion fires once, at creation, not at checkout — this ticket is where the code catches up: the dead `before_acquire` branch should be removed, not left as a check that looks meaningful and cannot be.

The fix is a type-level one, not a runtime one: give the tenant pool handle a type that carries its own tenant identity (e.g. a `TenantPool { tenant_id: TenantId, pool: PgPool }` or a newtype wrapper), constructed only by a single function that resolves `expected_db` from the tenant registry independently of whatever the caller passed for connection purposes — so a caller cannot construct a pool for tenant B while holding a handle that claims to be tenant A. `src/tenant/registry.rs` (which already resolves tenant metadata) is the natural place for `expected_db` to come from, rather than a bare string threaded through by the caller.

Soft coupling: the acceptance test for this ticket is the isolation-mechanism test DESIGN.md §14 now specifies — "a mutation test that deliberately re-derives both from the same input must turn this test red." No other ticket currently owns that test; this one does.

**Settled during refinement (2026-09-03):** the fix is `connect_tenant_pool` (`src/tenant/pool.rs`) gaining a `tenant_id: Uuid` + `control_pool: &PgPool` input, resolving `expected_db` via a fresh `tenant_repo::find_by_id` lookup keyed on `tenant_id` — independent of the `database_name: &str` the caller still supplies to build the connection itself. This is DESIGN.md §2.1's own prescription verbatim ("`expected_database` must trace back to the caller's own intent... resolved separately from whatever the pool-construction code did with it"), so no DESIGN.md change is needed. The function now returns a `TenantPool { tenant_id, pool }` rather than a bare `PgPool`, per the Implementation Plan's decision 2. `connect_tenant_pool` is called at 17 production sites across 10 files and 34 sites across 11 integration-test files (applicability-gate recount, 2026-09-03; the per-file breakdown in the Implementation Plan's Task 3/4 was already correct) — every one already holds the `Tenant` row (hence `tenant.id`) and the `control_pool` it came from, right next to the call, so the edit is mechanical but genuinely fans out; `cost` re-graded `M` → `L` on that basis, `impact`/`complexity` unchanged.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-020-make-tenant-pool-identity-unrepresentable-to-mis-wire
```

Do all work on this branch; WIP commits are encouraged. Publish only per the project's commit
policy (`messgr` is publish-gated, root-path — tidy WIP into atomic commits before presenting,
default to keeping that history over squashing; no push/MR without explicit user approval).

### Prerequisite gate (hard)

None. `depends-on: []` — no other ticket must be done/merged first. Working tree must be clean
before branching.

### Confirmed design decisions (do not deviate without asking)

1. **`connect_tenant_pool` gains `control_pool: &PgPool` and `tenant_id: Uuid`, and drops `profile: Profile`.** `database_name: &str` stays, used only to build the connection (`db::with_database_name`). `expected_db` for the post-connect assertion is resolved fresh, inside the function, via `tenant_repo::find_by_id(control_pool, tenant_id)` — independent of whatever the caller passed as `database_name`. `profile` is dropped because its only remaining use (`checks_pool_identity`, decision 3) is removed.
2. **The function returns `TenantPool { pub tenant_id: Uuid, pub pool: PgPool }`, not a bare `PgPool`.** Every current call site already holds a `tenant.id` next to the `PgPool` it just opened; returning a struct that pairs them makes that pairing a value, not a coincidence of two variables in scope. Fields are `pub`, matching `Tenant`'s and `TenantContext`'s existing style — the safety property lives in there being exactly one function that produces a `TenantPool`, not in field privacy.
3. **The `before_acquire` arm and `Profile::checks_pool_identity` are deleted, not deprecated.** DESIGN.md §2.1 already states the assertion fires once, at creation; `db.rs`'s own test doc comment already admits the checkout-time arm can never observe anything creation-time didn't. `checks_pool_identity` has no other caller, so it is removed with it.
4. **No new `TenantId` newtype.** `Tenant.id`/`tenant_id` are `Uuid` everywhere in this codebase today; a wrapper type for this one ticket would add a conversion at every existing `Uuid`-typed call site for no safety this fix doesn't already provide by other means.
5. **`TenantPool.pool` is unwrapped (`.pool`) at the same call site that received it, not threaded further.** Every helper one level down (`approve_template_inner`, `set_provider_config_inner`, `run_inner`, `pre_provision_deks`, `repo::*`, `tenant_config_repo::*`, `PgListener::connect_with`, `sqlx::migrate!(...).run(...)`, etc.) keeps taking `&PgPool`/`PgPool` unchanged — only the ~13 functions that call `connect_tenant_pool` directly are touched.
6. **`connect_tenant_pool` still returns `Result<TenantPool, sqlx::Error>`, not a new error enum.** An unknown `tenant_id` maps to `sqlx::Error::Configuration(...)`, the same pattern this codebase already uses for domain-level rejections (e.g. `producer/register.rs::rejected`). Every existing `?`-propagating call site keeps compiling against its own existing `From<sqlx::Error>` impl — no new `From<...>` impls anywhere.

### Tasks

#### Task 1 — Remove the dead checkout-time recheck

`src/db.rs::connect_with_expected_database`: delete the `.before_acquire(...)` builder call and the `checks_on_acquire`/`expected_for_acquire` locals; drop the now-unused `profile: Profile` parameter (keep `after_connect`/`assert_current_database` unchanged). Update the function's doc comment (currently "on every new physical connection and — when `profile.checks_pool_identity()` — on every checkout") to describe only the creation-time assertion. Rename the test `assert_current_database_panics_on_the_mismatch_before_acquire_would_catch` to `assert_current_database_panics_on_the_mismatch` and rewrite its doc comment to drop the before_acquire framing.

`src/profile.rs`: delete `Profile::checks_pool_identity` and the `pool_identity_checks_skip_only_production` test. Leave the module doc comment as-is — it motivates `Profile` generally, not this one method.

#### Task 2 — `TenantPool` and the fixed `connect_tenant_pool`

`src/tenant/pool.rs`: add `pub struct TenantPool { pub tenant_id: Uuid, pub pool: PgPool }`. Change `connect_tenant_pool` to:

```rust
pub async fn connect_tenant_pool(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_id: Uuid,
    database_name: &str,
    max_connections: u32,
) -> Result<TenantPool, sqlx::Error>
```

Body: build `options` from `base_db_url`/`database_name` exactly as today; separately call `tenant_repo::find_by_id(control_pool, tenant_id)`, mapping `None` to `sqlx::Error::Configuration(format!("no tenant registered with id {tenant_id}").into())`; call `db::connect_with_expected_database(options, max_connections, &tenant.database_name)` using the *freshly looked-up* `tenant.database_name` as `expected_db` — never the caller's `database_name` argument; return `TenantPool { tenant_id, pool }`. Update the module doc comment to state the independence property explicitly, mirroring DESIGN.md §2.1's wording.

#### Task 3 — Update every production call site

Same shape at each: bind the caller's already-resolved `Tenant` and `control_pool`, pass `control_pool, base_db_url, tenant.id, &tenant.database_name, max_connections` (drop the trailing `profile` argument), bind the result to a `TenantPool` local, and replace every downstream bare-`PgPool` use of that local (`.close()`, `.clone()`, passing `&tenant_pool` to a helper, `sqlx::migrate!(...).run(...)`, `PgListener::connect_with(...)`) with `.pool`. Do not change the inner helpers' own signatures (they keep taking `&PgPool`):

- `src/tenant/registry.rs` (`TenantRegistry::get_or_open`)
- `src/tenant/provision.rs` (`provision_tenant`)
- `src/stats.rs`
- `src/producer/register.rs` (3 call sites)
- `src/bin/dispatcher.rs`
- `src/template/approve.rs` (4 call sites)
- `src/customer_dek/lifecycle.rs`
- `src/tenant_config/configure.rs` (2 call sites)
- `src/partition_lifecycle/lifecycle.rs`
- `src/provider_config/configure.rs` (2 call sites)

**Discovered during pickup, folded into this task:** in every file above except `src/bin/dispatcher.rs`, the enclosing function's own `profile: Profile` parameter was used *only* to forward to `connect_tenant_pool` — with that argument gone, `profile` is unused and `just lint` (`cargo clippy -- -D warnings`) fails on it. Remove `profile: Profile` from these signatures too: `register_producer`/`disable_producer`/`list_producers` (register.rs), `approve_template`/`show_template`/`list_template_versions`/`render_preview` (approve.rs), `set_tenant_config`/`show_tenant_config` (tenant_config/configure.rs), `set_provider_config`/`list_provider_config` (provider_config/configure.rs), `tenant_message_stats` (stats.rs), `run_for_tenant` (partition_lifecycle/lifecycle.rs), `pre_provision_for_tenant` (customer_dek/lifecycle.rs), `TenantRegistry::get_or_open` (registry.rs), `provision_tenant` (provision.rs). This cascades one level further, to their own callers — **`src/bin/control.rs`**, not in the original file list above, drops the `profile`/`Profile::from_env()` argument at each of these call sites. `src/bin/dispatcher.rs` keeps its `profile` local (used separately for `VaultKeyStore::connect_as_tenant`) and only drops it from the one `connect_tenant_pool` call.

#### Task 4 — Update every test call site

Same mechanical edit in `tests/tenancy.rs`, `tests/kill_switch.rs`, `tests/dispatcher.rs`, `tests/ingest.rs`, `tests/tenant_config.rs`, `tests/customer.rs`, `tests/ledger_outbox_schema.rs`, `tests/partition_lifecycle.rs`, `tests/customer_dek.rs`, `tests/stats.rs`, `tests/provider_config.rs`, plus **`tests/producer.rs` and `tests/template.rs`** (not in the original list — they call `register_producer`/`disable_producer`/`list_producers` and `approve_template`/`show_template`/`list_template_versions`/`render_preview` respectively, and per Task 3's discovery those calls drop their `profile`/`Profile::Dev` argument too). Each fixture already calls `provision_tenant` immediately before `connect_tenant_pool` — capture its returned `ProvisionOutcome.tenant_id` (currently discarded in several fixtures) and thread it through; add a `tenant_id: Uuid` field to any fixture struct (`TestTenant`, `Fixture`, …) that reconnects later in the same test. Also drop the now-unused `profile` argument at every call to `provision_tenant` and to the Task 3 outer functions across all of these files (`provision_tenant`'s own `profile` parameter is removed per Task 3).

#### Task 5 — The isolation-mechanism acceptance test

`tests/tenancy.rs`: add `connect_tenant_pool_independently_verifies_the_tenant_it_was_told_to_open` — provision two tenants A and B; call the fixed `connect_tenant_pool` with tenant A's `tenant_id` but tenant B's real `database_name` (a caller-side mis-wire: the physical connection lands on B's database while the caller's own identity label says A); assert it panics, because `expected_db` comes from a fresh `tenant_repo::find_by_id(control_pool, tenant_a.tenant_id)`, never from the `database_name` argument the caller supplied. Document in the test's doc comment, citing DESIGN.md §14, that this is deliberately the mutation-sensitive test the design specifies: reverting Task 2's fix to set `expected_db = database_name` (re-deriving both from the same input) makes this exact test go red — B's real database now equals its own claimed identity, no panic, the `result.is_err()` assertion fails.

Leave `a_mis_wired_pool_trips_the_current_database_assertion` in place — it exercises `db::connect_with_expected_database` directly, one level lower, and remains valid.

### Acceptance test

```
just build
just test
just lint
```

All green. Specifically: `cargo test --test tenancy connect_tenant_pool_independently_verifies_the_tenant_it_was_told_to_open` passes; the full `cargo test` passes, confirming every call site touched in Tasks 3–4 still compiles with unchanged behaviour. Reviewer-only manual check (not committed): temporarily apply the Task 5 mutation (`expected_db = database_name` instead of the freshly-looked-up `tenant.database_name`) and confirm the new test fails; then revert.

### Docs update (mandatory when user-facing)

No user-facing surface. DESIGN.md §2.1 already prescribes this exact fix in prose — no wording change needed there. `just docs-check` must still pass, confirming no stale cross-reference was introduced.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint` clean.
2. Docs updated per above (none needed beyond confirming `just docs-check` passes).
3. Write a summary: files touched, decisions made, anything deferred.
4. Suggest a Conventional Commit message, ticket id in brackets, e.g. `fix(tenant): derive pool identity assertion independently of connection input (T-020)`.
5. Tidy WIP commits into a small number of atomic, correctly scoped commits before presenting (root-path child).
6. Commit locally on the ticket branch; do not push or open a merge request without user approval. Present the commit message; only after approval finalize (keep the tidied history, or squash per the user's choice at approval time), verify the remote base is not behind (`git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` must print nothing), push, and open the merge request. Hand back to the user.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit found the isolation assertion in src/tenant/pool.rs compares a value to itself, so it cannot fire on a real mis-wiring; src/db.rs's own test doc comment already admits the checkout-time arm is dead.
- 2026-09-03 — TO DO → READY: plan complete
- 2026-09-03 — READY → IN DEVELOPMENT: picked up
- 2026-09-03 — plan amended inline: Task 3/4 widened — removing `profile` from `connect_tenant_pool` left it unused (only forwarded, never otherwise read) in every enclosing wrapper function, which `just lint`'s `-D warnings` would reject; removing it there cascades to those wrappers' own callers, adding `src/bin/control.rs`, `tests/producer.rs`, and `tests/template.rs` to the file list.
- 2026-09-03 — IN DEVELOPMENT → IN REVIEW: acceptance green
