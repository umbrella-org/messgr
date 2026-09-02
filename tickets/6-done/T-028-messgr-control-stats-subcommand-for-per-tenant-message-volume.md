---
id: T-028
title: messgr-control: stats subcommand for per-tenant message volume
project: messgr
depends-on: []
spawned-by: [T-027]
impact: medium
complexity: low
cost: S
---

# T-028 — messgr-control: stats subcommand for per-tenant message volume

## Outcome

After this ships, running `messgr-control stats --tenant-slug <slug>` prints, per channel
(SMS, email), the total number of `comms_request` rows and a breakdown by
`final_status` (`pending` for rows still in flight), optionally restricted to requests
created on or after `--since <date>` — without standing up any new server.

## Description

Spawned by T-027 (dropped): the original ask was a web dashboard with message-traffic KPIs,
but that meant a brand-new binary + unauthenticated HTTP server, ahead of open
correctness/security tickets (T-018, T-020, T-021, T-023, T-024) and directly against
DESIGN.md §11.1's guard on shipping any UI over this system without `AuthProvider`. This
ticket gets the same numbers a much cheaper way: a `stats` subcommand on the existing
`messgr-control` binary.

**Corrected against the actual schema (refinement).** The original filing mentioned joining
`outbox`/`comms_event` for "dispatched/delivered/failed." Re-reading
`migrations/tenant/0004_ledger_outbox_schema.sql`: `comms_request.final_status` already
holds the terminal outcome directly (`NULL` while in flight) and `channel` is
`sms | email | whatsapp` on the same row. So one query against `comms_request` alone gives
channel + status + count; `outbox` and `comms_event` are not needed for this ticket. Also
dropped the `--channel` filter flag floated in the original filing — the query always
reports both `sms` and `email` (never `whatsapp`) broken out, so there is nothing a filter
would add for a two-value set.

**Amended inline at pickup (applicability gate finding, non-blocking).** The column's
documented full vocabulary (`delivered | bounced | cancelled | suppressed_consent |
suppressed_list | unverified_address`) is aspirational — the dispatcher (`src/dispatcher/
worker.rs`, `src/dispatcher/drain.rs`, via `repo::write_terminal`) only ever writes `"sent"`,
`"failed"`, or `"expired"` today; the rest belong to the gate-chain/webhook-receipt work
still in T-020/T-021/build step 12. The query itself is unaffected (it groups by whatever
strings actually exist), but the Docs task and the acceptance test's synthetic data must not
present the full aspirational list as current behaviour — use `sent`/`failed`/`expired`/
`pending` for both.

**Corrected again at review (T-028/F3, fixed inline).** The claim above was still incomplete:
`src/dispatcher/drain.rs`'s `write_discarded` (the kill-switch engage-and-discard path, T-016,
merged before this ticket) also writes `final_status = "discarded"`. The actual current
vocabulary is `sent | failed | expired | discarded`, plus `pending` for `NULL`. Docs corrected
in the same review pass; the query and test behaviour are unaffected (group-by is
value-agnostic).

**Why this doesn't need auth.** `messgr-control`'s trust boundary is already "whoever can
run this binary has DB/Vault access" — same as every other subcommand it has today. No
listener, no new attack surface, no exception to record against §11.1 (that guard is about
UIs; this is an operator CLI, same class as the rest of `messgr-control`).

**Why this stays inside existing design boundaries.** DESIGN.md §11.4 already draws the
line for platform-console-style tooling: counts/metadata are fine to show ("per-tenant
health and volume" is explicitly listed), message *content* is not (no Transit policy, no
decryption). This ticket only ever runs `count(*)`/`GROUP BY` on `channel`/`final_status` —
it never touches `payload_ciphertext`, `destination_ciphertext`, or any DEK.

**Explicitly out of scope:** any HTTP server, any browser UI, any trend chart, any
cross-tenant aggregation in one call (loop the registry yourself, or pass `--tenant-slug`
per tenant), any `--format json` flag (not needed for the first cut) — those stay with
whatever eventually picks up DESIGN.md's real `messgr-query`/§11.2/§11.3 build-order step,
or a later ticket, not this one.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-028-messgr-control-stats-subcommand
```

Root-path child (`path = "."`): commit locally as you go, tidy into atomic commits before
presenting, do not push or open a merge request without explicit user approval.

### Prerequisite gate (hard)

None. `depends-on: []` — no other ticket's branch needs to be merged first. Working tree
must be clean on `main` before branching.

### Confirmed design decisions (do not deviate without asking)

1. **`comms_request.final_status` is the only source of truth queried.** No join to
   `outbox` or `comms_event` — the terminal outcome already lives on the ledger row
   (§4.1/§4.4), and `outbox`/`comms_event` add nothing this ticket's Outcome needs.
2. **Only `sms` and `email` are reported**, `whatsapp` rows are excluded from every query —
   no `--channel` flag; the output always shows both.
3. **No new binary, no auth.** This is a subcommand on the existing `messgr-control`
   binary, using its existing trust boundary (DB/Vault-access-implies-authorized, same as
   every other subcommand there). Do not add an HTTP listener of any kind.
4. **`--since` is optional and unbounded by default.** Omitting it reports all-time counts;
   supplying it filters to `comms_request.created_at >= <date>T00:00:00Z`.
5. **Output is plain `println!` lines, one row per (channel, total) and per
   (channel, status, count)** — matching the existing style of
   `ProducerCommand::List` (src/bin/control.rs). No table-formatting crate, no `--format
   json` in this ticket.

### Tasks

#### Task 1 — `src/stats.rs`: the query function

New file `src/stats.rs` (flat module, mirroring `src/platform_audit.rs`/`src/destination_hmac.rs`'s
single-file style — this is one query, not a domain needing its own directory). Register it
in `src/lib.rs` as `pub mod stats;` (alphabetical position: after `sender`, before
`template`).

```rust
//! Per-tenant message-volume counts for `messgr-control stats` (DESIGN.md
//! §11.4: counts/metadata for platform tooling, never payload content;
//! T-028). Reads `comms_request.final_status` directly -- no join to
//! `outbox`/`comms_event` needed, since the terminal outcome already lives
//! on the ledger row (§4.1).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::profile::Profile;
use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelStatusCount {
    pub channel: String,
    pub status: String,
    pub count: i64,
}

/// Resolves `tenant_slug`, opens its pool, runs the count query, closes the
/// pool. Mirrors `partition_lifecycle::lifecycle::run_for_tenant`'s
/// resolve/connect/close shape so `src/bin/control.rs` stays as thin as
/// every other command.
pub async fn tenant_message_stats(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    since: Option<NaiveDate>,
    profile: Profile,
) -> Result<Vec<ChannelStatusCount>, sqlx::Error> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no tenant registered with slug {tenant_slug:?}").into(),
            )
        })?;

    let tenant_pool =
        connect_tenant_pool(base_db_url, &tenant.database_name, 5, profile).await?;

    let since_ts: Option<DateTime<Utc>> = since
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc());

    let result = sqlx::query_as::<_, ChannelStatusCount>(
        r#"
        SELECT
            channel,
            coalesce(final_status, 'pending') AS status,
            count(*) AS count
        FROM comms_request
        WHERE channel IN ('sms', 'email')
          AND ($1::timestamptz IS NULL OR created_at >= $1)
        GROUP BY channel, status
        ORDER BY channel, status
        "#,
    )
    .bind(since_ts)
    .fetch_all(&tenant_pool)
    .await;

    tenant_pool.close().await;
    result
}
```

#### Task 2 — wire the subcommand into `src/bin/control.rs`

Add `use messgr::stats::tenant_message_stats;` to the imports. Add to the `Command` enum
(after the `PartitionLifecycle` variant):

```rust
/// Message-volume counts by channel and status for one tenant (DESIGN.md
/// §11.4: counts/metadata only, never payload content). Reads
/// `comms_request.final_status` directly. T-028.
Stats {
    #[arg(long = "tenant-slug")]
    tenant_slug: String,
    /// Only count requests created on or after this date (UTC). Omit for
    /// all-time.
    #[arg(long)]
    since: Option<chrono::NaiveDate>,
},
```

Add the match arm (after `Command::PartitionLifecycle { .. } => ...`):

```rust
Command::Stats { tenant_slug, since } => {
    let rows = tenant_message_stats(
        &control_pool,
        &config.control_database_url,
        &tenant_slug,
        since,
        config.profile,
    )
    .await
    .unwrap_or_else(|err| {
        panic!("failed to compute stats for tenant {tenant_slug:?}: {err}")
    });

    let mut channels: Vec<&str> = rows.iter().map(|r| r.channel.as_str()).collect();
    channels.sort_unstable();
    channels.dedup();

    for channel in channels {
        let total: i64 = rows
            .iter()
            .filter(|r| r.channel == channel)
            .map(|r| r.count)
            .sum();
        println!("{channel} total={total}");
        for row in rows.iter().filter(|r| r.channel == channel) {
            println!("{channel} status={} count={}", row.status, row.count);
        }
    }
}
```

#### Task 3 — acceptance test: `tests/stats.rs`

New file, following `tests/ledger_outbox_schema.rs`'s conventions exactly (real
provisioning against the local stack via `provision_tenant`, no mocks; its own copies of
`control_database_url`/`vault_keystore`/`unique_name`/`drop_test_tenant`/`TestTenant`
helpers — this project does not share test helpers across files). Insert `comms_request`
rows directly (extend `ledger_outbox_schema.rs`'s `insert_comms_request` helper shape to
also take `channel: &str` and `final_status: Option<&str>`), then assert
`tenant_message_stats` returns the expected `(channel, status, count)` triples using only
values the dispatcher actually writes today (`sent`, `failed`, `expired`) plus `NULL` →
`pending`: e.g. 2 `sms`/`sent`, 1 `sms`/`pending` (`final_status = NULL`), 1 `email`/`failed`,
and 1 `whatsapp`/`sent` row inserted but **not** present in the result (channel filter
proven). Also assert `--since` in the future excludes everything and `--since` in the past
includes everything, using two rows with distinct `created_at` values.

### Acceptance test

```
just build
just test --test stats
just lint
```

The new `#[tokio::test]`s in `tests/stats.rs` pass, in particular: (a) counts match exactly
for a known set of inserted rows across `sms`/`email`/`whatsapp` with `whatsapp` excluded
from the result; (b) a `NULL final_status` row is reported as `status=pending`; (c) `--since`
filtering is exact at the boundary (a row with `created_at` equal to the `since` timestamp
is included; one microsecond before is excluded). Manual smoke check:
`cargo run --bin messgr-control -- stats --tenant-slug <a provisioned dev tenant>` prints
non-empty output with no panic against the local dev stack.

### Docs update (mandatory when user-facing)

Add a `== Message stats` section to `docs/user-manual/control-plane-cli.adoc` (after `==
Partition lifecycle`, matching its style: a `[source,bash]` example line, then prose
explaining what `total`/`status=`/`count=` mean, that `whatsapp` is intentionally excluded,
and that this reads `comms_request` directly with no join). Document the status values as
they exist **today** — `sent`, `failed`, `expired`, `pending` (`final_status IS NULL`) — not
the full aspirational vocabulary in DESIGN.md §4.4; note that `delivered`/`bounced` and the
gate-outcome statuses land once the webhook-receipt path and T-020/T-021's gate chain ship.
Run `just docs-check` and fix anything it flags.

### Finish (mandatory)

1. Acceptance test green; `just build`, `just test`, `just lint`, `just docs-check` clean.
2. Docs updated and registered (Task above).
3. Write a summary: files touched (`src/stats.rs` new, `src/lib.rs` +1 line,
   `src/bin/control.rs` +subcommand, `tests/stats.rs` new,
   `docs/user-manual/control-plane-cli.adoc` +section), decisions made, anything deferred.
4. Suggested commit message:
   ```
   feat(control): add stats subcommand for per-tenant message volume (T-028)
   ```
5. Tidy WIP commits into a small number of atomic commits (root-path child).
6. Commit locally on `feat/T-028-messgr-control-stats-subcommand`. Do not push or open a
   merge request without explicit user approval. Present the commit message; after
   approval, verify `origin/main` is not behind (`git fetch origin main && git diff
   --name-only origin/main...HEAD | grep '^tickets/'` must print nothing), then push and
   open the merge request. Merging is the human's.

## Review

**Reviewer independence (step 0):** delegated. The orchestrating reviewer authored the
`feat/T-028-messgr-control-stats-subcommand` branch in this same session, so audits (steps
2-4a) were run by an independent, freshly-spawned sub-agent, briefed adversarially with the
ticket, the branch, and `AGENTS.md`/`review-addendum.md`. Every finding it returned was
independently re-verified by hand before being recorded below (ran the exact commands
myself; mutation-tested F4; read the cited code myself) — delegation buys independence, not
accuracy.

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | `cargo clippy --all-targets --all-features -- -D warnings` (CI's actual clippy job) fails: `needless_lifetimes` in `tests/stats.rs`'s `count_of`. `just lint` doesn't lint test targets, so this passed local verification while failing CI. | `.github/workflows/ci.yml:29`; reproduced: `error: could not compile messgr (test "stats")` at `tests/stats.rs:170` | Elide the lifetime: `fn count_of(rows: &[...], ...)`. |
| F2 | blocking | correctness | — | `cargo fmt --all -- --check` (CI's actual fmt job) fails on this branch's new code — unformatted diffs in `src/bin/control.rs:914` and three call sites in `tests/stats.rs`. | `.github/workflows/ci.yml:19`; reproduced diff via `cargo fmt --all -- --check` | Run `cargo fmt --all` and commit the result. |
| F3 | non-blocking | docs-gap | fixed inline | Ticket's Description and the new docs section both claimed `final_status` is only ever `sent`/`failed`/`expired` today. `src/dispatcher/drain.rs`'s `write_discarded` (kill-switch discard path, T-016, predates this ticket) also writes `"discarded"`. Verified by reading `drain.rs:187-198,271` and grepping every `write_terminal`/`write_discarded`/`write_expired` call site in the tree. | `src/dispatcher/drain.rs:187-198`, called from the kill-switch drain loop at `:271` | Corrected `docs/user-manual/control-plane-cli.adoc` (commit `9d12234` on the feature branch) and this ticket's Description (above) to name `discarded`. No behaviour change — the query is value-agnostic. |
| F4 | non-blocking | test-gap | noted | The acceptance test's own stated criterion ("`--since` filtering is exact at the boundary") is not actually exercised — both test rows are hours away from the midnight boundary the `since` filter tests against. Mutation-tested: changing `created_at >= $1` to `created_at > $1` in `src/stats.rs` still passes both tests. | `tests/stats.rs:213-262`; mutation test run and reverted (`git diff --stat src/stats.rs` clean after) | A future strengthening could insert a row at exactly `since_ts` and one at `since_ts - 1µs`. Not scheduled now — too small to be its own ticket. |
| F5 | non-blocking | design | folded → T-025 | `tenant_message_stats` returns bare `Result<_, sqlx::Error>` for the unknown-tenant-slug case, unlike every other same-shaped "resolve slug → connect → act" function (`partition_lifecycle::run_for_tenant`, `producer::register`, `provider_config::configure`, `tenant_config::configure`), which wrap it in a domain error type. Functionally harmless (the `Configuration` variant's `Display` still surfaces a clear message). | `src/stats.rs:31` vs `src/partition_lifecycle/lifecycle.rs:75` | Folded into T-025 item 10 (same "pick one error-handling policy" theme as its items 3/6) rather than fixed ad hoc. |
| F6 | non-blocking | design | noted | No index covers `channel` or `(channel, created_at)` on `comms_request`; the new query is a sequential scan over the tenant's ledger. Explicitly out of this ticket's stated scope (a read-only, operator-invoked reporting command, not a hot path), and messgr-control commands are not called per-request, so this doesn't clear the "would actually be scheduled" bar on its own today. | `migrations/tenant/0004_ledger_outbox_schema.sql:32` (only `(final_status, created_at DESC)` and the partial `campaign_id` index exist) | Revisit if `stats` is ever called on a schedule/frequently, or once ledger sizing (T-019) gives a concrete per-tenant row-count number to judge against. |

Areas the independent audit checked with no defect: all five plan tasks present as specified;
all five confirmed design decisions honoured (no `outbox`/`comms_event` join, `whatsapp`
excluded with no `--channel` flag, no new binary/listener — grepped for
`TcpListener`/`axum`/`actix`/`bind`, `--since` optional/unbounded, plain `println!` output);
`src/lib.rs` module ordering; no SQL injection (parameterized throughout, explicit
`$1::timestamptz` cast avoids sqlx's type-inference failure); no connection-pool leak
(`tenant_pool.close().await` runs on every path; the unknown-slug path never opens one); no
message content or secrets read; schema match confirmed against
`migrations/tenant/0004_ledger_outbox_schema.sql`; docs placement/content otherwise correct
including a `cargo run --bin messgr-control -- stats --help` cross-check; review-addendum's
NULL-index/Vault-secrets/lease-release/new-column items not applicable (no table, index,
column, secret, or lease added); board/ticket bookkeeping correctly on `main`, code on the
feature branch.

Re-ran the full acceptance test plus build/lint/docs commands myself after independent
verification: `just build` (exit 0), `just test` — full suite, no regressions (exit 0),
`cargo test --test stats` (exit 0 — the ticket's literal `just test --test stats` does not
exist as a recipe; `justfile`'s `test` recipe doesn't forward args, itself worth noting but
not blocking this ticket), `just lint` (exit 0, but does not catch F1 — see above),
`just docs-check` (exit 0). `cargo clippy --all-targets --all-features -- -D warnings` and
`cargo fmt --all -- --check` (CI's actual jobs) both fail, per F1/F2.

**Disposition summary:** 2 blocking (F1, F2) → `5-rework/`. 4 non-blocking: 1 fixed inline
(F3), 1 noted (F4), 1 folded into T-025 (F5), 1 noted (F6).

cost: estimated S, actual S — the two blocking findings are one `cargo fmt --all` run and a
one-line lifetime elision; no scope growth.

### Rework fix record — round 1 (commit db1257e)

Fixed F1 (elided the needless lifetime in `tests/stats.rs`'s `count_of`) and F2 (ran
`cargo fmt --all`, formatting `src/bin/control.rs`'s `channels` collect line and three
`insert_comms_request` call sites in `tests/stats.rs`). Re-ran and confirmed clean:
`just build`, `cargo test --test stats`, `just test` (full suite, no regressions),
`just lint`, `just docs-check`, plus CI's actual jobs —
`cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all -- --check`
— both exit 0. No other findings touched; F3/F4/F5/F6 stand as recorded above.

## History

- 2026-09-02 — created (TO DO). source: review: T-027 (unauthenticated new web binary,
  wrong shape for the actual need) was dropped in favor of this cheaper CLI-only
  alternative that reuses `connect_tenant_pool` and stays inside §11.4's metadata-only
  boundary.
- 2026-09-02 — TO DO → READY: plan complete.
- 2026-09-02 — plan amended inline: applicability-gate audit (independent agent) found the
  Description/Docs/Acceptance-test steps used `final_status` values (`delivered`, `bounced`,
  etc.) that DESIGN.md §4.4 documents but the dispatcher does not yet write — only `sent`,
  `failed`, `expired` exist today. Corrected the Description, the Docs task, and the
  acceptance test's synthetic data to use the real current vocabulary; the query logic
  itself was unaffected (group-by is value-agnostic).
- 2026-09-02 — TO DO → READY: plan complete
- 2026-09-02 — READY → IN DEVELOPMENT: picked up
- 2026-09-02 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-02 — IN REVIEW → REWORK: F1/F2: CI clippy (all-targets) and fmt checks fail
- 2026-09-02 — REWORK → IN REVIEW: findings fixed
- 2026-09-02 — IN REVIEW → DONE: review clean after rework; 4 non-blocking findings all dispositioned (1 fixed inline, 1 folded into T-025, 2 noted)
- 2026-09-02 — MR #20 opened (`feat/T-028-messgr-control-stats-subcommand`), awaiting merge
