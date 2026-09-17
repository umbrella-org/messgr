# Overview and architecture (§1–2)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

# messgr — Design

**Version 3** · 2026-09-03 · `0d8f82d` (T-019 review: per-tenant storage sizing, cluster and restore-RTO limits)

Centralized communications orchestration and audit ledger for customer messaging across SMS, email, and WhatsApp.

**Constraints:** Rust · 100k–5M messages/day per tenant · 7-year retention · customer service + compliance users.

**Two deployments, one codebase:** on-premise for a single institution, and a regional cloud service for up to ~20 large institutions. On-prem is the N=1 case of the same multi-tenant system (§2.1) — not a fork, not a build flag.

**Design principle:** boring technology — the fewest moving parts that satisfy the requirements, biased toward components the ops team can reason about at 3am.

---

## 1. What this system is (and is not)

messgr has two jobs that pull in opposite directions:

1. **Orchestration** — a high-churn job queue. Small working set, heavy UPDATE traffic, latency-sensitive.
2. **Audit ledger** — an append-only compliance archive. Billions of rows, immutable, retained 7 years, queried rarely but thoroughly.

The core structural decision in this design is to **keep these separate**. Conflating them (one table serving both roles) produces a queue that degrades as the archive grows: status-transition UPDATEs bloat partitions that should be frozen, and autovacuum ends up reasoning about 84 monthly partitions to maintain a working set of perhaps 50k pending rows.

**Not in scope:** campaign management, audience segmentation, and journey orchestration. messgr accepts send requests and records what happened. Upstream systems decide who gets what and why.

---

## 2. Architecture

```
  producers (fraud, statements, onboarding, marketing, collections, cards)
  each mTLS-identified against the producer registry (§4.9)
        │                                     ┌──────────────────────┐
        ├── POST /comms ────────────────────▶│  ingest-api          │
        │   (optionally scheduled_for)        │  (axum)              │
        │                                     │  producer identity   │
        └── POST /comms/bulk ───────────────▶│  admission rate limit│
            (campaign batches, COPY-backed)   │  idempotency check   │
                                              │  consent pre-filter  │
                                              └──────┬───────────────┘
                                                     │
                                      ┌──────────────┴──────────────┐
                                      ▼                             ▼
                            comms_request (ledger)              outbox (queue)
                            append-only, partitioned            small, ephemeral
                            monthly, 7yr retention              next_attempt_at drives
                                                                scheduled delivery
                                                                      │
                                                     ┌────────────────┘
                                                     ▼
                                           ┌──────────────────────┐
                                           │  dispatcher          │   one process per TENANT (§9),
                                           │  (one per tenant)    │   one claim loop per channel;
                                           │                      │   hot standby via advisory lock
                                           │                      │
                                           │  gate chain:         │──▶ provider (SMS/email/WhatsApp)
                                           │   expiry             │
                                           │   kill switch        │◀── DLR webhook ──┐
                                           │   producer quota     │                  │
                                           │   verification       │                  │
                                           │   consent            │                  │
                                           │   suppression        │                  │
                                           │   quiet hours        │                  │
                                           │   rate limit         │                  │
                                           └──────────┬───────────┘                  │
                                                      │                              │
                                      ┌───────────────┼──────────────┐        ┌──────┴───────┐
                                      ▼               ▼              ▼        │ webhook-api  │
                                comms_event    producer_usage   (provider)    │ (DMZ)        │
                                (append-only)  (flushed ~5s)                  └──────────────┘

  OTP / auth fast path  ──▶ sms-sender library ──▶ provider     (synchronous, bypasses queue,
                                    └── async, best-effort ──▶ comms_request      all gates, and
                                                               (log only)          every kill switch)

                                           ┌──────────────────────┐
                                           │  query-api + UI      │──▶ Postgres streaming replica
                                           │  + admin panel       │
                                           │  (axum + Askama/htmx)│──▶ kill_switch, producer_quota
                                           └──────────────────────┘    (writes to primary)
```

The diagram above is one tenant's data path. Per region there is one Postgres primary plus a streaming replica, one database per tenant, and one dispatcher pair per tenant (§9). No Kafka, no Redis, no Kubernetes requirement.

Note the two writes the admin panel makes to the primary — kill switches and quota changes are the only control-plane mutations in an otherwise read-only query service, and they propagate to dispatchers by `NOTIFY` rather than by config reload (§5.2).

### 2.1 Tenancy model

The same product ships two ways: on-premise for a single institution, and as a regional cloud service for up to ~20 large institutions. **There is one codebase and one schema. On-prem is simply the N=1 case** — not a separate build, not a compile-time feature flag. Anything else guarantees the two deployments drift.

**One database per tenant, inside a shared regional Postgres cluster.**

```
cluster-eu/
  control            tenant registry, provisioning, platform console
  tenant_acme        comms_request, outbox, customer, ...   (84 partitions, own retention)
  tenant_bank2       comms_request, outbox, customer, ...
cluster-uk/
  control
  tenant_thistle     ...
```

At this tenant count, a shared schema with `tenant_id` filtering would buy savings that round to zero — one migration instead of twenty, one pool instead of twenty — while carrying costs that land hard on large-institution customers: no way to recover one tenant without touching the others, no per-tenant retention (shared partitions cannot be detached), a multi-day purge to offboard, and a security questionnaire answered with "your data shares a table with another bank, separated by a `WHERE` clause". Per-tenant databases make retention and offboarding fall out of ordinary Postgres operations, and make single-tenant recovery a logical extraction rather than surgery on shared partitions — though **not** a one-command operation; see §13 for what restoring one tenant actually involves.

**Per-tenant storage sizing (T-019).** Still-open #15 named the prerequisite this closes: nothing about cluster capacity, tenant packing, or restore RTO can be quoted without first sizing one tenant's ledger. The figures below are estimates built on stated assumptions, not measurements against a real template inventory — replace them once real templates and traffic exist.

*Row size.* `comms_request`'s fixed columns run to roughly **250 bytes** per row (estimate): four `uuid` columns at 16 B each (64 B: `tenant_id`, `id`, `customer_id`, `producer_id`), `created_at` 8 B, `channel`/`class` text averaging ~7 B/~10 B, `template_id` assumed ~20 chars (~21 B), `template_version` 4 B, `campaign_id` present only on the ~30% of rows that carry a campaign (~6 B weighted), `destination_hmac` a fixed 32-byte HMAC (~33 B with its varlena header), `destination_ciphertext` assuming a ~20-character address plus AES-GCM's 28-byte nonce+tag (~49 B), `scheduled_for`/`expires_at` mostly NULL (~0 B weighted — a NULL column costs only a null-bitmap bit, not its full width), `final_status`/`finalized_at` set on the ~90% of rows that reach a terminal state (~17 B weighted) — summing to ~219 B of column data, plus Postgres's own ~30 B per-tuple overhead (heap tuple header, null bitmap, alignment padding). `payload_ciphertext` (§7) then dominates: plaintext plus AES-256-GCM's 12-byte nonce and 16-byte tag, plus a varlena header. Assuming (confirmed during refinement) an average rendered body of 160 bytes for SMS (§1's own constraint line), ~3,000 bytes for email, and ~400 bytes for WhatsApp, and NULL for `class = 'auth'` (§7.4):

| Channel/class | Payload + crypto overhead | Row total |
|---|---|---|
| SMS (marketing/transactional) | ~190 B | ~440 B |
| Email | ~3,032 B | ~3,280 B |
| WhatsApp | ~430 B | ~680 B |
| Auth (no payload, §7.4) | 0 B | ~250 B |

`comms_event` (§4.4) fixed columns run to roughly **85 bytes** per row (estimate): `comms_request_id`/`customer_id` at 16 B each, `occurred_at` 8 B, `event_type` text averaging ~9 B, `provider_ref` empty (~1 B) on the two dispatch-internal events in three and a ~16-byte real reference on the provider-sourced one (~6 B weighted), `provider_status` NULL except on the provider-sourced event (~3 B weighted) — ~58 B of column data plus ~27 B of Postgres per-tuple overhead. Assuming three events per message on average (`queued`, `sent`, plus one terminal provider-sourced event carrying an average 500-byte raw delivery-receipt JSON, encrypted under the same AES-GCM overhead — §4.4's correction that gave every event a real dedup key applies regardless of size), that terminal event runs ~615 B and the other two ~85 B each: **~790 B of `comms_event` storage per message**, blended.

Blending `comms_request` row size across a representative per-tenant mix (confirmed during refinement) of 60% SMS / 25% email / 10% WhatsApp / 5% auth gives ~1,165 B/message, plus the ~790 B/message of `comms_event` above: **~1,955 B/message raw**. `comms_request` carries five physical indexes (the four `CREATE INDEX` statements plus the `PRIMARY KEY`); `comms_event` carries two (the `customer_id` index plus the `UNIQUE` constraint's own index). Applying a **1.6× rule-of-thumb multiplier** for that index overhead plus page/TOAST overhead — a planning approximation, not derived arithmetically from the index count — gives **~3.1 KB of physical storage per message**.

*Annual and 7-year growth, across §1's stated 100k–5M messages/day range* (steady-state; ignores organic growth within the 7-year window, which would push every figure below higher):

| Volume | Annual growth | 7-year total |
|---|---|---|
| 100k msgs/day | ~114 GB | **~0.8 TB** |
| 1M msgs/day | ~1.14 TB | **~8.0 TB** |
| 5M msgs/day | ~5.7 TB | **~40 TB** |

*Tenants per cluster.* Against a ~10 TB usable-per-cluster planning ceiling (confirmed during refinement), packing is sharply tiered rather than a single number — the 50× volume range means the top and bottom of it do not share a cluster the same way:

- **Small tenants (~100k/day, ~0.8 TB/7yr):** roughly a dozen fit comfortably in one cluster.
- **Mid tenants (~1M/day, ~8 TB/7yr):** effectively one per cluster — a second already forces >16 TB.
- **Large tenants (top of range, ~5M/day, ~40 TB/7yr):** a single tenant's full-retention footprint alone is **4× the planning ceiling**, and crosses it within under two years of accumulation. These tenants need a cluster sized to themselves, not a shared one, and "how many tenants share a cluster" has no single answer above the small-tenant tier.

This replaces the earlier unquantified "not thousands of small tenants" assumption with an actual, if rough, number: comfortable sharing exists only at the small end of the stated volume range; the large end is effectively a dedicated-cluster case regardless of how packing is planned.

**`tenant_id` is on every table as a plain column. There is no Row Level Security.**

An earlier version of this design added RLS policies on every table, with `SET LOCAL app.tenant_id` per transaction, a non-owner runtime role, and `FORCE ROW LEVEL SECURITY`. That was over-engineering, and it is worth explaining why so it does not get reintroduced.

RLS earns its keep in a *shared* schema, where its job is catching a query that forgot `WHERE tenant_id = …`. **With one database per tenant there is no such query to forget** — every row in the database belongs to that tenant. The only remaining job was catching a mis-routed connection: code reaching for tenant A's pool and getting tenant B's handle.

For that one job the cost was: policies and force-RLS on ~20 tables, a separate non-owner runtime role and CI checks guarding it, `SET LOCAL` on every transaction, and PgBouncer transaction mode promoted from a pooling choice to a correctness requirement. That last one collided with session advisory locks and `LISTEN` (§2.3).

The same bug is caught far more cheaply:

```rust
// once, right after the pool is opened — a live connection cannot change
// which database it's bound to mid-life, so re-checking at checkout time
// tests nothing this check didn't already catch at creation.
let db: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&conn).await?;
assert_eq!(db, expected_database, "tenant pool mis-routed");
```

One query, no schema surface, no pooling constraint. A pool is bound to a connection string, so it cannot silently change databases underneath you; the realistic failure is selecting the wrong pool, and the assertion catches exactly that — **provided `expected_database` is derived independently of whatever built this pool's connection string.** If both come from the same `tenant_id` (or the same config value) passed into one function, the assertion compares that value to itself and can never fail, however the pool was actually constructed; it looks tested and isn't. `expected_database` must trace back to the caller's own intent — e.g. the tenant the caller believed it was asking for, resolved separately from whatever the pool-construction code did with it — not be re-derived from the same input inside the same call.

**Correction: this was previously written as firing "at pool creation and at checkout".** A checkout-time recheck cannot observe anything creation-time didn't already: a `sqlx::Pool` hands out connections bound to the same connection string used to create it, and Postgres has no operation that reassigns a live session to a different database. The only meaningful moment for this assertion is once, right after the pool opens.

`tenant_id` stays as a column — cheap, useful in exports and support queries, and it keeps the shared-schema consolidation path open if the business ever pivots to hundreds of small tenants. That path remains a weak secondary justification (such a pivot would be a partitioning redesign regardless), but the column costs nothing to keep.

### 2.2 Regional topology

**Regions are fully independent. There is no global control plane.** Each region runs its own control database, Vault cluster, Postgres cluster, and full set of binaries. Tenants are pinned to exactly one region and receive a regional endpoint (`eu.messgr.example`, `uk.messgr.example`).

Refusing a global tenant directory is deliberate. It would be the one component spanning jurisdictions, it would need its own residency answer, and it would become a cross-region availability dependency for every send — all to save tenants from typing a region-specific hostname. Cross-region tenant migration is an offline operation, not a supported runtime feature.

> **Stated limitation: a multi-jurisdiction institution becomes multiple tenants, and their customer timelines do not merge.**
>
> A bank operating in both the UK and the EU gets one tenant per region: two databases, two ledgers, two UIs. "Show me every communication sent to this customer" — the reason this system exists — cannot span them. That lands on exactly the large multinational institutions in the target market, so it needs an answer ready before the first sales conversation rather than during onboarding.
>
> The answer is usually that their own residency rules already forbid the merged view, which makes the split correct rather than merely tolerable. Where a customer genuinely exists in both jurisdictions they are two customer records under two regulators, and a single pane over both would be the compliance problem, not the feature. But this must be said out loud and written into the product description; it is a real constraint, not an implementation detail.

On-prem is structurally identical to a region with one tenant. Same binaries, same control database holding a single row. The control plane is admitted overhead in that deployment — an extra small database for one row — accepted because a colocated-on-prem variant would be the first divergence between the two deployment paths, and divergence compounds.

### 2.3 Connection topology

**Dispatchers bypass the connection pooler. Everything else goes through it.**

This split is not about tenancy — dropping RLS removed that reason (§2.1) — it is simply that dispatchers depend on two session-scoped Postgres features that transaction pooling does not support:

| Feature | Used by | Under transaction pooling |
|---|---|---|
| `pg_try_advisory_lock()` | dispatcher leader election (§9) | The lock lands on whatever backend served that transaction, which then returns to the pool. **Leadership becomes undefined — two active dispatchers for one tenant, both sending.** |
| `LISTEN` | outbox wakeups (§4.2), kill-switch propagation (§5.2) | Registration silently never fires; the fast path is gone with no error to notice. |

`pg_advisory_xact_lock()` is not a substitute — it releases at transaction end, which cannot express "hold leadership".

| Service | Path |
|---|---|
| `ingest`, `query`, `otp`, `webhook` | via PgBouncer, transaction mode — short transactions, high concurrency, multiplexing is the point |
| `dispatcher` | direct to Postgres — long-lived, ~40 per region, session semantics required |

**Connection budget.** "Twenty databases is a for-loop" holds for migrations and does not hold for connections. With a pool per tenant database in each request-path instance the count is instances × tenants × pool size: four ingest instances at ten connections each is 800 backends before counting query services, at roughly 10MB of backend memory apiece. PgBouncer's own pools are per-database too, so twenty databases means twenty server-side pool sets to size.

Three measures, all in place before tenant ten:

- App-side pools stay small; PgBouncer does the multiplexing.
- Pools are created lazily — an idle tenant costs no connections.
- If that is insufficient, shard request-path instances by tenant the way dispatchers already are, so no single process holds every tenant's pool.

This is the genuine operational cost of database-per-tenant, and the thing most likely to bite around tenant fifteen.

### 2.4 End-to-end send walkthrough

The diagram above shows the topology; this traces one message through it, tying together
sections that are otherwise scattered. Two paths — everything through the queue, and OTP's
bypass of it.

**1. Ingest.** A producer POSTs `/comms` (or `/comms/bulk`) over mTLS. The certificate resolves
to `(tenant_id, producer_id)` before anything else happens (§4.9, §11) — identity is never taken
from the request body. `ingest-api` then, in order: applies the admission rate limit (`429` if
breached, before touching Postgres — §5.1), checks `idempotency` for a replay and returns the
existing `comms_request_id` if found (§4.3), resolves `customer_id` and `address_id` from
whatever the caller supplied — explicit id, external id, address, or nothing resolvable, in
which case a provisional customer is minted (§4.7) — and, for bulk campaign submissions only,
runs a non-authoritative consent pre-filter so obviously-suppressed rows never reach the queue
(§5). An OTP request skips resolution entirely: the destination must be supplied explicitly, or
the request is rejected (§4.8).

**2. Write.** `comms_request` (the ledger row, `final_status` still NULL) and `outbox` (the
queue row) are written in the same transaction. A row in one without the other is not a state
this system has — the outbox entry is deleted on completion (§4.2) and the ledger row is what
survives for seven years, so if the transaction fails, neither exists and the producer's retry
lands on the idempotency key. `next_attempt_at` is `now()` for an immediate send, or a future
instant for `scheduled_for` / `scheduled_local` (§6.2) — either way it is the same row and the
same claim index, just sorted later.

**3. Wake.** The insert fires `NOTIFY` on the outbox-wakeup channel; an idle dispatcher for that
tenant picks it up immediately, with a 1-second poll as the correctness fallback if the
notification is missed (§4.2). A scheduled row simply sits in the claim index, invisible to
`WHERE next_attempt_at <= now()`, until its time comes — no scheduler process exists separately
from this.

**4. Claim.** The tenant's active dispatcher (leader-elected via advisory lock, §2.3, §9) runs
one claim loop per channel, `UPDATE ... SET leased_until ... FOR UPDATE SKIP LOCKED ... ORDER BY
priority, next_attempt_at`, so transactional rows are always claimed ahead of marketing ones
(§4.2, §8). **Correction (T-021 review): there is no time-based lease expiry.** The claim
predicate is `leased_until IS NULL`, never a comparison against `now()`, so a lease does not
"simply expire" on any clock. Leader election (T-039, §9) shipped this the way this section
anticipated it would need to: rather than a fresh process sweeping stale leases at raw startup
(safe only when exactly one instance could ever exist per tenant), the sweep now runs once,
immediately after a process wins the advisory lock — winning it is what guarantees the previous
leader's session, and therefore its leases, are actually gone, which is what makes the sweep safe
now that two processes can be live at once.

**5. Gate chain.** Every claimed row is evaluated against the full chain **at this moment**, not
against the state that existed at ingest: expiry, kill switch, producer quota, verification,
consent, suppression, quiet hours, then the provider-side rate limit (§5). Auth-class
messages never reach this step at all (§3). A block either ends the message in a terminal
`final_status` (`expired`, `suppressed_consent`, `suppressed_list`, `unverified_address`), defers
it to a later `next_attempt_at` (quota, rate limit, quiet hours — with jitter, §6.1),
or holds it under an active kill switch (§5.2). Immediately before the provider call the
dispatcher re-reads `outbox.cancelled_at`, because a `DELETE /comms/{id}` may have landed while
the lease was held (§6.2).

**6. Dispatch.** A message that clears every gate goes to the provider through the channel's
`Sender` adapter, inside that provider's in-process circuit breaker and token bucket (§9). The
outcome — `sent` or a provider-level failure — is appended to `comms_event`, `producer_usage` is
incremented in-process (flushed every few seconds, §5.1), and on a terminal outcome the outbox
row is deleted and `comms_request.final_status` is set in the one permitted ledger mutation
(§4.1). A retryable provider failure instead bumps `attempts` and reschedules `next_attempt_at`
with backoff and jitter; the row stays in the outbox.

**7. Receipt.** The provider's asynchronous delivery webhook lands on `webhook-api` in the DMZ,
routed by opaque per-tenant token rather than tenant slug, verified against the provider's
signature, and appended to `comms_event` — deduplicated on the natural key, tolerant of
out-of-order and pre-commit arrival, orphaned to a side table if the `provider_ref` is not yet
known (§10). This does not touch `comms_request.final_status`; that column reflects the
*dispatch* outcome, not delivery — a `sent` message can still later bounce, and the event stream
carries that, not the ledger's summary column.

**8. Read.** `query-api` answers "everything sent to this customer" with one index scan on
`(customer_id, created_at DESC)` against the replica, no join to the customer projection and no
dependency on the event feed being up (§4.1, §11.2). Message detail joins in `comms_event` for
full history. None of this touches the primary, so a compliance search cannot compete with
ingestion for write capacity.

**The OTP path replaces steps 1–7 entirely.** `sms-sender` (or `otp-api` in cloud) calls the
provider synchronously and returns; the ledger write happens afterward, asynchronously and
best-effort, with no gate chain, no outbox row, and no dependency on Postgres or Vault being
reachable at all (§3, §3.1). It rejoins the read path at step 8 — the same UI and API show it —
but nothing upstream of that row's existence is shared with the queue.

---

