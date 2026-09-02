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

<!-- empty until IN REVIEW -->

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
