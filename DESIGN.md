# messgr — Design

**Version 1** · 2026-09-02 · `be36441`

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
(§4.2, §8). A crashed dispatcher's leases simply expire and the standby (or a recovered leader)
reclaims the rows — no reaper.

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

## 3. The OTP question

**Auth SMS does not go through the queue.**

The obvious design — one pipeline, priority column, OTP marked P0 — puts customer login on the critical path of a system whose other job is bulk marketing. A campaign bug, a bad migration, or dispatcher lock contention would then prevent customers from logging in. A dedicated worker pool addresses head-of-line blocking but not shared fate, and a polling dispatcher imposes a latency floor equal to the poll interval.

Instead:

- Auth services call `sms-sender`, a thin Rust library (or a dedicated single-purpose HTTP service, if the callers are not Rust) that talks to the SMS provider **synchronously**.
- The audit record is written to `comms_request` **asynchronously and best-effort**. If Postgres is unavailable, the send still succeeds and the record is buffered to local disk and backfilled.
- Quiet hours are not evaluated on this path at all — OTP is exempt by policy.

This preserves the single pane of glass (every OTP still appears in messgr's ledger and UI) while removing messgr from the availability path of authentication.

**Cost of this choice:** OTP rate limiting and provider failover are duplicated in `sms-sender` rather than centralized. That is the correct trade — a small amount of duplicated logic in exchange for decoupling tier-0 auth from a tier-1 batch system.

### 3.1 The cloud variant

On-prem, `sms-sender` is a library linked into the bank's own auth service — same process, no network hop, and the availability argument above holds exactly.

In cloud that is not available: the tenant's auth service is on their infrastructure, and a library cannot hold their provider credentials or reach a Vault mount across the boundary. The cloud deployment therefore exposes **`otp-api`, a dedicated minimal endpoint per region**:

- Its own binary and its own process pool, sharing nothing with `ingest-api` or the dispatchers.
- Synchronous: authenticate the tenant by mTLS, look up the provider credential, call the provider, return. No queue, no gate chain, no Postgres write on the request path.
- The audit record is written asynchronously and best-effort, exactly as on-prem — a Postgres outage delays the log, never the OTP.
- Independently deployable and independently scalable, so a messgr release never touches the auth path without an explicit decision to do so.

**Correction: "look up the provider credential" is doing the same job Vault-independence needs for the message path, and it was never given the same mechanism.** §2.4 and the end of §3.1 claim OTP has no dependency on Vault being reachable — but `otp-api`'s provider credential comes from Vault (§13: "all secrets come from Vault"), and unlike a message's DEK, no cache, TTL, or pre-provisioning is specified for it here. As written, "look up the provider credential" reads as a per-request or startup-time Vault call with nothing said about what happens when Vault is sealed. Fixed by stating the same discipline §7.6 already applies to DEKs: `otp-api` fetches its provider credential from Vault at startup and holds it in memory for the life of the process, refreshed on a background timer rather than per request, so a sealed or unreachable Vault degrades nothing on the OTP request path — it only delays picking up a credential *rotation* until Vault recovers.

This is weaker than the on-prem story — a network hop and a shared regional service now sit between the tenant and their SMS provider, where on-prem there was neither. It is worth being explicit with cloud tenants about that difference rather than presenting the two deployments as equivalent. Tenants for whom OTP latency and availability are paramount should be told they can keep auth on-premise while using the cloud service for everything else; the ledger accepts backfilled auth records from either source.

---

## 4. Data model

Everything below lives in a **tenant database** (§2.1) — that is, the tenant boundary is the database itself, not a column. **Correction: an earlier draft of this paragraph claimed `tenant_id uuid NOT NULL` was a repeated first column on every table below.** It is not, and every table shown from here on is correct as written, with a single deliberate exception: `comms_request` alone carries `tenant_id`, kept there as a tripwire and a consolidation path if the business ever pivots to a shared schema (§2.1) — not as an isolation mechanism, since a query inside one tenant's database has no other tenant's rows to filter out. No other table needs it, and adding it elsewhere would misstate what actually enforces isolation: the database boundary plus the `current_database()` assertion (§2.1), not a column. `quiet_hours_policy`'s `tenant_id` column, present in an earlier draft of that table, is removed for the same reason — it was the one other table that had drifted from this convention.

The control database (§4.11) is separate and holds no customer data.

### 4.1 Ledger — `comms_request`

Append-only. Partitioned by RANGE on `created_at`, monthly. 84 live partitions per tenant at steady state — and because each tenant has its own database, retention length is a per-tenant setting rather than a platform-wide one.

```sql
CREATE TABLE comms_request (
    tenant_id              uuid        NOT NULL,   -- tripwire + consolidation path (§2.1)
    id                     uuid        NOT NULL,
    created_at             timestamptz NOT NULL,
    customer_id            uuid        NOT NULL,   -- always set; provisional shell if unresolvable (§4.6)
    channel                text        NOT NULL,   -- sms | email | whatsapp
    class                  text        NOT NULL,   -- auth | transactional | marketing
    template_id            text        NOT NULL,
    template_version       int         NOT NULL,   -- pinned; templates are immutable per version
    campaign_id            text,                   -- null for transactional
    destination_hmac       bytea       NOT NULL,   -- keyed HMAC, pepper in Vault; indexed lookup
    destination_ciphertext bytea       NOT NULL,   -- under customer DEK; the address as actually used
    payload_ciphertext     bytea,                  -- NULL for auth class; see §7
    producer_id            uuid        NOT NULL,   -- registered caller (§4.9)
    scheduled_for          timestamptz,            -- NULL = send immediately (§6.2)
    expires_at             timestamptz,            -- drop rather than send late (§6.2)
    final_status           text,                   -- NULL while in flight; see below
    finalized_at           timestamptz,
    PRIMARY KEY (created_at, id)
) PARTITION BY RANGE (created_at);

CREATE INDEX ON comms_request (customer_id, created_at DESC);
CREATE INDEX ON comms_request (final_status, created_at DESC);
CREATE INDEX ON comms_request (campaign_id, created_at) WHERE campaign_id IS NOT NULL;
CREATE INDEX ON comms_request (destination_hmac, created_at DESC);
```

**Correction: a `dek_id uuid` column was removed from this table.** It shipped in T-009, always written NULL, referencing nothing, and read by no code. The DEK for a row is never row-scoped in the first place — encryption is per **customer** (§7.6), so `customer_id` already resolves the key via `customer_dek`. A column that names no reader and no writer is dead weight on a partitioned 7-year table; if a future need for per-message key versioning arises, add the column with a stated reader at that time.

**Correction: `destination_hmac` had no index, despite §11.2 promising one.** The address-scoped query pattern ("who did we contact on this number") is documented as an index scan on `destination_hmac`, but no such index existed on `comms_request` — only `customer_id`, `final_status`, and `campaign_id` were indexed. Added above as `(destination_hmac, created_at DESC)`, matching the shape of the other lookup indexes on this table.

**`final_status` is the one permitted mutation of a ledger row**, and it needs justifying because "append-only" is otherwise the whole point.

The gate chain (§5) produces terminal outcomes — `sent`, `expired`, `cancelled`, `suppressed_consent`, `suppressed_list`, `unverified_address` — and `GET /comms?status=` filters on them. Deriving status from `comms_event` would mean an aggregate over a partitioned 7-year table on every query; storing it nowhere would make the suppression outcomes that the compliance story depends on unqueryable.

So: exactly one `UPDATE` per row, setting `final_status` and `finalized_at` when the outbox row reaches a terminal state. Because that happens within the outbox's lifetime — days at most, and bounded by the scheduling horizon — **the update only ever touches hot partitions.** Partitions older than the horizon are never rewritten and stay effectively frozen, which is the property that mattered. The terminal outcome is also written to `comms_event`, so the event stream remains complete; `final_status` is a denormalization for query, not the source of truth.

**The ledger is self-contained by design.** `customer_id` and the destination are both denormalized onto every row at write time and never rewritten. The customer timeline (§11.2) is therefore a single index scan with no join to the customer projection — it keeps working when the projection is empty, mid-rebuild, or its event feed is down. An audit ledger must not depend on a cache to be readable.

Both destination columns earn their place: the HMAC is searchable without decryption, the ciphertext is what the UI renders. Storing only the HMAC would leave the UI unable to show which address was used, and resolving that through the projection would fail for any message older than the event feed.

Partitioning here is **not** about dropping old data — nothing is dropped for 7 years. It buys vacuum and index locality (queries are overwhelmingly recent-biased), the ability to move cold partitions to slower storage, and bounded index sizes.

### 4.2 Queue — `outbox`

Unpartitioned, deliberately small. Rows are DELETEd on reaching a terminal state, so the table stays in the low hundreds of megabytes regardless of retention.

```sql
CREATE TABLE outbox (
    comms_request_id  uuid        PRIMARY KEY,
    created_at        timestamptz NOT NULL,     -- FK component into ledger partition
    channel           text        NOT NULL,
    class             text        NOT NULL,
    priority          smallint    NOT NULL,     -- 1 transactional, 2 marketing; stored, not computed
    customer_id       uuid        NOT NULL,
    address_id        uuid        NOT NULL,     -- resolved contact point (§4.6); consent keys on it
    producer_id       uuid        NOT NULL,     -- quota + kill-switch scoping (§5.1, §5.2)
    campaign_id       text,
    next_attempt_at   timestamptz NOT NULL,     -- future-dated for scheduled sends (§6.2)
    expires_at        timestamptz,
    cancelled_at      timestamptz,              -- set by cancellation; re-checked before send
    attempts          smallint    NOT NULL DEFAULT 0,
    leased_until      timestamptz
);

CREATE INDEX outbox_claim ON outbox (channel, priority, next_attempt_at)
    WHERE leased_until IS NULL;
CREATE INDEX ON outbox (producer_id, next_attempt_at);
CREATE INDEX ON outbox (campaign_id, next_attempt_at) WHERE campaign_id IS NOT NULL;
```

**Correction: the admin panel's scheduled-queue view (§11.3) had no supporting index.** "Pending future-dated messages by producer, campaign, and due window" needs `(producer_id, next_attempt_at)` and `(campaign_id, next_attempt_at)` — the claim index alone (keyed on `channel`) cannot serve either. Added above.

`priority` is a stored integer rather than a `class_rank(class)` function call, so the index can actually serve the `ORDER BY`. An expression the planner has to evaluate per row cannot, which would turn every claim into a sort over the whole ready set.

Claim query — **one loop per channel**, run concurrently inside the single per-tenant dispatcher (§9). Channels are separated because each has its own provider, rate limit, and circuit breaker; a wedged SMS provider must not stall email:

```sql
UPDATE outbox SET leased_until = now() + interval '2 minutes'
WHERE comms_request_id IN (
    SELECT comms_request_id FROM outbox
    WHERE channel = $1 AND next_attempt_at <= now() AND leased_until IS NULL
    ORDER BY priority, next_attempt_at
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

`ORDER BY priority` is what makes transactional preempt marketing. There is no separate share-of-budget mechanism — see §8.

Leases (rather than a `status = 'processing'` column) mean a crashed dispatcher's work becomes claimable automatically once the lease expires. No reaper job.

**Correction: the claim predicate and the retry path were never reconciled, and the gap is load-bearing.** `outbox_claim` is a partial index `WHERE leased_until IS NULL` — a leased row is invisible to it until the lease's own timeout passes, regardless of `next_attempt_at`. §2.4 step 6 describes a retryable provider failure as bumping `attempts` and rescheduling `next_attempt_at` "with backoff and jitter; the row stays in the outbox" — but says nothing about `leased_until`. If the retry path leaves the existing lease in place, the row is unclaimable until that lease's fixed 2-minute timeout expires regardless of the computed backoff, silently overriding whatever backoff was intended; if instead nothing ever clears `leased_until` at all, a row that fails once **before** reaching a terminal write is stranded forever, since only a terminal write (step 6, `sent` or a non-retryable failure) or the lease's own expiry ever frees it. The design must state this explicitly: a retryable failure clears `leased_until` to `NULL` in the same statement that reschedules `next_attempt_at`, so the outbox's one release mechanism is "write a terminal state" **or** "explicitly clear the lease on a rescheduled retry" — never a bare timeout race between the two.

`LISTEN`/`NOTIFY` on insert wakes an idle dispatcher immediately; a 1-second poll is the fallback, so a missed notification costs latency but never correctness. This is the queue-wakeup channel only — kill-switch propagation uses a separate channel with a slower fallback (§5.2).

### 4.3 Idempotency

Global uniqueness cannot be enforced on the partitioned ledger — PostgreSQL requires unique indexes on a partitioned table to include every partition key column, so `UNIQUE (created_at, idempotency_key)` would be the only legal form, and it permits the same key in two different months. Idempotency therefore lives in its own unpartitioned table:

```sql
CREATE TABLE idempotency (
    producer_id       uuid        NOT NULL,
    key               text        NOT NULL,
    comms_request_id  uuid        NOT NULL,
    expires_at        timestamptz NOT NULL,
    PRIMARY KEY (producer_id, key)
);
```

Retained 30 days, swept nightly (the sweep job itself is not yet built — track it against one ticket, not two; see build order). A retried POST returns the original `comms_request_id` with `200`, not a duplicate send.

**Correction: the key was a bare `PRIMARY KEY (key)`, scoped to nobody.** Idempotency keys are caller-supplied (§2.4 step 1). A bare `text PRIMARY KEY` means two different producers who happen to choose the same key — a sequential counter, a UUID library seeded the same way, a copy-pasted test value — collide on each other's rows: the second producer's request silently returns the *first* producer's `comms_request_id`. Idempotency is a per-producer contract, not a platform-wide namespace; the key is now `(producer_id, key)`, matching how quotas and kill switches already scope to the authenticated caller (§4.9).

### 4.4 Events, consent, templates

```sql
CREATE TABLE comms_event (              -- partitioned monthly, append-only
    comms_request_id  uuid        NOT NULL,
    customer_id       uuid        NOT NULL,  -- denormalized so erasure can find these rows
    occurred_at       timestamptz NOT NULL,
    event_type        text        NOT NULL,
      -- queued | sent | delivered | failed | bounced | read | complaint
      -- | expired | cancelled | suppressed_consent | suppressed_list | unverified_address
    provider_ref      text        NOT NULL DEFAULT '',  -- '' for dispatch-internal events; see below
    provider_status   text,                  -- normalized code, safe to keep in clear
    provider_payload_ciphertext bytea,       -- raw provider JSON, under customer DEK — see below
    UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)
) PARTITION BY RANGE (occurred_at);
CREATE INDEX ON comms_event (customer_id, occurred_at);

CREATE TABLE orphan_event (              -- §10: a receipt for a provider_ref not yet known
    id                          uuid        PRIMARY KEY,
    received_at                 timestamptz NOT NULL,
    provider                    text        NOT NULL,
    provider_ref                text        NOT NULL,
    occurred_at                 timestamptz NOT NULL,
    event_type                  text        NOT NULL,
    provider_status             text,
    provider_payload_raw        jsonb,      -- see below: plaintext, deliberately, and only here
    reconcile_attempts          smallint    NOT NULL DEFAULT 0
);
CREATE INDEX ON orphan_event (provider_ref);

CREATE TABLE consent (
    address_id   uuid NOT NULL REFERENCES customer_address(id),   -- per contact point, not per person
    class        text NOT NULL,
    opted_in     bool NOT NULL,
    source       text NOT NULL,          -- where the opt-in/out was captured, for evidence
    updated_at   timestamptz NOT NULL,
    PRIMARY KEY (address_id, class)
);

CREATE TABLE suppression (              -- hard bounces, complaints, regulatory blocks
    destination_hmac bytea PRIMARY KEY,
    reason           text  NOT NULL,
    added_at         timestamptz NOT NULL
);

CREATE TABLE template (
    template_id text NOT NULL,
    version     int  NOT NULL,
    channel     text NOT NULL,
    locale      text NOT NULL,
    body        text NOT NULL,
    approved_by text NOT NULL,
    approved_at timestamptz NOT NULL,
    PRIMARY KEY (template_id, version, locale)
);
```

> **Raw provider payloads are encrypted, and this was very nearly missed.** A delivery receipt from an SMS gateway or an email provider echoes the recipient address back in its JSON body — a phone number or email in clear, sitting in an append-only table for seven years. Left as plain `jsonb` it would have been PII entirely outside the erasure scheme: §7.2's physical redaction touched `comms_request` and `customer_address` but not `comms_event`, and crypto-shredding would not have covered it either. **Both erasure paths would have leaked the destination.**
>
> Fixed by encrypting the raw payload under the same customer DEK and denormalizing `customer_id` onto the event so erasure can locate the rows. `provider_status` stays in clear because operational queries ("how many `SMS_UNREACHABLE` from this gateway today") must not require decryption.
>
> The general lesson for review: every new table needs asking *what PII lands here, and which erasure path reaches it*. Third-party payloads are the easiest place to miss, because nobody chose their contents.

**Correction: `comms_event`'s dedup constraint was inert for exactly the events that need it most.** `UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)` relies on Postgres unique-index semantics — but Postgres treats every NULL as distinct from every other NULL, so two rows with the same `(occurred_at, comms_request_id, event_type)` and both `provider_ref IS NULL` do **not** collide. `provider_ref` is NULL for every event this system generates itself rather than receives from a provider — gate-chain terminal outcomes (`expired`, `suppressed_consent`, and the rest) and dispatch-internal events (`queued`, `sent`, `failed`) all have no provider reference. §10's "inserts use `ON CONFLICT DO NOTHING`; duplicates are free" is true only for provider-sourced events keyed by a real `provider_ref` — a retried write of a gate outcome (a crash between the `comms_event` insert and the `comms_request.final_status` update, then a safe retry of the whole transaction) inserts a second, indistinguishable row instead of no-opping. Fixed by giving every event a value to dedupe on regardless of source: `provider_ref` becomes `NOT NULL DEFAULT ''`, with providers supplying the real reference and dispatch-internal events writing `''` — an empty string is not NULL, so the unique index now catches both cases uniformly.

**Correction: `orphan_event` (§10) was named but never given a schema.** Added above. Its payload is deliberately **not** encrypted under a customer DEK, unlike `comms_event` — the whole reason a receipt lands here is that `comms_request_id` (and therefore `customer_id`) isn't known yet, so there is no DEK to encrypt under. This is a genuine, narrow exception to "PII is always written encrypted" (hard invariant 7's spirit, if not its letter, since no customer is yet identified to scope a key to), and it must stay narrow: reconciliation is a "short delay" per §10, and `reconcile_attempts` exists so a row that fails to reconcile past a small bound (config, not hardcoded) pages someone rather than accumulating as a permanent plaintext-PII table. A row that reconciles is deleted from `orphan_event` once re-inserted into `comms_event` proper, encrypted, under the now-known customer's DEK.

Templates are **immutable once approved**; changes create a new version. Every `comms_request` pins `template_version`, so "what exactly did we send this customer in 2021" remains answerable years later. Approval metadata is captured because a bank will need to show who signed off on customer-facing content.

**Render syntax (T-010): literal `{{key}}` placeholders**, substituted by plain string scanning against a caller-supplied variable map — no templating engine. Whitespace inside the braces is trimmed, so `{{ key }}` matches too. A key the body references but the caller doesn't supply is a hard render error, never sent as literal `{{key}}` text or blanked out — a bank must not send customer-facing content with an unsubstituted placeholder. A variable the caller supplies but the body never references is silently ignored.

### 4.5 Keys, erasure, and campaign rollup

```sql
CREATE TABLE customer_dek (
    customer_id    uuid PRIMARY KEY,
    wrapped_dek    text NOT NULL,       -- Vault Transit ciphertext, "vault:v1:..." (§7.6)
    created_at     timestamptz NOT NULL,
    shredded_at    timestamptz          -- set when key destroyed; row retained as tombstone
);

CREATE TABLE erasure_request (
    id             uuid PRIMARY KEY,
    customer_id    uuid NOT NULL,
    mode           text NOT NULL,       -- crypto_shred | physical_redact
    requested_by   text NOT NULL,
    requested_at   timestamptz NOT NULL,
    legal_basis    text NOT NULL,
    completed_at   timestamptz,
    rows_affected  bigint,
    backups_clear_at timestamptz        -- see §7.3
);

```

The erasure log is itself a compliance artifact: when a record's payload turns out to be unreadable years later, you must be able to show *why*, *who authorized it*, and *under what legal basis*. An erasure that leaves no trace is indistinguishable from data loss.

> **A `campaign_stats` rollup table was specified here and has been removed.** It contradicted its own justification: the argument against an OLAP tier was that biweekly queries do not warrant a pipeline, and the response was to build a nightly incremental job for those same biweekly queries. It also introduced a correctness surface nobody had addressed — delivery receipts arriving *after* the nightly run would leave the rollup permanently disagreeing with the ledger.
>
> Campaign queries are served by the `(campaign_id, created_at)` index on the replica (§11.2). If a 26-times-a-year query is genuinely too slow, that is the moment to build a rollup — with the late-receipt problem solved deliberately rather than discovered.

### 4.6 Customer projection

**messgr is never the system of record for customer data.** Master data lives upstream; this is a read-only projection, fed by an event feed, held only to resolve *who to send to right now*. It is deliberately narrow in domain — no names, demographics, account data, balances, or segments. Narrow domain, not few columns: doing this correctly still takes four tables.

```sql
CREATE TABLE customer (
    id                uuid PRIMARY KEY,
    locale            text NOT NULL,          -- selects template locale (§4.4)
    timezone          text NOT NULL,          -- quiet hours + local-time scheduling (§6)
    provisional       bool NOT NULL DEFAULT false,
    source_system     text,
    source_updated_at timestamptz,            -- staleness bound; NULL for provisional
    created_at        timestamptz NOT NULL
);

CREATE TABLE customer_external_id (
    customer_id uuid NOT NULL REFERENCES customer(id),
    system      text NOT NULL,                -- core_banking | crm | digital | cards
    external_id text NOT NULL,
    PRIMARY KEY (system, external_id)
);
CREATE INDEX ON customer_external_id (customer_id);

CREATE TABLE customer_address (
    id                uuid PRIMARY KEY,
    customer_id       uuid NOT NULL REFERENCES customer(id),
    kind              text NOT NULL,          -- email | msisdn | whatsapp | push | postal
    value_ciphertext  bytea NOT NULL,         -- under customer DEK (§7)
    value_hmac        bytea NOT NULL,         -- keyed, pepper in Vault; same as destination_hmac
    rank              smallint NOT NULL,      -- 1 = primary, 2 = secondary, ...
    label             text,
    verified_at       timestamptz,
    active_from       timestamptz NOT NULL,
    active_to         timestamptz,            -- NULL = current
    source_updated_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX ON customer_address (customer_id, kind, rank) WHERE active_to IS NULL;
CREATE UNIQUE INDEX ON customer_address (kind, value_hmac) WHERE active_to IS NULL;
CREATE INDEX ON customer_address (value_hmac, active_from);
CREATE INDEX ON customer_address (customer_id) WHERE active_to IS NULL;

CREATE TABLE customer_alias (               -- master-system merges; ledger stays immutable
    old_customer_id uuid PRIMARY KEY,
    customer_id     uuid NOT NULL REFERENCES customer(id),
    merged_at       timestamptz NOT NULL
);
```

**Why not `primary_email` / `secondary_emails` / `primary_phone` / `secondary_phones`.** That shape hardcodes two channel types into the schema. WhatsApp already fits badly — its identity is a phone number but with separate opt-in state and its own session-window rules — and push tokens, in-app inbox, and postal mail break it outright. Every new channel would become a migration plus a rewrite of every query touching contacts. `kind` + `rank` gives the same primary/secondary semantics as an ordered list, extensible without schema change.

**Contact values are encrypted, not plaintext.** §7 encrypts message payloads and HMACs destinations; leaving the highest-value PII in the system sitting in a plaintext column would contradict that and leave contact details fully readable after an erasure request. Encrypted under the same customer DEK, so crypto-shredding covers them for free. The lookup hash is a *keyed* HMAC with a Vault-held pepper — the phone-number space is ~10¹⁰, so an unkeyed SHA-256 is brute-forceable in seconds.

**Address rows are append-only.** An update closes the current interval (`active_to`) and inserts a new row. This exists for two narrow jobs: backfilling attribution on historical sends, and answering "who held this number in 2023" during an investigation. It is *not* load-bearing for the customer timeline — the ledger already carries `customer_id` and the destination (§4.1). Telcos recycle disconnected numbers after ~90 days, so over a 7-year window the same `value_hmac` legitimately belongs to more than one person; intervals are what keep those apart.

**Correction, found during T-015's refinement.** The `(kind, value_hmac)` unique index above was missing from the original draft of this schema. Without it, address-only resolution (§4.7: caller supplies a destination with no `customer_id`/external id) has a genuine race — two concurrent sends to a never-seen number can each fail to find an existing address row and each mint a *separate* provisional customer for the same number, with nothing to stop it. `active_to IS NULL` scopes the constraint to the currently-active row per `(kind, value_hmac)`, which is exactly the set address-only resolution searches; closing an interval and inserting a successor (the append-only update path above) still works, since only one row per `(kind, value_hmac)` is ever active at a time. Kept as a plain unique index, not a broader rework, because the failure mode is narrow and this closes it completely.

### 4.7 Resolution at ingest

Every message gets a `customer_id`, always. A nullable one would break per-customer DEKs — there would be no key to encrypt the payload under.

| Caller supplies | Behaviour |
|---|---|
| `customer_id` | used directly, after alias expansion |
| external id + system | resolved via `customer_external_id` |
| address only | resolved via `value_hmac` against currently-active addresses |
| nothing resolvable | **mint a provisional customer** + address row, stamp its id |

Provisional shells are cheap (a handful of thin rows), get a DEK like any customer, and are reconciled by the event feed later through `customer_alias` — reusing the merge machinery rather than inventing a second path. Prospect marketing and enrolment-time OTP both land here naturally.

**Never reject a send because resolution failed.** A missing timeline entry is bad; a blocked OTP is worse.

**Merges.** When the master system merges two customers, ledger rows for the retired id are *never rewritten* — rewriting an audit ledger is precisely what you do not do. `customer_alias` redirects, and timeline queries expand the id set through the alias chain. Splits generally require manual adjudication; flag them for a human rather than guessing.

### 4.8 What the projection resolves, and what it never does

The projection resolves **identity** — which `customer_id` a request belongs to (§4.7) — and nothing else. It never resolves the **destination**: `POST /comms` requires `destination` on every request, for every class, and that caller-supplied value is what the message is sent to, always. "Address-only" resolution (§4.7) uses the supplied destination to look up or mint the owning customer; it does not go the other way and hand back a different, stored address for the caller to send to instead.

**Correction: a staleness gate was specified here, in the gate chain (§5), and in `tenant_config`, and it could never fire.** The reasoning at the time was that transactional and marketing messages "resolve via the projection" for their destination, so a stale `customer_address.source_updated_at` needed a defer-and-alert gate to stop a send from reaching a number the customer no longer holds. That premise was wrong for every class, not just auth: the API has never accepted a request without an explicit `destination`, so there was never a code path where a projection-resolved address reached the provider without the caller having supplied it fresh on the same request. A gate guarding a path the API cannot take is not a defense-in-depth measure — it is untestable, and untested code guarding nothing is worse than no code, because it looks like a control. Removed: the gate chain row (§5), `tenant_config.staleness_max_age` (§4.10), and Still-open's staleness-threshold question. `source_updated_at` stays on `customer_address` and `customer` — it remains useful for support and investigation ("was this the address on file at the time") even with no gate reading it.

**OTP's real distinguishing property is not the explicit destination — every class has that.** It is that auth-class messages skip resolution and the gate chain entirely (§3, §5): the destination the caller supplies is used as-is, with no projection lookup, no `customer_id` linkage beyond what the auth service already knows, and no gate evaluated against it.

**Feed mechanism: event feed** from the master system (customer created/updated, address added/changed/verified/removed). Batch reconciliation runs nightly as a safety net against missed events, comparing checksums rather than replaying everything.

### 4.9 Producer registry

Send requests arrive from many upstream subsystems — fraud, statements, onboarding, marketing, collections, card services. Each is a **registered producer** with an identity derived from its mTLS client certificate, not a free-text `source_system` string a caller can set to anything it likes. Quotas, kill switches, and spend attribution all key on this identity, so it has to be authenticated rather than asserted.

```sql
CREATE TABLE producer (
    id          uuid PRIMARY KEY,
    name        text UNIQUE NOT NULL,
    cert_subject text UNIQUE NOT NULL,   -- mTLS CN/SAN this producer authenticates with
    owner_team  text NOT NULL,
    contact     text NOT NULL,           -- who to page when its quota alerts fire
    enabled     bool NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL
);

CREATE TABLE producer_quota (
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_minute  int,                     -- burst ceiling; NULL = unlimited
    per_day     int,                     -- daily total; NULL = unlimited
    enforcement text NOT NULL,           -- hard | soft  (§5.1)
    PRIMARY KEY (producer_id, channel, class)
);

CREATE TABLE producer_quota_override (   -- time-boxed uplift, e.g. campaign day
    id          uuid PRIMARY KEY,
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_day     int  NOT NULL,
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz NOT NULL,    -- mandatory; overrides always expire
    approved_by text NOT NULL,
    reason      text NOT NULL
);

CREATE TABLE producer_usage (            -- flushed from dispatcher every few seconds
    producer_id  uuid NOT NULL,
    channel      text NOT NULL,
    class        text NOT NULL,
    granularity  text NOT NULL,          -- minute | day
    window_start timestamptz NOT NULL,
    sent         bigint NOT NULL DEFAULT 0,
    blocked      bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (producer_id, channel, class, granularity, window_start)
);
-- minute rows are swept after 7 days; day rows are kept for a year.
-- Without this, minute granularity grows unbounded: ~130k rows/day/tenant at
-- 10 producers x 3 channels x 3 classes x 1440 minutes, purely to draw a sparkline.
CREATE INDEX ON producer_usage (granularity, window_start);

CREATE TABLE kill_switch (
    id          uuid PRIMARY KEY,
    scope       text NOT NULL,           -- global | channel | producer | producer_channel | campaign
    scope_key   text,                    -- NULL for global
    on_queued   text NOT NULL,           -- hold | discard  (§5.2)
    engaged_by  text NOT NULL,
    engaged_at  timestamptz NOT NULL,
    reason      text NOT NULL,
    released_by text,
    released_at timestamptz              -- NULL = currently active
);
CREATE UNIQUE INDEX ON kill_switch (scope, COALESCE(scope_key, '')) WHERE released_at IS NULL;
```

**Correction: this index did not do its job for `global` scope.** `scope_key` is NULL for `global` — and Postgres unique indexes treat every NULL as distinct from every other NULL, so `(scope, scope_key) = ('global', NULL)` never collides with itself. Two operators could each engage a global kill switch, both succeed, and now releasing one leaves the other silently still active — exactly the two-active-switches hole a unique index here exists to prevent. Wrapping `scope_key` in `COALESCE(scope_key, '')` gives every scope a real, comparable value; `''` is never a legitimate `scope_key` for any non-global scope, so this cannot mask a genuine collision.

Quota overrides carry a mandatory `valid_to`. A permanent "temporary" uplift is the most common way quota systems quietly stop meaning anything.

### 4.10 Tenant configuration

Everything an institution can differ on lives in its own database, so there is no platform-wide policy to negotiate between tenants.

```sql
CREATE TABLE tenant_config (
    singleton           boolean NOT NULL DEFAULT true,  -- no tenant_id (§2.1 — the tenant is
                                                         -- the database); PRIMARY KEY (singleton)
                                                         -- + CHECK(singleton) enforces one row
    display_name        text,                   -- not yet created — no reader yet (T-007)
    retention_years     int  NOT NULL,          -- 7 for this customer; another may want 3
    default_timezone    text NOT NULL,          -- fallback for unknown customer tz (§6.1)
    default_locale      text NOT NULL,
    schedule_horizon_days int NOT NULL DEFAULT 90,
    quota_day_boundary_tz text NOT NULL,
    verification_mode   text NOT NULL DEFAULT 'observe',  -- enforce | observe (§5)
    oidc_issuer         text,                   -- the tenant's own IdP (§11.1) — not yet created, added by T-035
    oidc_client_id      text,                   -- not yet created, added by T-035
    oidc_group_claim    text,                   -- not yet created, added by T-035
    PRIMARY KEY (singleton)
);

CREATE TABLE quiet_hours_policy (
    scope       text NOT NULL,          -- region | segment | default
    scope_key   text NOT NULL DEFAULT '',  -- '' for the institution-wide default row
    start_local time NOT NULL,
    end_local   time NOT NULL,
    PRIMARY KEY (scope, scope_key)
);

CREATE TABLE provider_config (
    channel         text NOT NULL,
    priority        smallint NOT NULL,  -- ordered list; failover order (§12.1)
    provider        text NOT NULL,      -- twilio | smtp | meta_wa | ...
    credential_path text NOT NULL,      -- Vault path, never the credential itself
    rate_limit_per_sec int NOT NULL,
    PRIMARY KEY (channel, priority)
);
```

**Correction: `quiet_hours_policy`'s primary key could not represent its own `default` scope.** `scope_key` was nullable, and the `default` scope (institution-wide fallback, per §6.1's resolution order `customer tz -> segment policy -> institution default`) is exactly the row with no natural key — but a `PRIMARY KEY` column is implicitly `NOT NULL`, so a `default`-scope row could never be inserted at all under the schema as originally written. Same fix as `kill_switch` above, adapted to a primary key rather than a partial unique index (which cannot itself wrap an expression): `scope_key` is `NOT NULL DEFAULT ''`, with `''` reserved for the scope that has no key.

T-007 ships only `tenant_config`'s `retention_years`, `default_timezone`, `default_locale`,
`schedule_horizon_days`, `quota_day_boundary_tz`, and `verification_mode` —
`display_name` and the `oidc_*` columns are shown above as the eventual design but are not yet
migrated; nothing reads them yet (T-035 adds the `oidc_*` columns when real OIDC lands).

**Correction: T-007 also shipped `staleness_max_age`, and it is now dead in the shipped
schema, not just cut from the design above.** §4.8 explains why the gate it backed could never
fire — every request supplies its own `destination`, so nothing ever reads a projection-resolved
address closely enough to check its staleness. The column still exists in
`migrations/tenant/0002_tenant_config.sql`; dropping it is a schema migration, not a documentation
change, and is tracked as part of the ledger/queue schema remediation ticket rather than done here.
`quiet_hours_policy` is an unrelated table not yet created (later ticket: quiet-hours
resolution). `provider_config` ships in T-012, without a `tenant_id` column — corrected here to
match the "no `tenant_id` inside a tenant-database table" convention `tenant_config` and
`producer` already established (§2.1: the tenant is the database); the primary key is
`(channel, priority)`. T-012 ships the table, a channel-agnostic `Sender` trait, and a generic
HTTP adapter proven against a mock server — it does not commit to a real vendor, so "provider
selection" (Still Open #4) remains open.

**Tenants bring their own provider accounts.** The platform never resells messaging, which removes an entire category of problems: rate limits and spend are naturally per-tenant, there is no shared provider budget to arbitrate, and a tenant exhausting its Twilio credit is visibly its own problem. Credentials are referenced by Vault path, never stored in Postgres. Producer quotas (§5.1) become a governance tool for the tenant's internal teams rather than a billing mechanism for the platform.

### 4.11 Control database

One per region, outside every tenant database, holding **no customer data and no message content**.

```sql
CREATE TABLE tenant (
    id             uuid PRIMARY KEY,
    slug           text UNIQUE NOT NULL,        -- routing key for the tenant UI hostname
    region         text NOT NULL,               -- must match this control DB's region; asserted on boot
    database_name  text UNIQUE NOT NULL,
    vault_mount    text UNIQUE NOT NULL,        -- per-tenant Transit mount (§7.6)
    webhook_token  text UNIQUE NOT NULL,        -- opaque; provider callback path (§10). Never the slug
    status         text NOT NULL,               -- provisioning | active | suspended
                                                -- | offboarding_archive | offboarding_destroy  (§7.7)
    created_at     timestamptz NOT NULL
);

-- mTLS producer certs resolve to a tenant BEFORE any tenant database is opened.
-- Without this, ingest cannot know which database to look the producer up in.
CREATE TABLE producer_cert (
    cert_subject text PRIMARY KEY,              -- CN/SAN presented by the producer
    tenant_id    uuid NOT NULL REFERENCES tenant(id),
    producer_id  uuid NOT NULL,                 -- resolved within that tenant's producer table
    enabled      bool NOT NULL DEFAULT true     -- denormalized from producer.enabled (T-006):
                                                 -- resolution reads only this column, so it
                                                 -- never has to open the tenant database above
                                                 -- just to tell "unknown" from "disabled" apart
);

CREATE TABLE tenant_schema_version (             -- migrations run N times; drift must be visible
    tenant_id  uuid PRIMARY KEY REFERENCES tenant(id),
    version    int NOT NULL,
    applied_at timestamptz NOT NULL
);

CREATE TABLE platform_kill_switch (              -- operator-level; overrides tenant switches (§5.2)
    id          uuid PRIMARY KEY,
    scope       text NOT NULL,                   -- platform | tenant
    tenant_id   uuid,
    engaged_by  text NOT NULL,
    engaged_at  timestamptz NOT NULL,
    reason      text NOT NULL,
    released_by text,
    released_at timestamptz
);

CREATE TABLE platform_audit (                    -- provisioning, suspension, break-glass
    id         uuid PRIMARY KEY,
    actor      text NOT NULL,
    action     text NOT NULL,
    tenant_id  uuid,
    detail     jsonb NOT NULL,
    at         timestamptz NOT NULL
);
```

`tenant_schema_version` exists because running migrations N times introduces a failure mode that a single database does not have: partial application. Nineteen tenants on version 47 and one stuck on 46 is a state the platform must be able to see and report, not discover when a query fails.

---

## 5. The gate chain

Every message evaluates these gates **at send time**, not at ingestion time. A campaign queued Monday may dispatch Wednesday, by which point consent may have changed.

| Gate | Rule | On block |
|---|---|---|
| Expiry | `expires_at` in the past (§6.2). Checked first — cheapest, and avoids spending any other gate's work on a dead message. | Terminal: `expired` |
| Kill switch | Any active switch matching global / channel / producer / producer+channel / campaign (§5.2). | Hold, or discard per switch |
| Producer quota | Per-minute and per-day counters for (producer, channel, class) (§5.1). | Hard: defer to next window. Soft: send and alert |
| Verification | `customer_address.verified_at` must be set for transactional and marketing. **Per-tenant mode: `enforce` or `observe`** — see below. | Terminal: `unverified_address` |
| Consent | `consent.opted_in` for (`address_id`, class). Marketing requires explicit opt-in; transactional does not. | Terminal: `suppressed_consent` |
| Suppression | `destination_hmac` present in suppression list (hard bounce, complaint, regulatory). | Terminal: `suppressed_list` |
| Staleness | `source_updated_at` within threshold, for projection-resolved destinations only (§4.8). | Defer + alert |
| Quiet hours | See §6. Auth class exempt. | Reschedule |
| Rate limit | Per-provider token bucket, in-process. | Defer, retry next tick |

Consent enforcement was the single most consequential omission in the first draft of this design: sending marketing to an opted-out customer is a regulatory penalty, not a bug. It is now a hard gate with its own terminal state, so suppressed sends are visible and auditable rather than silently dropped.

**Consent keys on `address_id`, not on `customer_id` or on the raw address value.** Keying on the person is wrong because a customer may reasonably opt out of marketing on one address and not another. Keying on the value is worse: a recycled phone number would silently inherit the previous owner's opt-in. Because `customer_address` rows are append-only and a recycled number produces a *new* row, consent on the interval id means the new owner starts with no consent record — and absence of consent defaults to opted-out for marketing. Correct behaviour falls out of the schema rather than needing a cleanup job.

Suppression deliberately keys on the raw `destination_hmac` instead. It is fail-safe (it blocks sending), so over-suppressing a recycled number is the acceptable direction to err; entries carry a review date rather than living forever.

**The verification gate needs an explicit mode, because its input may not exist.** Whether the master system publishes per-address verification state on the event feed is still an open question. A gate whose input is always NULL either blocks every send or silently passes every send, and the second is what happens by accident. So `tenant_config.verification_mode` is explicit: `enforce` blocks unverified addresses; `observe` allows them but records the outcome and reports a count. A tenant whose feed carries no verification data runs in `observe` **visibly**, rather than believing a control is active when it is not.

Auth class skips verification and consent entirely — an OTP goes to a destination the caller supplied and vouched for (§4.8).

Ingestion also runs a **consent pre-filter** on bulk campaign submissions — not for correctness (the send-time gate is authoritative) but to avoid enqueueing hundreds of thousands of rows that will be discarded.

### 5.1 Producer quotas

**Two different limits, deliberately not called the same thing.** Conflating them is the usual way quota systems end up confusing to operate:

| Limit | Where | Protects | On breach |
|---|---|---|---|
| Admission rate | ingest API | messgr's own database from a runaway producer | `429`, immediate, retryable |
| Send quota | dispatcher | customers and provider spend | defer or alert, per enforcement mode |

**Quota is charged at dispatch, not at ingest.** Dispatch is where money is spent and where a customer is actually disturbed. It also resolves the scheduling question cleanly: a message scheduled three weeks out consumes the quota of the day it *sends*, not the day it was submitted. Charging at ingest would make future-dated campaigns silently eat a quota window nobody is watching.

**Counters live in-process.** A `UPDATE producer_quota SET used = used + 1` row would be a single hot row at 1600/sec. But §9 already establishes one active dispatcher process per tenant — and every producer belongs to exactly one tenant — so the counter is a plain in-memory map behind that single enforcer, with no coordination and no contention. It is flushed to `producer_usage` every few seconds for reporting, and rebuilt from that table on dispatcher startup so a restart mid-window does not reset a producer's daily allowance to zero.

**Windows:** a per-minute burst ceiling and a per-day total. Fixed windows, not rolling — a daily figure resetting at a known local midnight is something an operator can reason about and a producer team can plan against. Rolling windows are fairer and harder to explain; not worth it here.

**Enforcement mode is per (producer, channel, class), and the default matters:**

| Class | Default | Rationale |
|---|---|---|
| `marketing` | **hard** — defer to next window | Over-messaging is the actual risk; delay is harmless |
| `transactional` | **soft** — send anyway, alert loudly | A quota must never be the reason a fraud alert or a statement is withheld |
| `auth` | exempt, but counted | Never blocked. Still metered, because a spike in OTP volume is a security signal worth seeing |

> This asymmetry is the important part of the design. A quota system that can block transactional traffic has converted a cost-control feature into an availability risk. Hard limits belong on the traffic where delay is an acceptable outcome, and nowhere else.

Uplift for campaign days goes through `producer_quota_override` — time-boxed, approved, reasoned, and it expires on its own.

### 5.2 Kill switches

**Scopes**, because real incidents are rarely "disable this producer": `global`, `channel`, `producer`, `producer_channel`, `campaign`. Stopping one bad campaign or all SMS is the common case.

**Semantics.** Engaging a switch does three things:

1. New ingests matching the scope are rejected with a distinct error code (not a generic 500 — the producer team needs to know *why*).
2. Dispatch stops for matching queued messages.
3. Queued messages are **held**, not discarded, unless the switch specifies `on_queued = 'discard'`.

Hold is the default because most incidents end with "resume". Discard exists because sometimes they don't — a six-hour-old flash-sale blast firing after the sale ended is worse than never sending it.

**Correction: "held" was never given a representation, and the obvious one spins.** A row under an active switch cannot be left exactly as claim-eligible as any other: if the claim query (§4.2) simply leases it, finds the gate chain blocking it on kill switch, and drops the lease again, that row gets re-claimed the moment the lease's own timeout passes — repeatedly, for as long as the switch stays engaged, burning a claim-and-check cycle per lease interval for every held row. That is not "held", it is a busy-wait dressed up as one. The dispatcher must check kill-switch state **before** claiming, not after: it keeps the current set of engaged scopes cached in-process (refreshed by the same `NOTIFY`/30-second-poll pair described below) and excludes matching channels, producers, or campaigns from the claim query's candidate set entirely while a switch is active. A row genuinely held then sits untouched — no lease taken, no gate re-evaluation, no spin — until the switch releases and the drain-rate ramp below picks it up deliberately.

**Correction: ingest-side rejection (point 1 above) has no propagation mechanism that can reach it.** The `NOTIFY`-based propagation described below is read by the dispatcher, which holds a **direct** connection to Postgres (§2.3). `ingest-api` does not — it connects through PgBouncer in transaction mode, and §2.3's own table states plainly that `LISTEN` registration silently never fires under transaction pooling. So the mechanism that makes kill-switch propagation "seconds, not minutes" for the dispatcher cannot be the mechanism for ingest rejecting new requests; the design specified point 1 without specifying how `ingest-api` learns a switch fired at all. Fixed by giving `ingest-api` its own poll, independent of `LISTEN`: each instance re-reads `kill_switch` on its pooled connection every few seconds, caches the active set in-process, and rejects matching ingests against that cache rather than querying `kill_switch` per request. This is slower than the dispatcher's `NOTIFY` path and that is acceptable — a few seconds of ingest lag before a bad campaign starts being rejected is a different, looser bound than "dispatch stops within seconds", and the two were never the same requirement.

**Release is the dangerous half, and it is easy to overlook.** Disable a marketing producer for four hours, re-enable, and 500k held messages become dispatchable in the same instant. Release therefore ramps: the dispatcher drains a released backlog at a configured rate rather than at full speed. The `expires_at` check (§6.2) runs first, so genuinely stale held messages drop rather than arriving hours late.

**Auth is structurally immune, and the panel must say so.** A global kill switch does not stop OTP, because OTP does not go through the dispatcher at all (§3). This is correct behaviour — you do not want an operator locking every customer out of the bank during a marketing incident — but an operator who believes "global kill" stopped *everything* is operating on a false model. The admin panel states explicitly that auth traffic continues, and disabling the auth path is a separate control requiring **two-person approval**.

**Propagation must be seconds, not minutes.** Dispatchers cache config, so a switch fires `NOTIFY kill_switch` — a **separate channel** from the outbox wakeup of §4.2, with a separate 30-second fallback re-read rather than that path's 1-second poll. Two channels, two intervals, deliberately: queue wakeup optimises latency on every message, config re-read optimises nothing in steady state and only needs to be fast during an incident. A missed notification costs seconds, not correctness. A kill switch that takes effect on the next config reload is not a kill switch.

**Where the auth kill switch actually lives.** §3 puts OTP outside the dispatcher entirely, so it cannot read `kill_switch` — which would make the two-person-approval auth control above unimplementable as described. It lives instead in `sms-sender` / `otp-api`, which re-reads a dedicated `auth_enabled` flag from the control database on a short interval. It is deliberately a different mechanism in a different place, because the whole point of §3 is that the auth path shares nothing with the queue.

**Correction: "fails closed only for that flag" directly contradicted §3's central promise, and it is worth saying plainly why this was wrong.** §3 exists so that "a marketing incident cannot stop customers logging in" (AGENTS.md hard invariant 1) — the whole design of the OTP path is that it has no dependency on Postgres or Vault being reachable (§2.4 step, restated at the end of §3.1). A flag that fails *closed* when the control database is unreachable reintroduces exactly the dependency §3 was built to remove: a control-database outage — unrelated to any marketing incident, unrelated to any intentional switch — would now silently disable customer login. That is a worse outcome than the flag not existing at all. The flag must fail **open** (auth stays enabled) on a read failure or timeout, with alerting on the failure itself so an unreachable control database is visible and gets fixed — and fail exactly to whatever value it last successfully read otherwise. Two-person approval governs *engaging* the flag deliberately; it was never meant to govern what happens when nobody engaged anything and the database is just having a bad day.

**Every engage and release is audited** — who, when, why, and how many messages were held or discarded. In a bank this is the first thing asked about after the incident.

**Two tiers in cloud.** Tenant switches (`kill_switch`, in the tenant database) are operated by that tenant's own `comms_ops` role and cannot see or affect any other tenant. Platform switches (`platform_kill_switch`, in the control database, §4.11) are operated by the provider and can suspend a single tenant or the whole region — for abuse, non-payment, or a platform-wide incident.

A platform switch overrides a tenant's, never the reverse. Propagation still uses `NOTIFY`, but the control plane must fan out to each tenant database rather than issuing one notification, so the dispatcher's periodic re-read (30s) is the guaranteed path and `NOTIFY` is the fast path. A tenant cannot release a platform switch, and the tenant admin panel shows a platform suspension as a distinct, non-actionable state rather than a mysteriously stuck queue.

---

## 6. Send timing

### 6.1 Quiet hours

Three failure modes the naive implementation hits:

1. **Thundering herd.** Rescheduling every suppressed message to exactly `quiet_end` wakes an entire region's backlog at 07:00:00.000. Reschedule to `quiet_end + random_jitter(0, 30min)`. Plain uniform jitter — an earlier draft weighted it so transactional landed early in the window and marketing late, which is a knob nobody will tune and which duplicates the `ORDER BY priority` preemption already in the claim query (§4.2).
2. **Unknown timezone.** Null or missing customer timezone must fall back to an explicit institution default, configured per deployment — never to server-local time, and never to "send anyway".
3. **DST.** A message scheduled for 02:30 local on a spring-forward night refers to a moment that does not exist. Store everything in UTC, resolve windows with a real tz database (`chrono-tz`), and on a non-existent local time, round forward to the next valid instant.

```
policy: quiet_hours(region|segment) -> (start_local, end_local, tz)
resolve: customer tz -> segment policy -> institution default
```

Auth class skips this evaluation entirely (§3).

### 6.2 Scheduled delivery

The mechanism is nearly free: `outbox.next_attempt_at` already drives when a message becomes claimable, so a scheduled send is one whose `next_attempt_at` starts in the future. Future-dated rows sit in the claim index but sort after `now()`, so they cost nothing to skip. No scheduler process, no cron, no second queue.

The API accepts two forms, because marketing wants the second one and only ever gets offered the first:

| Form | Meaning |
|---|---|
| `scheduled_for` (absolute, UTC) | send at this instant |
| `scheduled_local` (time + optional date) | send at this **customer-local** time, resolved via `customer.timezone` |

Local-time scheduling is what "send at 9am on Tuesday" actually means for a campaign spanning timezones. It resolves through the same tz machinery and non-existent-local-time handling as §6.1 — the DST edge case is identical and must not be solved twice.

**All gates run at dispatch, never at schedule time.** This is what makes scheduling safe rather than dangerous: a message queued three weeks ago is checked against consent, suppression, verification, quota, and kill switches *as they stand at the moment of sending*. A customer who opts out on Monday does not receive Tuesday's pre-scheduled marketing. This falls out of §5 already; it is worth stating because the obvious alternative — validating at submission — would create a large and silent compliance hole.

**Cancellation is mandatory, not optional.** Anything schedulable must be cancellable: the offer was pulled, the account closed, the campaign was wrong. `DELETE /comms/{id}` sets `outbox.cancelled_at`. The race is real — a cancel can arrive while the dispatcher holds the lease — so the dispatcher **re-reads `cancelled_at` immediately before the provider call**, and the API returns `409 Already Sent` when it lost. Reporting a cancellation that did not happen is worse than failing to cancel.

**`expires_at` — drop rather than send late.** If dispatch is stalled (provider outage, kill switch, quota hold) a scheduled message can come due hours after it mattered. "Your flash sale ends at noon" arriving at 3pm is worse than silence. `expires_at` is checked first in the gate chain (§5) and produces a terminal `expired` state that is visible in reporting rather than a silent drop. Optional, but strongly recommended for anything time-bound.

**Maximum horizon, default 90 days.** Unbounded scheduling is a slow-acting footgun: a message scheduled two years out will fire against a template that has since been superseded, for a product that may be withdrawn, to a customer who may have left. Requests beyond the horizon are rejected unless the producer holds an explicit override.

**Outbox growth.** Scheduled messages occupy the outbox for their full delay, which is in tension with §4.2's "small and ephemeral" premise. At realistic ratios (scheduled volume is a small fraction of immediate) this is comfortable, and future-dated rows are inert. Monitor outbox row count as a first-class metric; if far-future scheduling ever becomes a bulk pattern, move rows beyond ~7 days into a separate `scheduled` table promoted nightly. Do not build that until the metric says so.

### 6.3 Precedence when timing rules collide

| Situation | Outcome |
|---|---|
| Scheduled time falls inside quiet hours | Quiet hours wins — deferred to window end + jitter. Scheduling is a producer convenience; quiet hours is a customer protection |
| Scheduled + `expires_at` before quiet-hours window ends | Message expires and is dropped. Correct: it cannot be sent legally *and* on time |
| Scheduled + kill switch active at due time | Held, subject to `expires_at` on release (§5.2) |
| Scheduled + quota exhausted at due time | Marketing defers to next window; transactional sends with an alert (§5.1) |
| Auth class | Never scheduled. Auth is synchronous by definition (§3); the API rejects `scheduled_for` on auth-class requests |

---

## 7. PII, retention, and the erasure conflict

Storing rendered message bodies for 7 years means storing account numbers, balances, and names for 7 years — and a GDPR erasure request then collides head-on with a regulatory retention obligation. Neither can simply win.

Two erasure modes are supported. **Crypto-shredding is the default**; physical redaction is available when a regulator or legal instruction demands that the bytes actually leave the disk.

### 7.1 Mode 1 — crypto-shredding (default)

- `payload_ciphertext` is encrypted with a per-customer data encryption key (DEK), AES-256-GCM.
- DEKs are generated and wrapped by **HashiCorp Vault's Transit engine**; the KEK never leaves Vault. See §7.6.
- Erasure → destroy the wrapped DEK, retain the row as a tombstone with `shredded_at` set. Ciphertext stays in the ledger for the full retention clock, satisfying the regulator; plaintext is unrecoverable, satisfying the data subject.
- Non-personal metadata (timestamps, channel, template_id, delivery status) survives and continues to answer "was this customer contacted, when, on what channel" — which is what most compliance queries actually need.

Properties: **O(1)** — one row touched, no partition rewrite, no `VACUUM`, no background job, completes in milliseconds regardless of how many messages the customer ever received. That is why it is the default.

### 7.2 Mode 2 — physical redaction (on request)

Overwrite the PII-bearing columns in place across all of a customer's ledger rows:

```sql
UPDATE comms_request
SET payload_ciphertext     = NULL,
    destination_ciphertext = NULL,
    destination_hmac       = '\x00'::bytea
WHERE customer_id = $1;

-- delivery receipts echo the recipient address back in provider JSON (§4.4)
UPDATE comms_event
SET provider_payload_ciphertext = NULL
WHERE customer_id = $1;

-- projection rows carry plaintext-equivalent PII too
UPDATE customer_address
SET value_ciphertext = '\x00'::bytea, value_hmac = '\x00'::bytea
WHERE customer_id = $1;
DELETE FROM customer_external_id WHERE customer_id = $1;
```

The `comms_event` statement was absent from an earlier version of this design, which meant physical redaction left the recipient address readable in provider payloads. Any new table holding third-party data must be added here at the same time it is created — the erasure surface is easy to grow without noticing.

**Named exemption: `suppression` is deliberately not touched by erasure, and this needs to be a stated decision rather than a gap someone finds later.** `suppression` holds `destination_hmac` with no `customer_id` column, so it cannot join into `WHERE customer_id = $1` at all — and that absence is structural, not an oversight: the table's entire purpose is to block future sends to a bad *destination*, regardless of which customer currently holds it (§5's suppression gate). A customer's erasure request must not silently un-suppress a hard-bounced or complained-about number for whoever is issued it next. So: a suppression entry outlives the customer that triggered it, by design, until its own review date (§5) retires it independently. The mechanical CI check in §14 that walks the schema for customer-linkable columns must carry this table as a named, reasoned exemption — not a silent absence from the erasure statements above, which is indistinguishable from the `comms_event` miss this section already recounts.

`customer_id` itself is retained as an opaque UUID — it carries no personal information once the projection is redacted, and keeping it preserves the timeline's structural integrity and the ledger's foreign keys.

Deliberately a **redaction, not a row DELETE**. The row skeleton survives so that message counts, campaign reach figures, and delivery statistics remain accurate and reconcilable. Deleting rows outright would silently change historical aggregates and destroy the referential target of `comms_event`. Full row purge is available as a third, explicitly-authorized mode, but it is the nuclear option and it does corrupt historical counts — do not offer it as a routine choice.

Operational consequences, all of which matter:

- **Cold partitions must be writable — a permanent constraint bought by an optional feature.** Partitions older than 18 months sit on slower storage but must not be marked read-only, or redaction cannot reach them; anything genuinely immutable becomes un-erasable. Worth naming the trade: this forecloses read-only or WORM storage tiering for the entire seven years, on behalf of a mode that may never be exercised. If no regulator ever demands physical deletion, the constraint was paid for nothing. It is accepted because discovering you cannot comply, *after* seven years of data has landed on immutable storage, is the strictly worse outcome.
- **`VACUUM` is mandatory afterwards.** The pre-update tuple still holds the old ciphertext until vacuumed. The erasure job runs `VACUUM` on affected partitions before reporting completion.
- **Cost is O(rows), across up to 84 partitions.** Run as a background job against an index on `customer_id`, throttled, never synchronously in a request handler.

### 7.3 The backup window — applies to *both* modes

An earlier draft of this design claimed crypto-shredding "reaches every backup, because no backup contains the key". **That is wrong when the wrapped DEK lives in Postgres**, and it is worth stating plainly because it is an easy and consequential mistake to make.

- *Physical redaction* does not reach WAL archives or base backups already written; they hold the pre-redaction tuple.
- *Crypto-shredding* does not either. Restoring a pre-erasure Postgres backup restores the wrapped DEK, and Vault still holds the KEK that unwraps it. The erasure is reversible by anyone who can restore a backup.

So for both modes: **true erasure completes when the last backup containing the data ages out.** With a 90-day backup retention window, an erasure requested today is genuinely complete in 90 days. `erasure_request.backups_clear_at` records that date, and the compliance report states it explicitly rather than claiming instantaneous deletion.

This is a property of every point-in-time-recoverable database, not a flaw introduced here. Crypto-shredding's real advantage over physical redaction is **cost and blast radius**, not backup reach: one row versus a throttled rewrite of up to 84 partitions plus a `VACUUM` pass.

Wrapped DEKs live in Postgres (Decisions taken #6), so this window applies. The deferred hardening in §7.6 would close it if a regulator ever challenges erasure timeliness.

**Backup retention is per cluster, not per tenant.** `tenant_config.retention_years` lets one tenant keep seven years of messages and another keep three — but WAL and base backups cover the whole cluster, so the backup window that governs erasure completion is shared by every tenant on it. A tenant wanting a 30-day window for fast erasure and one wanting 90 days for recovery confidence cannot both be satisfied on the same cluster.

Consequence, and it belongs in the contract rather than in a config table: **backup policy is uniform per region**. Tenants with conflicting requirements go on separate clusters — which is a provisioning decision at onboarding, not something to discover during a regulator conversation.

### 7.4 Auth payloads

**Auth class stores no payload at all.** `payload_ciphertext` is NULL for OTP. The code is a live credential and must never be retained; the metadata record alone is sufficient for audit. No erasure mode needs to touch it.

### 7.5 Retention mechanics

Partitions older than 18 months move to a slower storage tablespace (writable — see §7.2). Nothing is deleted until the 7-year boundary, at which point the whole partition is detached and dropped.

### 7.6 Key management — HashiCorp Vault

No HSM is available, so Vault is the root of trust. Vault also absorbs the rest of the secrets story (DB credentials, provider API keys, internal PKI), which is the main reason to prefer it over rolling a key table with a KEK in a config file.

**Engine: Transit.** The KEK never leaves Vault; messgr never sees it.

```
create DEK   POST transit/datakey/plaintext/messgr-dek
             -> { plaintext: <DEK>, ciphertext: "vault:v1:..." }
             store ciphertext in customer_dek.wrapped_dek, use plaintext, zeroize

unwrap DEK   POST transit/decrypt/messgr-dek  { ciphertext }
             -> plaintext DEK (cached in memory, see below)
```

**KEK rotation is cheap and never touches the ledger.** `transit/keys/messgr-dek/rotate` mints v2; a background job calls `transit/rewrap` over `customer_dek` to re-wrap each DEK under v2 without ever exposing plaintext; `min_decryption_version = 2` then retires v1. Cost is O(customers), not O(messages) — `payload_ciphertext` is untouched. Annual rotation is a routine job, not a migration.

**Vault must not be on the hot path.** An in-process LRU cache holds unwrapped DEKs (bounded size, TTL, zeroized on eviction). Steady-state message sending makes zero Vault calls. Vault is contacted only on cache miss or for a customer's first-ever message.

**DEK pre-provisioning.** A batch job creates DEKs ahead of the customer base rather than lazily on first send, so a sealed or unreachable Vault cannot block ingestion for a new customer.

> **Operational cost of having no HSM — read this before committing.**
> Vault's own master key needs protecting by *something*. With no HSM and no cloud KMS available on-prem, PKCS#11 auto-unseal is off the table, leaving **Shamir unseal: 5 key shares, threshold 3**. Every Vault node must be manually unsealed by three keyholders after any restart, including unplanned ones and including one node at a time during rolling upgrades of a 3-node Raft cluster.
>
> Concretely: a Vault restart at 3am pages three people. This is the single largest recurring operational burden the "no HSM" decision creates, and it should be a conscious acceptance rather than a discovery made during the first incident. Two ways to reduce it: (a) deploy Vault as a 3-node Raft cluster so a single node restart never seals the service, and (b) size the DEK cache and pre-provisioning window so messgr keeps sending normally throughout a full Vault outage. Both are recommended.
>
> If an HSM becomes available later, switching to auto-unseal is a Vault configuration change and a `seal migrate` — it does not affect messgr's code or schema. Worth revisiting once the ops burden is felt.

**Per-tenant KEKs.** Each tenant gets its own Transit mount and key. `tenant.vault_mount` (§4.11) records the **mount path only** — `transit/<tenant_slug>` — never the key name appended; the key inside that mount is always the fixed name `messgr-dek`, since mount varies per tenant and key name never does. (Corrected here: an earlier draft of this paragraph wrote the combined identifier `transit/<tenant_slug>/messgr-dek` as if that whole string were the value of `tenant.vault_mount`. It reads naturally as prose but is wrong as an implementation instruction — Vault's own Transit API takes mount and key name as two separate path segments, so a literal reading would have doubled the key-name segment the first time code actually built a request from the column. Caught during T-004's refinement, before any code shipped against it.) Vault policies bind an AppRole to exactly one mount, so a compromised tenant credential cannot decrypt another tenant's data, and no single key exists whose loss exposes the platform.

This makes two operations trivial that would otherwise be projects:

- **Tenant offboarding is O(1) — in the destroy case.** Destroy the tenant's Transit key and every payload and destination in their database becomes permanently unreadable, immediately, without touching a single row. `DROP DATABASE` then follows at leisure as space reclamation rather than as the security-critical step. Note that this covers only one of the two offboarding modes; see §7.7.
- **Platform operators cannot read tenant message content.** Operator credentials carry no Transit policy for any tenant mount, so this is not a promise about access control lists — the operator does not hold a key that decrypts the data. Break-glass requires a tenant-approved policy grant, logged in `platform_audit`.

> **Scope that claim precisely, because the loose version will not survive a careful security questionnaire.** Operators hold Postgres superuser. Everything unencrypted is readable to them: who was contacted, when, on which channel, under which template, and campaign membership. *"Bank X messaged these 400,000 customers about mortgage arrears"* is highly sensitive without a single body being decrypted.
>
> The defensible statement is: **operators cannot read message content; they can read metadata; metadata access is audited.** That is still a materially stronger position than most vendors offer — but claim it accurately.
>
> This also forces a detail: **the `destination_hmac` pepper must be per-tenant**, derived from that tenant's own Transit mount. A platform-wide pepper would let an operator test whether a known phone number appears in any tenant's ledger — turning a one-way hash into a confirmation oracle across the whole customer base. **Mechanism (T-008):** the pepper is minted once as an ordinary Transit datakey (the same primitive as a customer DEK, just not tied to a `customer_id`), its wrapped ciphertext persisted on the tenant row, and the unwrapped plaintext held in the same bounded, zeroizing, TTL cache as DEKs — HMAC-SHA256 is then computed locally. Calling Vault's native `transit/hmac` endpoint per lookup was considered and rejected: it would put Vault back on the hot path for every message, undoing the point of the DEK cache and pre-provisioning above.

> **Licensing note.** Vault *namespaces* are an Enterprise feature. This design deliberately uses separate **mounts plus policies** within a single namespace instead, which is available in the open-source edition and sufficient for cryptographic separation at 20 tenants. Namespaces would add administrative separation (tenants managing their own Vault policies), which is not a requirement here. Confirm before assuming an Enterprise licence is needed.

**Service authentication:** AppRole, with the SecretID delivered response-wrapped at deploy time. Vault tokens are short-lived and renewed by the agent, not baked into config. Each tenant dispatcher holds its own AppRole bound to its own mount.

**Regional Vault clusters.** Each region runs an independent 3-node cluster (§2.2). Note that this multiplies the Shamir unseal burden by region — three keyholders per region, and they should not be the same three people if regions are meant to be jurisdictionally independent. Factor this into the operational cost of adding a region.

**Local development:** `vault server -dev` in the Compose file, same trait-and-guard pattern as `MockProvider` (§11.1) — the binary refuses to start against a dev-mode Vault unless `profile = "dev"`.

**Deferred hardening — wrapped DEKs in Vault instead of Postgres.** *Not being built now; recorded so the option stays visible.* Storing `wrapped_dek` in Vault KV v2 (keyed by customer) rather than in the `customer_dek` table would mean Postgres backups contain **no key material at all**, so crypto-shredding would no longer be bounded by the database backup window (§7.3) — only by Vault's own, which is far shorter and covers a much smaller dataset. Erasure becomes `vault kv metadata delete`, destroying all versions. Cost: Vault moves onto the read path for cache misses, so cache sizing and pre-provisioning matter more.

The migration path if this is ever needed: read each `customer_dek` row, write the same wrapped value to Vault KV, drop the column. No re-encryption of `payload_ciphertext`, no unwrapping of DEKs, no ledger downtime. Keeping `wrapped_dek` opaque to the rest of the system — accessed only through a single `KeyStore` trait — is what preserves that path, so **do not let the column leak into queries or the API surface**.

### 7.7 Tenant offboarding — two modes

"Destroy the key" assumes termination means deletion. It often does not: a departing institution may be contractually and legally required to *retain* seven years of records for its own regulator, which is the opposite instruction.

| Mode | Action | Cost |
|---|---|---|
| **Terminate and destroy** | Destroy Transit key, `DROP DATABASE`, revoke AppRoles. Data unreadable immediately; backup window per §7.3 still applies. | O(1), minutes |
| **Terminate and archive** | Database and Transit key retained for the tenant's remaining retention period. Producers and UI disabled; dispatchers stopped; read access via a restricted export path only. | Ongoing storage, key custody, and unseal obligations for years after the commercial relationship ends |

The second mode is the one that hurts, and it needs pricing rather than engineering: the platform continues to hold a live decryption key and an obligation to keep it recoverable, for a customer no longer paying. It also means "offboarded" tenants still appear in the Vault unseal blast radius and in every future key-rotation job. `tenant.status` distinguishes the two, and the archive mode should carry an explicit end date after which it converts to destroy.

---

## 8. Throughput and campaign traffic

The average rate (5M/day ≈ 58/sec) is not the design target and should not be used for sizing. Marketing is bursty: a 2M-recipient campaign submitted as a batch is ~1600/sec sustained if drained over 20 minutes.

Two consequences:

**Bulk ingestion path.** `POST /comms/bulk` accepts a newline-delimited JSON stream and lands it via `COPY` into a staging table, then a single set-based INSERT into ledger and outbox. Submitting 2M individual POSTs is not supported and would not survive contact with the network.

**Constraint this path must satisfy, mechanism not yet chosen.** `COPY` writes `payload_ciphertext` and `destination_ciphertext` directly — both must already be encrypted under the right per-customer DEK by the time a row reaches the staging table, per hard invariant 7 (every payload written under a customer-scoped key, no exceptions for volume). A 2M-recipient campaign can span up to 2M distinct customers, so the batch may need up to 2M distinct DEK unwraps before or during staging — an order of magnitude past what §7.6's steady-state cache (sized and pre-provisioned for ordinary per-message traffic) was reasoned about. Whatever the bulk path does — pre-provision DEKs ahead of the batch, warm the cache from the campaign's recipient list before the `COPY` starts, or something else — it must be decided with this number in view, not discovered when the first real campaign is slow or a Vault mount starts throttling. Mechanism is step 18's problem; this paragraph exists so it isn't invisible until then.

**Preemption, and nothing more.** An earlier version of this section gave marketing "a configured fraction of each channel's rate budget". That is a second mechanism doing the same job as `ORDER BY priority` in the claim query (§4.2) — transactional rows are already claimed before marketing rows, which *is* preemption. The fraction added a tunable that nobody would tune and whose realistic failure mode is marketing starvation, the opposite of the problem it was written for.

Cut. The claim ordering is the whole mechanism: a campaign drains with whatever capacity transactional traffic leaves, and a fraud alert never waits behind one. If marketing starvation becomes real, the fix is a floor on marketing throughput — the inverse knob, added with evidence.

Note also that "admission" now means exactly one thing in this document: the ingest-side rate limit of §5.1. It is not reused for dispatcher-side scheduling.

---

## 9. Rate limiting and dispatcher topology

Provider rate limits need a single enforcement point. Multiple dispatcher processes have no shared view of the budget; a shared token bucket in Postgres means every send contends on one row, and static per-instance quota partitioning is wasteful and breaks when an instance dies.

**Chosen approach: one active dispatcher process per tenant, handling all of that tenant's channels.**

Because tenants bring their own provider accounts (§4.10), every rate limit is already scoped to a tenant. A single process per tenant is therefore a single enforcer for every `(channel, provider)` pair it owns — plain in-process token buckets, no coordination, no contention. Scale comes from internal async concurrency (tokio, hundreds of in-flight provider calls), which is ample for the 1600/sec peak of §8.

Multi-tenancy strengthens this choice rather than complicating it. The alternatives are worse:

| Alternative | Why not |
|---|---|
| One dispatcher per channel, all tenants | Shared fate — one process failing stops every tenant. Needs per-tenant fairness scheduling, per-`(tenant, provider)` breakers, and per-tenant concurrency caps, all to reconstruct isolation the process boundary gives for free. |
| One per `(tenant, channel)` | 20 tenants × 3 channels × 2 for standby = 120 processes per region, to solve a problem that does not exist once providers are per-tenant. |

At ~20 tenants this is 40 processes per region including standbys — lightweight tokio processes, each holding one connection pool to one tenant database. A small tenant running a mostly-idle process is acceptable waste at this scale; it would not be at a thousand tenants, which is the point at which the shared-schema consolidation path (§2.1) becomes worth taking.

**Blast radius is one tenant.** A dispatcher crash, a poisoned message, a wedged provider client, a memory leak — all contained. Nothing about tenant A's traffic can delay tenant B's OTP.

**HA without losing the single enforcer:** two processes per tenant. Both start; each attempts `pg_try_advisory_lock()` **in that tenant's own database**, over a **direct connection that bypasses PgBouncer** (§2.3 — a session advisory lock taken through a transaction-mode pooler does not hold, and would silently permit two active dispatchers for the same tenant). The winner dispatches, the loser idles and retries every few seconds. If the active process dies its session ends, Postgres releases the lock, and the standby takes over within seconds. Leader election with no new infrastructure, and the lock namespace is naturally partitioned because advisory locks are scoped per database.

Per-provider circuit breakers (open after N consecutive failures, half-open probe) sit inside each dispatcher, alongside exponential backoff with jitter on retryable failures.

**The remaining shared resource is the Postgres cluster itself, and this is not solved — only bounded.** Postgres has no per-database CPU or IO quota. `CONNECTION LIMIT` caps concurrency, not resource consumption: one tenant's 2M-row `COPY` on the primary, or a runaway analytical query on the replica, degrades every tenant on that cluster regardless of connection caps.

What is actually available: per-tenant `CONNECTION LIMIT`, per-tenant `statement_timeout`, and per-database IO monitoring to identify the culprit quickly. What closes it properly is the escape hatch — relocating a tenant to its own cluster is a `pg_dump` and restore rather than a redesign, which is a genuine benefit of the database boundary. Treat noisy-neighbour risk as a monitored operational condition with a known remediation, not as a solved problem.

---

## 10. Delivery receipts

Provider webhooks arrive duplicated, out of order, and occasionally **before** the sending transaction has committed. The design must tolerate all three:

- `comms_event` has a natural-key unique constraint; inserts use `ON CONFLICT DO NOTHING`. Duplicates are free.
- Events are keyed on `provider_ref` and never assume ordering. A `delivered` arriving before a `sent` is recorded as-is; the UI renders by `occurred_at`, not arrival order.
- A receipt for an unknown `provider_ref` goes to a small `orphan_event` table and is reconciled on a short delay rather than discarded.

**Tenant routing must not use the tenant slug.** A callback URL is configured in the provider's console and is effectively public; `/webhook/acme/twilio` lets anyone enumerating paths discover which institutions are customers. Route on an **opaque per-tenant webhook token** instead — `/webhook/7f3a…/twilio` — rotatable independently of the tenant slug, and carrying no information if leaked.

**Network placement:** the webhook receiver is the only internet-facing component in an otherwise internal system. It runs as a separate minimal binary in the DMZ, does provider signature verification and nothing else, and writes through a narrowly-scoped DB path. This is a firewall and network-segmentation conversation to have with infrastructure early — it is usually the longest-lead item in an on-prem deployment.

**Constraint this path must satisfy, mechanism not yet chosen.** A delivery receipt's raw provider JSON is stored encrypted under the customer's DEK (§4.4), which means `messgr-webhook` — the one DMZ-facing binary, deliberately kept to "signature verification and nothing else" — needs some way to get that encryption done. Holding a Vault AppRole with decrypt/encrypt policy for every tenant mount directly in the DMZ binary would make the platform's most exposed process also its widest keyholder, the opposite of "narrowly-scoped". Deferring encryption to an internal process instead means the raw payload crosses the DMZ boundary and sits briefly unencrypted somewhere on the internal side before that process picks it up — survivable if that window is short and the interim store is itself access-controlled, but it is a real design choice with a real exposure window, not a detail. Which of these (or another shape) is correct needs answering before step 12, not assumed by silence.

---

## 11. Query API and UI

**Reads never touch the primary.** `query-api` connects to a streaming replica. A compliance analyst running an unbounded date-range search cannot then starve transactional ingestion.

**UI:** server-rendered Askama templates plus htmx, in the same binary as the query API. No separate frontend build or deploy pipeline. The application is search-and-inspect over tabular data; a SPA would add a toolchain without adding capability.

Views: customer timeline (all channels, chronological), message detail (template version, rendered content subject to §7, full event history), and campaign reach summary.

**API:** REST with a published OpenAPI spec.

```
POST   /comms                    enqueue; optional scheduled_for | scheduled_local, expires_at
POST   /comms/bulk               batch submit (NDJSON stream)
DELETE /comms/{id}               cancel before dispatch; 409 if already sent (§6.2)
GET    /comms                    filter: customer_id, channel, class, campaign_id, producer_id,
                                 from, to, status, scheduled
GET    /comms/{id}               detail plus event history
GET    /customers/{id}/timeline

GET    /producers/{id}/usage     live quota consumption, current minute + day windows
GET    /producers/{id}/quota     configured limits and any active override
```

Producer identity is never taken from the request body — it is derived from the mTLS client certificate (§4.9). A producer cannot spend another producer's quota or evade its own kill switch by relabelling itself.

### 11.1 Authentication and authorization

Service-to-service: mTLS with per-client scopes separating read from write. The client certificate resolves to a `(tenant_id, producer_id)` pair, so tenant context is established by the transport rather than asserted in a header — a producer cannot address another tenant even if it tries.

UI: **OIDC authorization code flow with PKCE, against the tenant's own IdP.** Each tenant configures its issuer, client id, and group claim in `tenant_config` (§4.10); an institution's staff authenticate against their own directory and never receive platform credentials. Tenant resolution happens from the hostname or path prefix before the OIDC flow starts, so a user is only ever offered their own institution's login.

Behind an `AuthProvider` trait with two implementations:

| Implementation | Use |
|---|---|
| `OidcProvider` | production — real IdP, discovery document, JWKS validation, group-to-role mapping |
| `MockProvider` | local development and tests — static user table, no network, role selected by config |

> **Security control:** `MockProvider` must be impossible to enable outside development. The binary refuses to start if `auth.provider = "mock"` while `profile != "dev"`, and logs a fatal error naming the offending config key. A mock auth provider reachable in production is a full authentication bypass; this guard is not optional, and it belongs in the first commit that introduces the trait rather than being retrofitted.

Roles are enforced server-side on every query, never in the UI layer:

| Role | Scope |
|---|---|
| `customer_service` | single-customer lookup only. Must supply a `customer_id`. Cannot list, cannot export, cannot run campaign queries. |
| `compliance` | unrestricted search, campaign queries, bulk export. Every access written to an access-audit log. |
| `campaign_ops` | campaign-scoped aggregates and delivery stats. No message bodies. |
| `comms_ops` | quota dashboard, engage/release kill switches, cancel scheduled sends. No message bodies, no customer search. |
| `admin` | template approval, quiet-hours policy, provider config, producer registry, quota overrides. No message-body access. |

The `customer_service` restriction matters: a support agent should be able to answer "what did we send you last week" without being able to enumerate the customer base. Separating that from `compliance` is the difference between a support tool and a data-exfiltration surface.

`comms_ops` exists so that pulling a kill switch during an incident does not require an account that can also read customer messages. Incident response is a 3am activity performed by whoever is on call; it should not be gated behind the most privileged role in the system.

### 11.2 Query patterns

- *Customer-scoped* ("everything sent to customer X, across every channel and address") — one index scan on `(customer_id, created_at DESC)`, **no join to the customer projection**, because the ledger carries `customer_id` and the destination on every row (§4.1). The id set is first expanded through `customer_alias` so merged customers show a continuous history. Dominant customer-service path, trivially fast, and unaffected by projection or event-feed outages.
- *Address-scoped* ("who did we contact on this number") — index on `destination_hmac`, computed with the Vault-held pepper. Results are grouped by `customer_id` so a recycled number shows as two distinct owners rather than one merged timeline.
- *Campaign-scoped* ("reach and delivery rates for offer X") — expected roughly **biweekly**. Served by a plain indexed scan on the replica: `(campaign_id, created_at)` on `comms_request`, aggregating on `final_status`. Recipient-level questions ("list the customers who received offer X") use the same index.

**No rollup table and no OLAP tier at launch.** Biweekly does not justify either. A rollup was specified and cut (§4.5) because it contradicted this same reasoning and added a late-delivery-receipt correctness problem. Documented trigger to revisit: campaign queries exceeding ~30s, or measurably degrading UI latency on the replica. Build it when that happens, not before — and when it happens, decide first how late-arriving receipts reconcile.

### 11.3 Admin panel

Same binary, same server-rendered stack, gated to `comms_ops` and `admin`.

**Quota dashboard — "online reporting".** Per producer × channel × class: current-minute and current-day consumption against limit, sparkline over the trailing 24 hours, and a blocked/deferred count. Backed by `producer_usage`, which the dispatcher flushes every few seconds, so freshness is sub-minute without a metrics stack in the read path.

The panel reads Postgres deliberately. Prometheus metrics are also emitted for alerting, but **the operational view must not depend on the monitoring system being healthy** — the moment you most need to see what a producer is doing is the moment a metrics pipeline is most likely to be part of the incident.

**Kill-switch console.** Engage and release by scope, with a mandatory reason. Before engaging, the panel shows the blast radius: how many queued messages the scope currently matches, broken down by class. Engaging without that number in front of you is how a marketing kill accidentally holds a statement run.

Permanently displayed on this screen: *auth traffic is not affected by any switch shown here* (§5.2). The one control that does affect it is separated, styled differently, and requires two-person approval.

**Scheduled queue view.** Pending future-dated messages by producer, campaign, and due window, with cancel actions. This is the screen someone reaches for when a campaign goes out wrong and needs pulling before it fires.

**Producer registry.** Register, disable, set quotas, grant time-boxed overrides. Every mutation audited with actor, timestamp, and before/after values.

### 11.4 Platform console

A separate surface, served by `messgr-control` against the control database, for provider staff. It is **not** the tenant admin panel with extra permissions — a different binary and a different authentication realm, because the failure mode of conflating them is an operator accidentally acting inside a tenant.

What it shows: tenant lifecycle (provision, suspend, offboard), `tenant_schema_version` drift across the fleet, per-tenant health and volume, platform kill switches, and the `platform_audit` trail.

What it cannot do, by construction: read message **content**. Platform operators hold no Transit policy for any tenant mount (§7.6), so payloads are not decryptable with operator credentials. Break-glass content access requires a tenant-granted policy and lands in `platform_audit`.

What it *can* see, and what tenants must be told: metadata. Timestamps, statuses, counts, error codes — but also recipient counts per campaign and template identifiers, which is enough to infer a good deal about a tenant's business. Support work runs on exactly this data, so restricting it further would break support. The honest framing is that operators see metadata and every access is audited, rather than that operators see nothing.

**Provisioning a tenant** is one scripted operation: create the database, run migrations to current version, create the Vault mount and Transit key, issue AppRoles, register in `tenant`, start the dispatcher pair. It must be a single command from day one — a provisioning process assembled by hand is how tenant configurations drift apart and how the twentieth tenant ends up subtly different from the first.

---

## 12. Failure modes and degraded operation

Centralizing communications creates a single point of failure. The mitigations must be explicit:

| Failure | Behaviour |
|---|---|
| messgr fully down | OTP unaffected (§3). Transactional and marketing queue at the producer side; producers must treat `POST /comms` failures as retryable and buffer. |
| Postgres primary down | Ingestion fails (retryable). Dispatchers stall — which incidentally means nothing is being sent, so the urgency of a kill switch drops. UI degrades to read-only against the replica. Recovery is standard Postgres failover. |
| Primary degraded but sending continues, and a kill switch is needed | **The awkward case.** The admin panel writes switches to the primary, so a partially-failed primary is exactly when the control is hardest to reach. Mitigations: the kill-switch write path is a single tiny transaction on its own small connection pool, so it survives conditions that starve bulk traffic; and the runbook includes the direct `psql` statement to engage a switch, tested and kept alongside the on-call notes. Do not let the only path to stopping the system be a web form. |
| One provider down | Circuit breaker opens; that channel's queued messages back off and retry, draining when the provider recovers. Other channels unaffected. **Except OTP — see §12.1.** |
| Dispatcher crash | Leases expire, standby acquires the advisory lock, work resumes. At-least-once delivery — see below. |
| Marketing backlog | Transactional preempts via claim-order priority, not a share-of-budget cap — that mechanism was specified and cut (§8) because it duplicated `ORDER BY priority` for a tunable nobody would tune. Auth is on a separate path entirely. |
| One tenant's dispatcher wedges | Contained to that tenant — one process, one database, one advisory lock (§9). Standby takes over. No other tenant observes anything. |
| One tenant saturates cluster IO | **Bounded, not prevented** — Postgres has no per-database IO or CPU quota. `CONNECTION LIMIT` and `statement_timeout` cap concurrency and runaway queries; per-database IO monitoring identifies the culprit. Remediation is relocating that tenant to its own cluster — a dump and restore, not a redesign (§9). |
| One tenant needs a point-in-time restore | Full-cluster recovery to a side instance, then logical extraction of that tenant (§13). Other tenants stay live throughout, but RTO reflects a cluster restore. Requires a rehearsed runbook and spare capacity. |
| Migration fails on one tenant | `tenant_schema_version` (§4.11) makes drift visible rather than latent. That tenant's binaries stay pinned to the prior version until reconciled; the other nineteen proceed. Migrations must therefore be backward-compatible for one version — expand/contract, never rename-in-place. |
| Region loses Vault | That region's sends degrade per §7.6. Other regions are wholly unaffected — no shared control plane, no cross-region dependency (§2.2). |
| Producer floods with a runaway loop | Admission rate limit rejects at ingest (`429`) before the DB is touched; send quota caps what actually reaches customers. Operator can engage a producer-scoped kill switch within seconds (§5.2). Other producers unaffected. |
| Kill switch released onto a large backlog | Drain-rate limiting ramps dispatch rather than firing everything at once; `expires_at` drops genuinely stale held messages first (§5.2). |
| Dispatcher restart mid-quota-window | In-process counters rebuild from `producer_usage` on startup, so a restart does not silently reset a producer's daily allowance (§5.1). |
| Customer event feed down | Ledger, timeline, and UI unaffected — none of them read the projection (§4.1). Sending is entirely unaffected too: every request supplies its own destination (§4.8), so a stale or stopped feed degrades only identity resolution — a provisional customer may persist longer than it should — never delivery. |
| Vault sealed or unreachable | Sending continues normally on cached and pre-provisioned DEKs (§7.6). Degrades only for customers whose DEK is neither cached nor pre-provisioned, and for UI payload decryption on cache miss. Metadata-only views stay fully functional. Alert fires immediately — a sealed Vault needs three keyholders, so time-to-recover is human-bound. |

**Delivery semantics are at-least-once.** A crash between provider acknowledgement and DB commit will resend. Provider-side idempotency keys are used where the provider supports them; otherwise a small duplicate rate is accepted for transactional and marketing. This is why OTP does not use this path.

### 12.1 Single-provider risk — accepted, with a caveat

Running one provider per channel with backoff has been accepted for now. For queued traffic that is a sound trade: a provider outage delays marketing and transactional messages, the outbox absorbs them, and they drain on recovery. No data is lost.

**The OTP path is different, and the two decisions compound.** OTP is synchronous by design (§3) and therefore has no queue to absorb an outage. A single SMS provider going down means customers cannot log in — a tier-0 authentication outage caused by a third party, with no automatic mitigation.

Minimum mitigations for launch, none of which require building failover now:

- `provider_config` holds an **ordered list** per channel even when it currently has one entry, so adding a second provider is a config change rather than a schema migration and code refactor.
- SMS provider config is **hot-reloadable**. Ops can cut over without a deploy or restart, which is what determines the difference between a 5-minute and a 90-minute auth outage.
- A written runbook for manual cutover, exercised at least once before go-live. An untested runbook is not a mitigation.
- Alerting on OTP send failure rate with a much tighter threshold than the other channels.

Recommend revisiting genuine SMS failover before the OTP path carries production auth traffic (build step 17). It is the one place where "single provider" translates directly into "customers locked out of their bank".

---

## 13. Deployment

Six binaries. The same set runs on-prem and in each cloud region; on-prem simply has one tenant (§2.1).

| Binary | Placement | Notes |
|---|---|---|
| `messgr-ingest` | internal | write path, horizontally scalable; one pool per tenant database |
| `messgr-dispatcher` | internal | one active + one standby **per tenant** (§9) |
| `messgr-query` | internal | read replica only; serves tenant UI and read API |
| `messgr-webhook` | DMZ | signature verification, minimal surface; routes by opaque per-tenant token (§10) |
| `messgr-control` | internal | control database, provisioning, platform console (§11.4). Cloud only in practice; runs with one row on-prem |
| `messgr-otp` | internal / DMZ | synchronous OTP endpoint (§3.1). Always present in cloud. On-prem it is needed only if the auth service is not Rust and so cannot link `sms-sender` directly |

Supporting infrastructure, **per region**:

| Component | Topology | Notes |
|---|---|---|
| Postgres | primary + streaming replica | one database per tenant plus `control` (§2.1); replica serves all reads |
| Vault | 3-node Raft cluster | one Transit mount per tenant (§7.6), KV for provider credentials, PKI for internal mTLS. Shamir unseal, no auto-unseal |
| PgBouncer | transaction mode | mandatory for request-path services. Dispatchers bypass it entirely and connect direct (§2.3) |

Run under systemd or Docker Compose. Kubernetes only if the operator already runs it for other workloads — do not introduce it for this system alone. Configuration via environment variables and a TOML file; **all secrets come from Vault** — none in config files, none in environment variables beyond the Vault address and AppRole RoleID.

**Migrations run N times.** `messgr-migrate` iterates the tenant registry, applies `sqlx migrate` to each database, and records the result in `tenant_schema_version`. Two rules make this safe: migrations are **expand/contract** so any version is compatible with the binaries of the version before it, and a partial run leaves the fleet in a visible mixed state rather than a broken one. Never automatic on process start — with twenty databases, an accidental migration on rollout is twenty accidents.

Three nodes for Vault rather than one is not gold-plating: with manual Shamir unseal, a single-node Vault means every restart is a full outage requiring three keyholders. Raft integrated storage — not the Consul backend, which would add a component for no benefit at this scale.

**Restoring a single tenant is a full-cluster operation.** Postgres point-in-time recovery works at cluster granularity — WAL is shared across every database — so there is no way to roll one tenant's database back while the others stay current. The actual procedure:

1. Recover the whole cluster to the target timestamp on a **separate recovery instance**.
2. `pg_dump` the affected tenant's database from it.
3. Load into production, either alongside the live database or replacing it.

This is still far cleaner than extracting one tenant's rows from shared partitioned tables, and it remains a real argument for database-per-tenant — but it is not the one-command operation that "per-tenant PITR" suggests. Three things it requires: a written and **rehearsed** runbook, standing spare capacity (or a rapid provisioning path) for the recovery instance, and an RTO quoted to tenants that reflects a full-cluster restore rather than a single-database one.

**Note on the stack:** Rust is well-suited to the dispatcher's concurrency profile. The friction to plan for is integration surface rather than the language itself — SAML/OIDC, on-prem SMTP or Exchange, and bank middleware clients all have thinner Rust ecosystems than JVM or .NET. Budget time for those adapters, or front them with a small existing service where a mature client already exists.

---

## 14. Build order

0. Vault cluster: 3-node Raft, Transit engine, AppRole auth, unseal runbook with named keyholders. Blocks step 1. *Outcome: a key-management substrate to build on — nothing sends anything yet, but the first row written in step 2 can be encrypted under a real KEK.*
0b. Control database, tenant registry, `producer_cert` mapping, and the single-command provisioning script (§4.11, §11.4). Even for the on-prem launch customer — provisioning that one tenant through the same path is what keeps on-prem and cloud from diverging. `tenant_id` is present from the first migration; retrofitting a tenant column across a partitioned ledger later is a rewrite. *Outcome: a tenant can be provisioned by one command, including on-prem's single row.*
1. Producer registry + mTLS identity (§4.9). First, because `producer_id` is on the ledger from row one and every later control keys on it. *Outcome: a client certificate resolves to exactly one `(tenant_id, producer_id)`, rejected at the edge if unregistered or disabled — still no messages.*
2. Ledger and outbox schema, ingest API, one channel (SMS), no gates. **Envelope encryption via Vault Transit included from the first write** — see note below. Prove the queue mechanics end to end. *Outcome: the first true end-to-end send — a producer can `POST /comms` and an SMS leaves the system, encrypted, idempotent, auditable — deliberately with no gates yet, so this is a system proven correct, not yet safe.*
3. Customer projection + resolution at ingest (§4.6–4.8), provisional shells, alias expansion. Event feed consumer can be stubbed initially; the schema and resolution path cannot. *Outcome: messages hang off a `customer_id`, minted provisionally when unresolvable, so a per-customer timeline becomes answerable.*
4. Kill switches (§5.2). Early and deliberately — this is the control you want in place *before* the system can send at volume, not after the first incident proves you need it. *Outcome: an operator can stop traffic by any of five scopes within seconds, audited, before this step only killing processes would do it.*
5. Verification, consent, and suppression gates. Regulatory blockers, not enhancements. *Outcome: the system becomes lawful to send marketing from — the first point where a single message's decision trail is regulator-presentable.*
6. Producer quotas + `producer_usage` flush (§5.1). *Outcome: one misbehaving producer can no longer burn the messaging budget or the provider relationship, without ever being able to withhold transactional or auth traffic.*
7. Dispatcher HA, circuit breakers, retry and backoff. *Outcome: sending survives a node dying and a provider misbehaving. Steps 1–7 are the minimum viable system — from here it is production-shaped, and everything past step 11 can ship incrementally.*
8. Quiet hours with jitter and DST handling. *Outcome: no marketing at 3am, correct across DST and unknown timezones, auth exempt by construction.*
9. Scheduled delivery, cancellation, and `expires_at` (§6.2). *Outcome: producers can send later and change their mind, with expiry/cancellation/quiet-hours precedence defined rather than emergent (§6.3).*
10. Customer event feed consumer, replacing the step-3 stub. Nightly reconciliation job. *Outcome: the projection stays true without ingest doing the work, and drift from source is visible rather than silent.*
11. Remaining channels (email, WhatsApp) behind the same `Sender` trait. *Outcome: every channel under one API, one ledger, one gate chain — the headline claim becomes literally true.*
12. Webhook receiver and delivery-receipt ingestion. *Outcome: message status becomes `delivered`/`bounced`, not just `accepted by the provider`, and bounces/complaints feed suppression automatically.*
13. Query API and UI, with `AuthProvider` trait + `MockProvider` + the production-guard check (§11.1). Real OIDC wiring lands whenever the IdP is available; nothing else blocks on it. *Outcome: the ledger becomes usable by people who are not DBAs, role-scoped, with every compliance access logged.*
14. Admin panel: quota dashboard, kill-switch console, scheduled queue (§11.3). *Outcome: the system is operable without `psql` — the step-4 runbook becomes the fallback, not the primary interface.*
15. Erasure tooling: crypto-shred first, then physical redaction and the erasure audit log. *Outcome: an erasure request can be executed and evidenced, and a new PII table can no longer silently escape the erasure surface (§7.2's CI check).*
16. SMS provider failover (§12.1) — before step 17, not after. *Outcome: a single SMS provider outage stops being an OTP-availability incident.*
17. OTP fast path. Last, because it touches the auth flow and should not be the thing shaking out bugs in the sender adapters. *Outcome: customer login stops depending on the messaging platform at all — a marketing incident, a full outbox, or a dead dispatcher can no longer lock a customer out of the bank.*
18. Bulk campaign path and ingest admission rate limiting. *Outcome: campaign-scale sends without degrading the API for everyone else, and submission volume is bounded separately from send volume.*
19. **Cloud enablement**, once the on-prem customer is live: `messgr-otp` (§3.1), platform console (§11.4), platform kill switches, offboarding modes (§7.7), second region. Everything before this point already runs multi-tenant at N=1, so this step adds surfaces rather than reworking the core. *Outcome: messgr becomes a hosted multi-tenant product from the same codebase, with a second region proving the region-boundary assertions hold rather than being assumed.*

**Isolation must be tested, not asserted.** A suite that runs against a single tenant proves nothing about the property the whole design rests on. From step 0b onward, CI runs a **two-tenant** integration suite asserting at minimum:

- Work performed in tenant A's context never reads or writes a row in tenant B's database.
- The `current_database()` assertion fires when a pool is deliberately mis-wired to the wrong tenant, once at pool creation (§2.1 — checkout-time re-checking cannot observe anything creation-time didn't). This is now the whole isolation mechanism, so the test must prove `expected_database` is derived independently of the pool's own construction — a mutation test that deliberately re-derives both from the same input must turn this test red.
- Dispatcher leader election holds under a forced failover, over a direct connection, with exactly one active dispatcher observed throughout (§2.3).
- Every table containing customer data either appears in the erasure statements of §7.2 or is on a **named, reasoned exemption list checked in alongside the erasure code** (currently: `suppression`, §7.2). Checked against the live schema, not a hand-maintained list of tables to remember — a bare allowlist with no reasons is indistinguishable from the `comms_event` omission this check exists to catch; the reason is what a reviewer actually checks against.

The leader-election test matters because the failure it guards against — two active dispatchers double-sending — is invisible in a single-node test and only appears under pooling.

Steps 1–7 are the minimum viable system. Everything after 11 can ship incrementally.

**Kill switches before volume.** Step 4 looks early for an operational feature, but a system that can send to millions of customers and cannot be stopped is a system you should not point at production. The database-level switch and a `psql` runbook are enough at step 4; the panel at step 14 makes it usable at 3am by someone who is not the author.

**Second sequencing constraint:** the customer projection lands at step 3, *before* the gates at step 5, because consent keys on `customer_address.id` (§5) and the resolution path decides what `address_id` a message carries. Building gates against a `customer_id` key first and re-keying later would mean rewriting live consent records — exactly the data you least want to migrate.

**Sequencing constraint:** payload encryption and per-customer DEKs must be in place from step 2. Crypto-shredding only works if every payload was written under a customer-scoped key; retrofitting encryption later means a full re-encryption pass over the entire ledger, and any records written before the retrofit can never be crypto-shredded — only physically redacted, with the §7.3 backup window attached. The erasure *tooling* can wait until step 15; the *key discipline* cannot wait at all.

---

## Decisions taken

| # | Decision | Consequence |
|---|---|---|
| 1 | Crypto-shredding default, physical deletion available | Two erasure modes (§7). Cold partitions stay writable; erasure audit log added; backup-window limit documented. |
| 2 | OIDC, mocked initially | `AuthProvider` trait with hard production guard (§11.1). UI work unblocked from IdP availability. |
| 3 | Campaign queries biweekly | `(campaign_id, created_at)` index on the replica. No rollup table and no OLAP tier — both were considered and cut as unjustified at this query frequency (§11.2). |
| 4 | Single provider acceptable for now | Accepted for queued traffic; flagged as a tier-0 risk on the OTP path, with mitigations (§12.1). |
| 5 | Vault as root of trust, no HSM | Transit engine for DEK wrapping; Vault also owns provider credentials and internal PKI. Cost: Shamir manual unseal, 3 keyholders per restart — mitigated by 3-node Raft and DEK caching (§7.6). |
| 6 | Wrapped DEKs stored in Postgres | Vault stays off the read path. Erasure bounded by the DB backup window for both modes (§7.3); compliance reporting states the date rather than claiming instant deletion. Revisit only if a regulator challenges erasure timeliness. |
| 7 | Customer data is a projection only | Never system of record. It resolves identity only, never the destination — every class supplies `destination` explicitly on the request, so there is no gate needed against a stale, projection-resolved address (§4.8). |
| 8 | Fed by event feed | Nightly checksum reconciliation as safety net. Feed outage degrades resolution only — never the ledger, timeline, or OTP (§12). |
| 9 | Consent per contact point | Keyed on `customer_address.id`, so recycled numbers cannot inherit the previous owner's opt-in (§5). Requires the projection to land before the gates (§14). |
| 10 | Producers are registered, mTLS-identified | `producer_id` replaces free-text `source_system` on the ledger (§4.9). Quota and kill-switch scoping cannot rest on a caller-asserted string. |
| 11 | Quotas charged at dispatch, hard for marketing / soft for transactional / exempt for auth | Cost control never becomes an availability incident on tier-1 traffic (§5.1). Counters in-process behind the single per-tenant dispatcher, flushed to `producer_usage` for reporting. |
| 12 | Kill switches hold rather than discard, by default | Most incidents end in "resume". Release is drain-rate limited and `expires_at`-filtered so re-enabling does not stampede (§5.2). Auth is structurally exempt and the panel says so. |
| 13 | Scheduling reuses `outbox.next_attempt_at` | No scheduler process. All gates re-evaluated at dispatch, so a three-week-old scheduled message respects consent as it stands at send time (§6.2). Cancellation and `expires_at` are part of the feature, not follow-ups. |
| 14 | Database per tenant; **no RLS** | Chosen over shared-schema RLS: at ~20 large tenants the sharing saves little and costs per-tenant retention, clean offboarding, logical single-tenant recovery, and a defensible answer in bank security review. RLS was then specified *on top* and removed as over-engineering — with one database per tenant there is no tenant filter to forget, so a `current_database()` assertion catches the only bug it was guarding against, at a fraction of the cost (§2.1). `tenant_id` stays as a plain column. |
| 15 | Regions fully independent, no global control plane | Each region has its own control DB, Vault, Postgres, and binaries. Tenants pinned to one region with a regional endpoint. Avoids a cross-jurisdiction component and a cross-region availability dependency (§2.2). |
| 16 | One dispatcher per tenant, not per channel | Per-tenant provider accounts make every rate limit tenant-scoped, so one process per tenant is still a single enforcer — and blast radius becomes one tenant (§9). |
| 17 | Per-tenant Vault Transit mounts | Offboarding is destroying one key. Platform operators hold no key that decrypts tenant content — a materially stronger claim than access-control policy (§7.6). Uses mounts + policies, not Enterprise namespaces. |
| 18 | Tenants bring their own provider accounts | No shared provider budget to arbitrate, no messaging resale, no platform-level metering for billing. Producer quotas stay a tenant governance tool (§4.10). |
| 19 | One codebase, on-prem is N=1 | No compile-time tenancy flag, no second deployment path. The launch customer is provisioned through the same script cloud tenants will use (§14). |
| 20 | Split connection topology | Request-path services via PgBouncer transaction mode; dispatchers direct to Postgres. Session advisory locks and `LISTEN` do not survive transaction pooling, and the failure is silent double-dispatch (§2.3). Removing RLS removed the *correctness* reason for transaction mode but not this one — the split stands on its own. |
| 21 | Backup policy uniform per region | Per-tenant *message* retention is configurable; per-tenant *backup* retention is not, because WAL is cluster-wide. Conflicting requirements mean separate clusters, decided at onboarding (§7.3). |
| 22 | Multi-jurisdiction tenants do not merge | One tenant per region, timelines do not span. Stated product limitation with a compliance rationale, not an implementation gap (§2.2). |
| 23 | `final_status` on the ledger — the one permitted mutation | Gate-chain terminal outcomes need somewhere queryable to live; deriving them from `comms_event` would aggregate over a 7-year partitioned table per query. One `UPDATE` per row, always within the outbox lifetime, so only hot partitions are ever rewritten (§4.1). |
| 24 | Provider payloads encrypted, `customer_id` denormalized onto `comms_event` | Delivery receipts echo the recipient address in third-party JSON. Left plain it sat outside both erasure modes for 7 years (§4.4, §7.2). |
| 25 | End-to-end send walkthrough documented (§2.4) | Not itself an architecture decision — a canonical step-by-step trace through ingest, gate chain, dispatch, and receipt, cross-referencing §§2–11 so the request path stops being reassembled by hand from scattered subsections. Keep it in sync whenever a cited mechanic (claim query, gate order, write atomicity) changes. |
| 26 | `tenant_id` lives on `comms_request` only, not on every tenant-database table | Matches §2.1's actual isolation mechanism (the database boundary), not a column. Corrects a preamble claim in §4 that had drifted from every table shown beneath it, and a `quiet_hours_policy` column that had drifted the other way (§4, §4.10). |
| 27 | Staleness gate cut | `POST /comms` has always required an explicit `destination`, on every class, so a gate guarding a projection-resolved address guarded a path the API cannot take. Removed from the gate chain, `tenant_config`, and Still-open (§4.8, §5). |
| 28 | Kill-switch "held" state is checked before claim, not after | The obvious alternative — claim, get gate-blocked, lease expires, re-claim — is a busy-wait for the life of the switch. The dispatcher excludes matching rows from the claim query's candidate set while a switch is cached active (§5.2). |

## Still open

1. **Backup retention window** — sets `erasure_request.backups_clear_at` and therefore the date the bank can truthfully report physical erasure as complete. (§7.3)
2. **Quiet-hours policy content** — the actual windows per region, and the institution-wide default for customers with unknown timezone. (§6)
3. **OIDC group-to-role claim mapping** — needed only when real OIDC replaces the mock, but determines whether the four roles in §11.1 map cleanly onto existing directory groups.
4. **Provider selection** per channel, and whether the chosen SMS provider supports idempotency keys (affects duplicate rates under at-least-once delivery). (§12)
5. **Identifier systems** — which upstream systems (core banking CIF, CRM, digital, cards) will appear in `customer_external_id.system`, and which is canonical for the event feed. (§4.6)
6. **Verification semantics** — does the master system publish per-address verification state on the feed? Until answered, the launch tenant runs `verification_mode = 'observe'`, which is now explicit rather than an accidental always-pass (§5).
7. **Initial quota values** per producer, and the day-boundary timezone for the daily window. Needs the producer list and their expected volumes. (§5.1)
8. **Maximum scheduling horizon** — 90 days is the proposed default. Confirm, and decide who may hold an override. (§6.2)
9. **Two-person approval mechanism** for the auth kill switch — built into the panel, or an out-of-band process the panel merely records? (§5.2)
10. **Launch regions** and their jurisdictions — determines how many independent stacks, Vault clusters, and keyholder sets exist on day one. (§2.2)
11. **Vault edition** — confirm open-source with per-tenant mounts is acceptable, or whether an Enterprise licence is already held and namespaces are preferred. (§7.6)
12. **Cloud OTP posture** — will cloud tenants accept `otp-api` with its network hop, or should on-prem auth alongside cloud comms be the recommended pattern for tenants with strict auth SLAs? (§3.1)
13. **Tenant offboarding SLA** — how quickly must data become unreadable after termination? Key destruction is immediate; `DROP DATABASE` and backup expiry are not. Same §7.3 backup-window caveat applies per tenant. Also: is terminate-and-archive (§7.7) offered commercially, and at what price? It carries multi-year key-custody obligations after the relationship ends.
14. **Regional backup policy** — one window per region, so it must satisfy the strictest tenant on that cluster. Sets the erasure-completion date quoted to all of them (§7.3).
15. **Single-tenant restore RTO** — what recovery time can be quoted, given it requires a full-cluster restore to a side instance? Depends on cluster size and whether spare capacity stands by (§13). **Unresolved prerequisite: no per-tenant storage estimate exists anywhere in this document.** The ledger holds rendered bodies (§7) for seven years across a 50× volume range (100k–5M messages/day per tenant, §1), so an email-heavy tenant's `payload_ciphertext` dominates its size and therefore the cluster's. Size one tenant first; cluster size — and with it this RTO, the slow-storage migration point (§7.5), and how many tenants can reasonably share a cluster (§2.1) — all follow from that number.
