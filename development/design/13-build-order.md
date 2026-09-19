# Build order (§14)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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
- Every table containing customer data either appears in the erasure statements of §7.2 or is on a **named, reasoned exemption list checked in alongside the erasure code** (currently: `suppression`, `orphan_event`, `webhook_receipt_staging`, `customer_dek`, `customer_alias`, `outbox`, `customer`, §7.2). Checked against the live schema, not a hand-maintained list of tables to remember — a bare allowlist with no reasons is indistinguishable from the `comms_event` omission this check exists to catch; the reason is what a reviewer actually checks against.

The leader-election test matters because the failure it guards against — two active dispatchers double-sending — is invisible in a single-node test and only appears under pooling.

Steps 1–7 are the minimum viable system. Everything after 11 can ship incrementally.

**Kill switches before volume.** Step 4 looks early for an operational feature, but a system that can send to millions of customers and cannot be stopped is a system you should not point at production. The database-level switch and a `psql` runbook are enough at step 4; the panel at step 14 makes it usable at 3am by someone who is not the author.

**Second sequencing constraint:** the customer projection lands at step 3, *before* the gates at step 5, because consent keys on `customer_address.id` (§5) and the resolution path decides what `address_id` a message carries. Building gates against a `customer_id` key first and re-keying later would mean rewriting live consent records — exactly the data you least want to migrate.

**Sequencing constraint:** payload encryption and per-customer DEKs must be in place from step 2. Crypto-shredding only works if every payload was written under a customer-scoped key; retrofitting encryption later means a full re-encryption pass over the entire ledger, and any records written before the retrofit can never be crypto-shredded — only physically redacted, with the §7.3 backup window attached. The erasure *tooling* can wait until step 15; the *key discipline* cannot wait at all.

---

