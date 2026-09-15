---
id: T-030
title: Reconcile orphan_event rows into comms_event
project: messgr
depends-on: []
spawned-by: [T-022]
impact: low
complexity: medium
cost: M
---

# T-030 — Reconcile orphan_event rows into comms_event

## Outcome

A background reconciliation job periodically retries `orphan_event` rows (delivery-receipt
webhooks that arrived before, or without, a matching `comms_request`), promoting each match into
a real `comms_event` row and aging out ones that never match. `orphan_event` stops being a schema
with zero readers.

## Description

T-022 (DESIGN.md §4.4/§10, `development/design/09-delivery-receipts.md`) shipped the
`orphan_event` table schema-only, by design (decision 5): "a receipt for an unknown
`provider_ref` goes to a small `orphan_event` table and is reconciled on a short delay rather
than discarded" — but nothing yet reads or writes to it outside the ticket's own round-trip test,
and no ticket owned building the reconciliation job itself. Spawned during T-022's review (finding
F4, the same class of gap T-022 itself found and filed as T-029 for the idempotency-sweep job) so
it doesn't drift a third time.

**Correction made at refinement:** this Description originally said matching happens against
`comms_request` rows directly — wrong. `comms_request` carries no `provider_ref` column at all
(only `comms_event` does, written once the dispatcher's own send attempt gets a provider
response with a `provider_ref`, via `write_terminal`). The only real match target is
`comms_event.provider_ref`, recovering `comms_request_id`/`customer_id` from the row that
matches.

Scope: a job (matching T-029/T-014's polling-job shape) that periodically re-attempts matching
each `orphan_event.provider_ref` against `comms_event.provider_ref` rows written since, on a
short delay per the design note above. On match: encrypt `orphan_event.provider_payload_raw`
under the matched customer's DEK (AGENTS.md hard invariant 7 — this write is the payload's
*first* write, since `orphan_event` predates knowing the customer, so it must go through the
same DEK-encryption path every other payload write already does), insert the equivalent
`comms_event` row with that ciphertext, conditionally update `comms_request.final_status` (guard
against a late/out-of-order receipt regressing an already-more-final status — see decision 3
below), and delete the `orphan_event` row (its raw payload gone once encrypted and promoted).
Rows that exceed a fixed `reconcile_attempts` cap age out by deletion — see decision 4.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-030-orphan-event-reconcile
```

### Prerequisite gate (hard)

None. No `depends-on:`; nothing else must land first. Independent of `messgr-webhook`
(build-order step 12, not yet built) — see decision 5.

### Confirmed design decisions (do not deviate without asking)

1. **Match target is `comms_event.provider_ref`, not `comms_request`** (correction to this
   ticket's original Description — `comms_request` has no `provider_ref` column).
2. **Promotion encrypts the raw payload under the matched customer's DEK before writing it.**
   `orphan_event.provider_payload_raw` is this payload's first write under a customer scope
   (AGENTS.md hard invariant 7); reuse `customer_dek::lifecycle::get_or_create_dek` +
   `encryption::encrypt` (aad = `comms_request_id`'s raw bytes, matching every other
   `comms_request`/`comms_event` payload write's convention) — do **not** reuse
   `dispatcher::repo::write_terminal` as-is, since it always writes `NULL` ciphertext and has no
   DEK-lookup step.
3. **`comms_request.final_status` only advances, never regresses.** A small fixed ordering —
   `queued` < `sent` < `delivered` < `read` — plus an "absorbing" set that always wins
   regardless of current status (`failed`, `bounced`, `complaint`, `expired`, `cancelled`,
   `suppressed_consent`, `suppressed_list`, `unverified_address`, since these are
   compliance-relevant and must never be silently dropped by an earlier, more benign status).
   `final_status IS NULL` always advances. This guards against a late, out-of-order receipt
   (the whole reason `orphan_event` exists) overwriting an already-more-final status.
4. **`reconcile_attempts` cap is 5, then delete.** A named constant
   (`RECONCILE_ATTEMPTS_CAP: u16 = 5`), not a magic number — this job is a one-shot
   `messgr-control` subcommand invoked by cron (decision 5), not a continuous poll loop, so 5
   attempts means 5 separate cron invocations before giving up; tune later if the real cron
   interval makes that window too short or too long.
5. **Mirrors T-029's shape exactly**: a `messgr-control` subcommand, per-tenant, one-shot, no
   daemonizing. Built now, independent of `messgr-webhook` (the only future real writer to
   `orphan_event`) — the reconciliation logic is fully testable today via directly-inserted
   `orphan_event` rows (`tests/producer.rs`'s and T-022's own round-trip test's convention), and
   having it ready means no gap once `messgr-webhook` ships.

### Tasks

#### Task 1 — index `comms_event.provider_ref` (new migration)

`comms_event` has no standalone index on `provider_ref` (only the composite
`UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)`), so every reconcile match
would otherwise scan every partition. New migration
`migrations/tenant/0011_comms_event_provider_ref_index.sql`:

```sql
CREATE INDEX ON comms_event (provider_ref);
```

#### Task 2 — reconciliation module (`src/orphan_reconcile/`)

`src/orphan_reconcile/repo.rs` — queries:

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub struct PendingOrphan {
    pub id: Uuid,
    pub provider_ref: String,
    pub event_type: String,
    pub provider_status: Option<String>,
    pub provider_payload_raw: Option<serde_json::Value>,
    pub occurred_at: DateTime<Utc>,
    pub reconcile_attempts: i16,
}

pub struct Match {
    pub comms_request_id: Uuid,
    pub comms_request_created_at: DateTime<Utc>,
    pub customer_id: Uuid,
    pub current_final_status: Option<String>,
}

pub async fn list_pending(pool: &PgPool) -> Result<Vec<PendingOrphan>, sqlx::Error> { /* SELECT * FROM orphan_event */ }

pub async fn find_match(pool: &PgPool, provider_ref: &str) -> Result<Option<Match>, sqlx::Error> {
    // SELECT cr.id, cr.created_at, cr.customer_id, cr.final_status
    // FROM comms_event ce JOIN comms_request cr ON cr.id = ce.comms_request_id
    // WHERE ce.provider_ref = $1 LIMIT 1
}

pub async fn promote(/* pool, orphan id, comms_request_id+created_at, customer_id, event_type,
    provider_status, ciphertext, occurred_at, whether to advance final_status, new final_status */)
    -> Result<(), sqlx::Error> {
    // one transaction: INSERT INTO comms_event (... provider_payload_ciphertext) VALUES (...)
    //   ON CONFLICT (occurred_at, comms_request_id, event_type, provider_ref) DO NOTHING;
    // conditionally UPDATE comms_request SET final_status = $, finalized_at = $
    //   WHERE created_at = $ AND id = $ (only when decision 3's guard says advance);
    // DELETE FROM orphan_event WHERE id = $1
}

pub async fn record_miss(pool: &PgPool, orphan_id: Uuid, cap: i16) -> Result<(), sqlx::Error> {
    // UPDATE orphan_event SET reconcile_attempts = reconcile_attempts + 1 WHERE id = $1;
    // DELETE FROM orphan_event WHERE id = $1 AND reconcile_attempts >= $2
}
```

`src/orphan_reconcile/reconcile.rs` — orchestration:

```rust
use std::sync::Arc;
use sqlx::PgPool;
use crate::customer_dek::lifecycle::get_or_create_dek;
use crate::encryption;
use crate::key_cache::KeyCache;
use crate::keystore::KeyStore;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use super::repo::{self, PendingOrphan};

pub const RECONCILE_ATTEMPTS_CAP: i16 = 5;

const STATUS_ORDER: &[&str] = &["queued", "sent", "delivered", "read"];
const ABSORBING_STATUSES: &[&str] = &[
    "failed", "bounced", "complaint", "expired", "cancelled",
    "suppressed_consent", "suppressed_list", "unverified_address",
];

fn should_advance(current: Option<&str>, new_event_type: &str) -> bool {
    match current {
        None => true,
        Some(_) if ABSORBING_STATUSES.contains(&new_event_type) => true,
        Some(cur) => matches!(
            (STATUS_ORDER.iter().position(|s| *s == cur),
             STATUS_ORDER.iter().position(|s| *s == new_event_type)),
            (Some(c), Some(n)) if n > c
        ),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileReport {
    pub reconciled: u64,
    pub aged_out: u64,
    pub still_pending: u64,
}

pub async fn run_for_tenant(
    control_pool: &PgPool,
    control_database_url: &str,
    tenant_slug: &str,
    keystore: &dyn KeyStore,
    database_max_connections: u32,
) -> Result</* report, error */> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug).await? /* ... */;
    let tenant_pool = connect_tenant_pool(control_pool, control_database_url, tenant.id,
        &tenant.database_name, database_max_connections).await?.pool;
    let cache = KeyCache::new(/* small fixed capacity, e.g. 1000 */, /* short TTL, e.g. 60s — a
        single one-shot run, not a long-lived process */);

    let mut report = ReconcileReport::default();
    for orphan in repo::list_pending(&tenant_pool).await? {
        match repo::find_match(&tenant_pool, &orphan.provider_ref).await? {
            Some(m) => {
                let dek = get_or_create_dek(&tenant_pool, keystore, &cache, &tenant.vault_mount, m.customer_id).await?;
                let ciphertext = orphan.provider_payload_raw.as_ref().map(|raw| {
                    encryption::encrypt(&dek, m.comms_request_id.as_bytes(), &serde_json::to_vec(raw).expect("jsonb always serializes"))
                }).transpose()?;
                let advance = should_advance(m.current_final_status.as_deref(), &orphan.event_type);
                repo::promote(&tenant_pool, &orphan, &m, ciphertext, advance).await?;
                report.reconciled += 1;
            }
            None if orphan.reconcile_attempts + 1 >= RECONCILE_ATTEMPTS_CAP => {
                repo::record_miss(&tenant_pool, orphan.id, RECONCILE_ATTEMPTS_CAP).await?;
                report.aged_out += 1;
            }
            None => {
                repo::record_miss(&tenant_pool, orphan.id, RECONCILE_ATTEMPTS_CAP).await?;
                report.still_pending += 1;
            }
        }
    }
    Ok(report)
}
```

Register in `src/lib.rs`: add `pub mod orphan_reconcile;` (alphabetical, between `mtls` and
`partition_lifecycle`).

#### Task 3 — `messgr-control orphan-reconcile run` subcommand (`src/bin/control.rs`)

Mirrors `IdempotencySweep`'s shape (T-029, same file) but **does** connect to Vault (admin-token
client, `VaultKeyStore::connect(config.profile)` — matching `messgr-ingest`'s own T-011 decision
5 rationale: this is a per-invocation, multi-tenant-over-time CLI, not the single-tenant AppRole
case `connect_as_tenant` fits):

```rust
OrphanReconcile { command: OrphanReconcileCommand },
```
```rust
#[derive(Subcommand)]
enum OrphanReconcileCommand {
    /// Match pending orphan_event rows against comms_event.provider_ref, promote
    /// matches into comms_event (encrypted under the customer's DEK), age out rows
    /// past the reconcile_attempts cap.
    Run {
        #[arg(long = "tenant-slug")]
        tenant_slug: String,
    },
}
```
Handler connects Vault lazily in this arm only (matching the `Provision`/dev-pki arms' existing
pattern), then calls `orphan_reconcile::reconcile::run_for_tenant`, printing
`reconciled=<n> aged_out=<n> still_pending=<n>`.

#### Task 4 — docs (`docs/user-manual/control-plane-cli.adoc`)

New section after `idempotency-sweep`'s (Task 3 of T-029), same structure: command line,
one paragraph explaining the match/promote/age-out behavior and the cron/systemd-timer
invocation model, cross-referencing DESIGN.md's delivery-receipts section and T-030.

### Acceptance test

New file `tests/orphan_reconcile.rs`, following `tests/partition_lifecycle.rs`/`tests/keystore.rs`
conventions (real tenant provisioning, real Vault dev-mode Transit, no mocks):

1. **Match and promote**: provision a tenant, insert a real `comms_request` + a matching
   `comms_event` row carrying `provider_ref = "abc"`, insert an `orphan_event` row with the same
   `provider_ref` and a `provider_payload_raw` JSON body. Run `reconcile::run_for_tenant`. Assert:
   the `orphan_event` row is gone, a new `comms_event` row exists with a non-NULL
   `provider_payload_ciphertext` that decrypts (via the same customer DEK) back to the original
   JSON, and `comms_request.final_status` advanced per decision 3.
2. **final_status regression guard**: same setup, but `comms_request.final_status` is already
   `delivered` and the orphan's `event_type` is `sent` (not in the absorbing set) — assert
   `final_status` is unchanged after promotion.
3. **No match, under cap**: an `orphan_event` row whose `provider_ref` matches nothing — assert
   `reconcile_attempts` incremented by 1, row still present.
4. **No match, cap exhausted**: an `orphan_event` row with `reconcile_attempts = 4` (one below
   the cap) and no match — assert the row is deleted after this run.

Run:

```
just build
just test    # includes tests/orphan_reconcile.rs
just lint
just docs-check
```

### Docs update (mandatory when user-facing)

`docs/user-manual/control-plane-cli.adoc` — see Task 4 above.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated per Task 4.
3. Write a summary: files touched (`migrations/tenant/0011_comms_event_provider_ref_index.sql`,
   `src/orphan_reconcile/{repo.rs,reconcile.rs}`, `src/lib.rs`, `src/bin/control.rs`,
   `tests/orphan_reconcile.rs`, `docs/user-manual/control-plane-cli.adoc`), decisions made (the
   five above), anything deferred (tuning `RECONCILE_ATTEMPTS_CAP` against the real cron
   interval, once `messgr-webhook` ships and gives real traffic to observe).
4. Suggested commit message:

   ```
   feat(control): add orphan-event reconciliation (T-030)

   Matches pending orphan_event rows against comms_event.provider_ref,
   promotes matches into a real comms_event row encrypted under the
   customer's DEK, advances comms_request.final_status without
   regressing it, and ages out rows past a 5-attempt reconcile cap.
   ```

5. Root-path child (`path = "."`) — tidy WIP commits into atomic ones before presenting.
6. Commit locally on `feat/T-030-orphan-event-reconcile`. Publish only per commit policy (no
   push/MR without user approval). Present the commit message; after approval, verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing, then push and open the MR. Hand back to the user.

## Review

**Reviewer independence (step 0):** delegated. The reviewing agent authored this branch in the
same session, so the audits (steps 2–4a) were run by an independent, freshly spawned agent,
briefed adversarially (find defects, not confirm), with no memory of writing the code. Every
delegated finding below was re-verified by hand before being recorded, per step 0's "delegation
buys independence, not accuracy" — F1 was independently reproduced with a throwaway test
(inserted, run, confirmed the cross-customer mismatch, then discarded — repo left clean); F2/F3's
`DESIGN.md` citation was independently grepped and confirmed verbatim; F2's "no alerting" claim
was independently confirmed by `grep -rn "tracing::" src/orphan_reconcile/` (no matches).

**Implementation audit (step 2):** all 5 confirmed design decisions and all 4 tasks implemented,
in the files the plan names. Acceptance test re-run verbatim: `cargo test --test
orphan_reconcile` — 4/4 pass. `just build` / `just test` (full suite, 60+ tests) / `just lint`
clean. Mutation-tested the four acceptance-test assertions (flip `should_advance` to always
`true`, no-op `record_miss`, skip encryption in `promote_match`) — each went red on the
mechanism it claims to cover; none is tautological. AGENTS.md's ten hard invariants swept in
full: invariant 7 (per-customer DEKs from first write) is this ticket's central claim and holds
— the `comms_event` INSERT and `orphan_event` DELETE happen in one transaction
(`src/orphan_reconcile/repo.rs:88-127`), so there is no window where the plaintext orphan row
outlives the encrypted copy. Invariant 2 (ledger self-contained) holds — `customer_id` is
written directly from the match, no join-dependent read. Invariant 6 (erasure coverage) is a
no-op — no new table; `comms_event`'s existing erasure statement already covers a promoted row,
and `orphan_event`'s exemption text already names T-030.

**Quality / consistency audits (steps 3–4):** idiomatic, correctly avoids reusing
`dispatcher::repo::write_terminal` as-is (confirmed its `NULL`-ciphertext/no-DEK-lookup bug is
not duplicated, `src/dispatcher/repo.rs:216-264` vs `src/orphan_reconcile/repo.rs:81-128`);
`run_for_tenant`'s resolve/connect/close shape faithfully mirrors
`src/idempotency_sweep.rs:39-64`; `src/lib.rs` module registration correctly alphabetical. No
`justfile`/CI workflow changes in this diff (addendum step 2 item 8 not applicable).

**Documentation audit (step 4a):** new `== Orphan-event reconciliation` section in
`docs/user-manual/control-plane-cli.adoc`, matching the idempotency-sweep section's structure,
content verified accurate against the code. `just docs-check` passes.

**Docs-readability pass (step 4b):** conscious skip — no docs-readability reviewer configured
in this host session.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | `find_match` has no guard against `provider_ref = ''`, the sentinel every dispatch-internal `comms_event` row carries by default (`migrations/tenant/0004_ledger_outbox_schema.sql:72`); an `orphan_event` row with an empty `provider_ref` matches an arbitrary unrelated `comms_request`/`comms_event` row, causing that stranger's `final_status` to be mutated and the orphan's third-party payload to be encrypted under the stranger's DEK | `src/orphan_reconcile/repo.rs:44-59`; independently reproduced: an orphan row with `provider_ref = ''` alongside an unrelated victim's dispatch-internal `comms_event` row (also `provider_ref = ''`) flipped the victim's `comms_request.final_status` from `NULL` to `"delivered"` | add `AND ce.provider_ref <> ''` to `find_match`'s WHERE clause; add a regression test inserting a dispatch-internal `''` `comms_event` row alongside an unrelated orphan and asserting no match |
| F2 | non-blocking | design | new ticket (T-033) | cap-exhaustion deletes the `orphan_event` row with zero alerting, contradicting `DESIGN.md`'s own stated rationale that `reconcile_attempts` exists so an exhausted row "pages someone" | `development/design/03-data-model.md:196`; `grep -rn "tracing::" src/orphan_reconcile/` → no matches; cf. `src/partition_lifecycle/lifecycle.rs:160`'s `tracing::warn!` precedent for an analogous silent-loss condition | emit `tracing::warn!` when a row ages out — batched with F3 into T-033 |
| F3 | non-blocking | plan-wrong | new ticket (T-033) | `RECONCILE_ATTEMPTS_CAP` is a hardcoded Rust constant; `DESIGN.md` explicitly requires it be "config, not hardcoded" | `development/design/03-data-model.md:196`; `src/orphan_reconcile/reconcile.rs:24` `pub const RECONCILE_ATTEMPTS_CAP: i16 = 5` | move the cap into `tenant_config` (the existing `kill_switch_release_rate` pattern) — batched with F2 into T-033 |
| F4 | non-blocking | spec-unclear | new ticket (T-034) | `orphan.event_type` is written verbatim into `comms_request.final_status` with no validation it is a recognized status/event value — the first path where that value can originate from third-party (future webhook) input | `src/orphan_reconcile/reconcile.rs:46` (`should_advance`'s `None => true` arm); `src/orphan_reconcile/repo.rs:114` (`promote` binds `orphan.event_type` directly); no `CHECK` constraint on either table's `event_type` (`migrations/tenant/0004_ledger_outbox_schema.sql`) | validate `orphan.event_type` against the documented set before promoting; treat an unrecognized value as a non-match — batched with F5 into T-034 |
| F5 | non-blocking | test-gap | new ticket (T-034) | `find_match`'s `LIMIT 1` has no `ORDER BY`, so its row choice is nondeterministic and untested when multiple `comms_event` rows share a `provider_ref` | `src/orphan_reconcile/repo.rs:44-59` | add `ORDER BY occurred_at DESC` and a test pinning multi-match behavior — batched with F4 into T-034 |

Disposition summary: 1 blocking (F1 — fixed via rework, not dispositioned); 4 non-blocking, all
`new ticket` — F2+F3 batched into T-033 (orphan-reconcile cap alerting + configurability), F4+F5
batched into T-034 (orphan-reconcile input validation + match determinism).

cost: estimated M, actual M

### Rework fix record — round 1 (commit 63a62aa)

Fixed F1 only, on `feat/T-030-orphan-event-reconcile` (branch tip before this fix: `d8ba736`).
`find_match`'s WHERE clause gained `AND ce.provider_ref <> ''`
(`src/orphan_reconcile/repo.rs`), so an orphan row with an empty `provider_ref` now finds no
match instead of matching an arbitrary unrelated request. Added
`empty_provider_ref_never_matches_the_dispatch_internal_sentinel`
(`tests/orphan_reconcile.rs`), reproducing the exact failure mode found in review: a victim
request with a dispatch-internal `comms_event` row (`provider_ref = ''`) alongside an unrelated
orphan row whose own `provider_ref` is also `''` — asserts no match, the victim's
`final_status` stays untouched, and the orphan row remains pending rather than being wrongly
promoted. Re-ran verbatim: `cargo test --test orphan_reconcile` (5/5 pass, up from 4),
`just build`/`just test` (full suite)/`just lint`/`just docs-check` all clean. No other files
touched.

### Scoped re-review — round 1

**Reviewer independence:** delegated (same-session authorship of the fix commit). An
independent, freshly spawned reviewer verified F1's fix and audited its own replacement text —
not a re-audit of the whole feature. Re-verified by hand before recording: repo re-checked
clean on `feat/T-030-orphan-event-reconcile` at `63a62aa`, `cargo test --test orphan_reconcile`
independently re-run (5/5 pass).

**F1: confirmed fixed**, with mutation-test evidence — reverting the SQL guard by hand made
`empty_provider_ref_never_matches_the_dispatch_internal_sentinel` go red
(`report.still_pending` `1` vs expected `0`); restoring it (file byte-identical to `63a62aa`)
made it green again. Scope discipline held: `git show 63a62aa --stat` touches exactly
`src/orphan_reconcile/repo.rs` and `tests/orphan_reconcile.rs`, nothing else.

**Fix's own replacement text audited for new defects — none found.** SQL guard reasoning
verified structurally sound (`ce.provider_ref = $1 AND ce.provider_ref <> ''` cannot both hold
when `$1 = ''`, and is a no-op for any non-empty `$1`; both columns are `NOT NULL`, no NULL-
semantics surprise). The new test's four assertions (`report.reconciled == 0`,
`report.still_pending == 1`, victim's `final_status` untouched, orphan row still present) are
specific, not tautological. `find_match` has exactly one call site
(`src/orphan_reconcile/reconcile.rs:168`); no other path bypasses the guard. F2–F5 (already
spawned as T-033/T-034) correctly left out of scope, not re-litigated. `just lint` and
`just docs-check` re-confirmed clean.

**Verdict: no blocking findings remain.** Proceeding to `6-done/`.

## History

- 2026-09-04 — created (TO DO). source: review: T-022's review (finding F4) found `orphan_event`'s reconciliation job named in design but never ticketed, unlike the parallel idempotency-sweep gap T-022 itself filed as T-029 — filed here rather than left to drift a third time.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
- 2026-09-15 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-15 — IN REVIEW → REWORK: F1 blocking: find_match has no guard against the provider_ref='' sentinel
- 2026-09-15 — REWORK → IN REVIEW: findings fixed
- 2026-09-15 — IN REVIEW → DONE: scoped re-review clean: F1 fixed, no new findings; non-blocking F2-F5 spawned as T-033/T-034
- 2026-09-15 — PR #48 opened (`feat/T-030-orphan-event-reconcile` → `main`), pending merge.
