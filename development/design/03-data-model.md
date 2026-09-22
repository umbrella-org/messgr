# Data model (§4)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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

**Gap, found and resolved during T-052: `template_id`/`template_version` are `NOT NULL` on every row, but OTP never renders a template.** `payload_ciphertext` is NULL for the auth class (§7) and the code itself must never be templated or retained, so `sms-sender` has no template to pin. Resolved with a fixed sentinel row (`template_id = 'otp'`, `version = 1`, empty body, never rendered or read), approved once per tenant via the existing `messgr-control template approve` command (T-010) — an operational setup step, not a runtime code path.

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

Leases (rather than a `status = 'processing'` column) mean a crashed dispatcher's work becomes claimable again without a separate reaper job — see the correction above: the actual mechanism shipped by T-021 is a startup-time sweep, not a clock-based expiry. T-039 (leader election) moved that sweep to run once per acquired leadership rather than once at raw process start — still not a clock-based expiry, just re-anchored to the moment a fresh claimant is actually guaranteed sole.

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

Retained 30 days, swept nightly (`messgr-control idempotency-sweep run`, T-029). A retried POST returns the original `comms_request_id` with `200`, not a duplicate send.

**Correction: the key was a bare `PRIMARY KEY (key)`, scoped to nobody.** Idempotency keys are caller-supplied (§2.4 step 1). A bare `text PRIMARY KEY` means two different producers who happen to choose the same key — a sequential counter, a UUID library seeded the same way, a copy-pasted test value — collide on each other's rows: the second producer's request silently returns the *first* producer's `comms_request_id`. Idempotency is a per-producer contract, not a platform-wide namespace; the key is now `(producer_id, key)`, matching how quotas and kill switches already scope to the authenticated caller (§4.9).

### 4.4 Events, consent, templates

```sql
CREATE TABLE comms_event (              -- partitioned monthly, append-only
    comms_request_id  uuid        NOT NULL,
    customer_id       uuid        NOT NULL,  -- denormalized so erasure can find these rows
    occurred_at       timestamptz NOT NULL,
    event_type        text        NOT NULL,
      -- queued | sent | delivered | failed | bounced | read | complaint | expired
      -- | cancelled | suppressed_consent | suppressed_list | unverified_address | discarded
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
    added_at         timestamptz NOT NULL,
    review_at        timestamptz NOT NULL
);

**Correction: this snippet's `suppression` table was missing `review_at`.** `04-gate-chain.md`
and `06-pii-retention.md`'s prose both promise entries "carry a review date rather than living
forever" and "retire independently" once their own review date passes, but this table never had
the column. Fixed as part of T-038: `review_at` is mandatory and auto-expiring — an entry blocks
only while `review_at > now()`, so the gate query itself stops matching once it passes, with no
sweep job needed, and retiring an entry early is the identical `UPDATE` as letting it expire
naturally.

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

**Correction: this comment's `event_type` vocabulary was missing `discarded`.** `write_discarded`
(`src/dispatcher/drain.rs`, a kill-switch discard sweep) writes `event_type = 'discarded'` into
`comms_event`, and has since that code shipped; this comment never listed it, and neither did
migration `0004_ledger_outbox_schema.sql`'s own copy of the same comment. Caught by T-034's
review, which added code (`orphan_reconcile::reconcile::is_recognized_event_type`) that validates
`orphan_event.event_type` against exactly this list before promotion — a stale list there would
have been a silent compliance gap, not just a stale comment, even though `discarded` itself can
never legitimately arrive in a provider receipt. Added above; no schema change, no behaviour
change.

**Correction: `orphan_event` (§10) was named but never given a schema.** Added above. Its payload is deliberately **not** encrypted under a customer DEK, unlike `comms_event` — the whole reason a receipt lands here is that `comms_request_id` (and therefore `customer_id`) isn't known yet, so there is no DEK to encrypt under. This is a genuine, narrow exception to "PII is always written encrypted" (hard invariant 7's spirit, if not its letter, since no customer is yet identified to scope a key to), and it must stay narrow: reconciliation is a "short delay" per §10, and `reconcile_attempts` exists so a row that fails to reconcile past a small bound (config, not hardcoded) pages someone rather than accumulating as a permanent plaintext-PII table. A row that reconciles is deleted from `orphan_event` once re-inserted into `comms_event` proper, encrypted, under the now-known customer's DEK.

Templates are **immutable once approved**; changes create a new version. Every `comms_request` pins `template_version`, so "what exactly did we send this customer in 2021" remains answerable years later. Approval metadata is captured because a bank will need to show who signed off on customer-facing content.

**Render syntax (T-010): literal `{{key}}` placeholders**, substituted by plain string scanning against a caller-supplied variable map — no templating engine. Whitespace inside the braces is trimmed, so `{{ key }}` matches too. A key the body references but the caller doesn't supply is a hard render error, never sent as literal `{{key}}` text or blanked out — a bank must not send customer-facing content with an unsubstituted placeholder. A variable the caller supplies but the body never references is silently ignored.

### 4.5 Keys, erasure, and campaign rollup

```sql
CREATE TABLE customer_dek (
    customer_id    uuid PRIMARY KEY,
    wrapped_dek    text NOT NULL,       -- Vault Transit ciphertext, "vault:v1:..." (§7.6)
    created_at     timestamptz NOT NULL,
    shredded_at    timestamptz          -- set when key destroyed; row retained as tombstone;
                                         -- unread until crypto-shred ships (build step 15)
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
    kill_switch_release_rate int NOT NULL DEFAULT 500,   -- rows/second the release-drain ramp
                                                          -- admits after a switch releases (§5.2, T-016)
    reconcile_attempts_cap smallint NOT NULL DEFAULT 5,  -- orphan_event reconcile-attempts bound
                                                          -- before age-out deletes the row (§4.4/§10, T-033)
    oidc_issuer         text,                   -- the tenant's own IdP (§11.1) — not yet created,
                                                 -- added by a future OIDC ticket (not yet filed)
    oidc_client_id      text,                   -- not yet created, added by that same ticket
    oidc_group_claim    text,                   -- not yet created, added by that same ticket
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
`schedule_horizon_days`, `quota_day_boundary_tz`, and `verification_mode`; `kill_switch_release_rate`
(T-016) and `reconcile_attempts_cap` (T-033) were added later, each its own migration rather than
an edit to T-007's original one. `display_name` and the `oidc_*` columns are shown above as the
eventual design but are not yet migrated; nothing reads them yet (a future, not-yet-filed OIDC
ticket adds the `oidc_*` columns when real OIDC lands -- not a citable id yet, since one earlier
draft of this note named a specific ticket number for it before that number was claimed by
something else; do not cite a number here again until that ticket actually exists).

**Correction: T-007 also shipped `staleness_max_age`, and it is now dead in the shipped
schema, not just cut from the design above.** §4.8 explains why the gate it backed could never
fire — every request supplies its own `destination`, so nothing ever reads a projection-resolved
address closely enough to check its staleness. The column still exists in
`migrations/tenant/0002_tenant_config.sql`; dropping it is a schema migration, not a documentation
change, and is tracked as part of the ledger/queue schema remediation ticket rather than done here.
`quiet_hours_policy` is an unrelated table, created by T-043 with only its `scope = 'default'`
row ever read or written — see `05-send-timing.md`'s §6.1 correction note for why the `segment`/
`region` scopes it also carries are unreachable. `provider_config` ships in T-012, without a
`tenant_id` column — corrected here to
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
    webhook_token  text UNIQUE NOT NULL,        -- opaque; provider callback path (§10). Never the
                                                 -- slug; resolved by messgr-webhook (T-047)
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
                                                  -- cloud-only; unread until cloud enablement (step 19)
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

