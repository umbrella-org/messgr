# PII, retention, and the erasure conflict (§7)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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

-- consent choices are the customer's own expressed preference, not just
-- routing metadata; erase them with the rest of the ledger (T-037)
DELETE FROM consent WHERE address_id IN (
    SELECT id FROM customer_address WHERE customer_id = $1
);

-- projection rows carry plaintext-equivalent PII too
UPDATE customer_address
SET value_ciphertext = '\x00'::bytea, value_hmac = '\x00'::bytea
WHERE customer_id = $1;
DELETE FROM customer_external_id WHERE customer_id = $1;
```

The `comms_event` statement was absent from an earlier version of this design, which meant physical redaction left the recipient address readable in provider payloads. Any new table holding third-party data must be added here at the same time it is created — the erasure surface is easy to grow without noticing.

**`consent` is added here by T-037, and it is not a named exemption like `suppression` below.** A consent choice is the customer's own expressed preference, not destination-scoped block-list data, so it is erased along with the rest of the ledger rather than surviving it. Its placement above, ahead of `customer_address`'s own `UPDATE`, is not load-bearing: neither statement touches `customer_address.customer_id` — physical redaction retains that column rather than zeroing it (see below) — so the `DELETE`'s subquery resolves correctly regardless of where it sits relative to the `UPDATE`. `tests/erasure_coverage.rs`'s `COVERED` constant carries `consent` alongside `comms_request`/`comms_event`/`customer_address`/`customer_external_id`; its detection query gained a third arm (a foreign key targeting `customer_address`) so it can see `consent` at all — `consent`'s only foreign key is `address_id REFERENCES customer_address(id)`, a hop the pre-T-037 query never followed.

**Named exemption: `suppression` is deliberately not touched by erasure, and this needs to be a stated decision rather than a gap someone finds later.** `suppression` holds `destination_hmac` with no `customer_id` column, so it cannot join into `WHERE customer_id = $1` at all — and that absence is structural, not an oversight: the table's entire purpose is to block future sends to a bad *destination*, regardless of which customer currently holds it (§5's suppression gate). A customer's erasure request must not silently un-suppress a hard-bounced or complained-about number for whoever is issued it next. So: a suppression entry outlives the customer that triggered it, by design, until its own review date (§5) retires it independently. The mechanical CI check in §14 that walks the schema for customer-linkable columns must carry this table as a named, reasoned exemption — not a silent absence from the erasure statements above, which is indistinguishable from the `comms_event` miss this section already recounts.

**Named exemption: `orphan_event` is a second table structurally unreachable by `WHERE customer_id = $1`, for a different reason than `suppression`'s.** `orphan_event` (§4.4, §10) holds a delivery-receipt webhook that arrived for a `provider_ref` no known `comms_request` currently claims — third-party data, including `provider_payload_raw`, kept **unencrypted** because there is no customer to scope a DEK to yet. It has no `customer_id` column at all, so it cannot be reached by physical redaction's `WHERE customer_id = $1` any more than `suppression` can. Unlike `suppression`, this is a temporary gap rather than a permanent design choice: once reconciliation resolves a row to a real `comms_request` (a separate ticket, T-030), it stops being an orphan and becomes an ordinary `comms_event` row, reachable the normal way. Until then, it is a named exemption like `suppression` — not a silent absence, and not a reason to block on T-030 landing first, since a schema with zero rows in most tenants for most of its life is not itself an erasure risk.

**Named exemption: `customer_dek` is exempted through crypto-shredding, not physical redaction.** Its primary key is `customer_id`, and it holds each customer's wrapped encryption key rather than customer data itself. Destroying `wrapped_dek` (setting `shredded_at`) is Mode 1's own erasure mechanism (§7.1); Mode 2's physical redaction does not also need to touch this table, since the ciphertext columns it would unlock are already overwritten by that same redaction pass.

**Named exemption: `customer_alias` holds no PII-bearing value to redact.** It carries only two opaque customer-id UUIDs and a merge timestamp — a customer id itself carries no personal information (see below), so there is nothing in this table for physical redaction to touch.

**Named exemption: `outbox`'s `customer_id` column is routing/claim metadata, not message content.** The recipient address and rendered body live in `comms_request`, already covered above; `outbox` only tracks which dispatcher claimed a row and when, so redacting `comms_request` already removes the customer-linkable content this table's `customer_id` merely points at.

**Named exemption: `customer` itself holds no personal information, only operational preference and sync metadata.** `locale` and `timezone` select a template locale and resolve quiet-hours/scheduling (§4.4, §6) — operational preferences, not personal data. `source_system` and `source_updated_at` are sync bookkeeping for the event-feed consumer (§4.6), not customer-supplied content. Unlike the other five exemptions, `customer` has no `customer_id`/`*_ciphertext`/`*_hmac`/`*_raw` column of its own to be caught by — it is the table those columns *reference* — so the mechanical CI check in §14 must resolve it via the `customer_id` foreign keys other tables declare against it, not by inspecting its own columns.

**Named exemption: `webhook_receipt_staging` holds a raw webhook receipt with no `customer_id` column yet.** The next webhook-promote run either resolves one — encrypting the payload into `comms_event`, already covered above — or hands the row to `orphan_event`, already its own named exemption above (§4.4, T-047).

**Named exemption: `access_audit` is evidence of what a compliance user did, not the customer's own data.** It records every `compliance`-role `query-api` access (§11.1) — actor, role, route, and the `customer_id` a search named, when it named one — the same reasoning `suppression`/`orphan_event` above already give for surviving erasure: a bank's audit trail is expected to outlive the record it describes, not be erased alongside it (T-048 decision 8).

These eight tables — `suppression`, `orphan_event`, `customer_dek`, `customer_alias`, `outbox`, `customer`, `webhook_receipt_staging`, and `access_audit` — are the complete named-exemption list the mechanical CI check in §14 must carry alongside the covered-table statements above.

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

**In GB, not months (T-019, estimate — see §2.1):** 18 months of accumulation is the amount of data sitting on the fast tablespace at any time before the oldest partitions start moving to slow storage. Using §2.1's per-tenant growth estimates, that is roughly **~170 GB** for a 100k/day tenant, **~1.7 TB** for a 1M/day tenant, and **~8.6 TB** for a 5M/day tenant — the last of which is, by itself, nearly the entire ~10 TB per-cluster planning ceiling §2.1 uses, before any cold-tier data is even counted. Ops capacity planning for the fast tablespace should size against this figure per tenant tier, not against the flat "18 months."

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

