# PLAN.md — provisional ticket list for building DESIGN.md

Scratch artifact. Discard once the tickets are filed.

**What this is.** A rough decomposition of `DESIGN.md` into tickets, ordered by and
cross-referenced to the numbered **build order (§14)**. Every row cites the design sections
it implements. Sizes and splits are guesses — expect churn as tickets are refined and as
implementation reveals what actually belongs together.

**Id convention.** `T-001` (build step 0b) and `T-002` are both filed and done — `T-002` was
not part of this plan; it was spawned mid-flight from two non-blocking findings on `T-001`'s
review (`db::with_database_name` dropping connection-string options, `platform_audit` never
written), and consumed the id this plan had reserved for "Vault Transit integration" below.
Every id from that point on has been renumbered **+1** from the original draft to absorb the
shift (`T-002 Vault Transit integration` → `T-003`, and so on through the old `T-060` → `T-061`)
— the whole reason this file warns about churn. Ids below assume the tickets are filed in the
order listed, so `T-003` is the next one filed. If they are filed out of order — or another
unplanned ticket lands mid-sequence again, the way `T-002` did — the `depends-on` columns must
be re-mapped once more; they reference provisional ids, not fixed ones.

**Sequencing rules that constrain this list** (§14, restated so they are not lost in a
re-order):

- Envelope encryption and per-customer DEKs land **at step 2, from the first write**. Key
  discipline cannot be retrofitted; only the erasure *tooling* (step 15) can wait.
- The customer projection (step 3) lands **before** the gates (step 5), because consent keys
  on `customer_address.id`.
- Kill switches (step 4) land **before** the system can send at volume.
- Steps 1–7 are the minimum viable system. Everything after 11 can ship incrementally.
- From step 0b onward, CI runs a **two-tenant** integration suite. Each ticket below that
  touches tenant-scoped data extends it rather than assuming it.

Each build step below carries an **Outcome** line: what concretely exists and works once that
step's tickets are done, stated as capability rather than as code. It is there so the value of
stopping — or pausing — at any given step is legible without reading the ticket list.

WIP limit is 1 in development and 1 in review for `messgr`, so this is a queue, not a
parallel plan.

---

## Build step 0 — Vault

**Outcome.** A key-management substrate the rest of the system can be built on: a Vault cluster
with a per-tenant Transit mount and AppRole created by the provisioning command, a dev-mode
Vault in Compose, and a startup guard that refuses to run dev mode outside dev. Nothing sends
anything yet. What this buys is that the very first row written in step 2 can be encrypted
under a real KEK — the one thing in the design that cannot be retrofitted (§7, §14).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-003 | Vault Transit integration: `KeyStore` trait, Transit client, dev-mode Vault in compose with the non-dev startup guard | §7.6, §11.1 (guard pattern), §13 | T-001 | M |
| T-004 | Per-tenant Transit mount + AppRole creation wired into the provisioning command (fills the seam T-001 left) | §7.6, §11.4, §14 step 0b | T-003 | M |
| T-005 | Vault production topology: 3-node Raft, Shamir unseal runbook with named keyholders, AppRole/SecretID delivery | §7.6, §13 | T-003 | M (ops/doc; needed before go-live, not before code) |

Note: `KeyStore` must keep `wrapped_dek` opaque — the deferred "wrapped DEKs in Vault KV"
migration (§7.6) only stays available if the column never leaks into queries or the API.

## Build step 1 — Producer registry and mTLS identity

**Outcome.** The system knows who is calling it. A client certificate resolves to exactly one
`(tenant_id, producer_id)` pair, and an unregistered or disabled producer is rejected at the
edge before any handler runs. Still no messages. The value is that every ingest path built
afterwards inherits an identity that a caller cannot assert about itself (§4.9, §11.1).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-006 | `producer` table in the tenant database + repository, registration/disable operations (no UI yet) | §4.9 | T-001 | S |
| T-007 | mTLS identity resolution: client cert → `producer_cert` (control DB) → `(tenant_id, producer_id)`, as a shared layer; internal PKI issuance for dev | §4.9, §11.1, §2.2 | T-006 | M |

Producer identity is never read from the request body (§11, §4.9) — that rule is enforced
here and every later ingest ticket inherits it.

## Build step 2 — Ledger, outbox, ingest, one channel, encryption from the first write

**Outcome.** The first end-to-end send. A producer can `POST /comms` and an SMS actually
leaves the system: the request is recorded on the ledger in the same transaction as the outbox
row, payload and destination are encrypted under a per-customer DEK, a repeated idempotency
key replays instead of double-sending, a dispatcher claims and sends, and every state change
lands as a `comms_event`. Partitions roll forward and age out on their own. There are no gates
yet, so this is deliberately a system that always sends — the deliverable is proven queue
mechanics and an audit trail that is correct from the first row, not safety.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-008 | `tenant_config` table + typed config loading (retention, timezone/locale defaults, schedule horizon, verification mode, staleness bound, quota day boundary) | §4.10 | T-001 | S |
| T-009 | Per-customer DEK lifecycle: `customer_dek`, datakey creation, bounded zeroizing LRU cache, pre-provisioning batch, per-tenant HMAC pepper | §7.1, §7.6, §4.5 | T-003, T-004 | L |
| T-010 | Ledger + outbox schema: `comms_request` (monthly RANGE partitions), `outbox`, `comms_event`, `idempotency`, indexes as specified | §4.1–§4.4 | T-006, T-008 | L |
| T-011 | Template store: immutable `(template_id, version, locale)` rows, approval metadata, render path, version pinning onto the ledger row | §4.4 | T-010 | M |
| T-012 | `messgr-ingest`: `POST /comms`, idempotency replay, single-transaction ledger + outbox write, payload/destination encryption + HMAC | §4.1–§4.3, §7, §11 | T-007, T-009, T-010, T-011 | L |
| T-013 | `Sender` trait + first SMS provider adapter + `provider_config` (ordered list from day one, even at length 1) | §4.10, §11.1 (trait/mock pattern), §12.1 | T-008 | M |
| T-014 | Minimal dispatcher: per-channel claim loop with leases and `SKIP LOCKED`, `LISTEN`/`NOTIFY` wakeup with 1s poll fallback, `comms_event` write, single `final_status` update | §4.2, §4.1, §9 | T-012, T-013 | L |
| T-015 | Partition lifecycle: create-ahead job, 18-month move to slow (still writable) tablespace, detach + drop at the tenant's retention boundary | §4.1, §7.2, §7.5 | T-010 | M |

No gates in this step — the point is proving queue mechanics end to end (§14 step 2).

## Build step 3 — Customer projection and resolution

**Outcome.** Messages hang off a customer rather than off a bare phone number. Ingest can
resolve an external id, an alias, or a destination HMAC to a customer, minting a provisional
shell when it cannot, so the ledger carries `customer_id` on every row and a per-customer
timeline becomes answerable. Addresses are append-only intervals, which is the precondition
for consent keying on `customer_address.id` in step 5. The feed consumer is still a stub: the
projection is populated by resolution at ingest only (§4.6–§4.8).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-016 | Projection schema: `customer`, `customer_external_id`, `customer_address` (append-only intervals, encrypted value + keyed HMAC), `customer_alias` | §4.6, §7 | T-009, T-010 | L |
| T-017 | Resolution at ingest: alias expansion, external-id and HMAC lookup, provisional shell minting, and the hard rule that auth-class requests must carry an explicit destination | §4.7, §4.8 | T-012, T-016 | M |

Event-feed consumer stays stubbed here; only the schema and resolution path are
load-bearing at this step (§14 step 3).

## Build step 4 — Kill switches

**Outcome.** An operator can stop traffic. Five scopes, hold or discard, propagating to running
dispatchers within seconds, with a controlled drain-rate ramp on release and a tested `psql`
runbook for the case where the panel itself is unreachable. Every engage and release is
audited. This step also lands the gate-chain framework that every later gate plugs into. This
is the gate on sending at volume: before it, a bad campaign can only be stopped by killing
processes (§5.2, §14 step 4).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-018 | Gate-chain framework (ordered gates, terminal outcomes written to both `final_status` and `comms_event`) + `kill_switch` table, all five scopes, hold/discard, ingest rejection with a distinct error code, `NOTIFY kill_switch` on its own channel with 30s fallback re-read, full engage/release audit | §5, §5.2, §4.1, §4.9 | T-014 | L |
| T-019 | Release path: drain-rate ramp, `expires_at` filtering ahead of release, and the tested `psql` runbook for engaging a switch when the panel is unreachable | §5.2, §12 | T-018 | M |

## Build step 5 — Verification, consent, suppression

**Outcome.** The system is lawful to send marketing from. At dispatch time — not at submission —
marketing without an explicit opt-in on that address row is blocked, unverified addresses are
blocked or merely counted depending on the tenant's mode, and a suppressed destination is never
contacted. Each refusal is a terminal status plus an event with the reason. This is the first
point at which the decision trail behind any single message can be shown to a regulator.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-020 | Verification gate with per-tenant `enforce`/`observe` mode and an observable observe-mode count | §5, §4.10 | T-017, T-018 | S |
| T-021 | `consent` table keyed on `address_id` + consent gate (marketing requires explicit opt-in; transactional does not), source captured as evidence | §4.4, §5 | T-017, T-018 | M |
| T-022 | `suppression` table keyed on `destination_hmac` + gate, with review dates on entries; manual entry path until webhooks feed it (T-035) | §4.4, §5 | T-018 | S |

## Build step 6 — Producer quotas

**Outcome.** One misbehaving producer can no longer burn the messaging budget or the provider
relationship. Per-producer windowed limits charged at dispatch, hard on marketing and
soft/exempt elsewhere, with counters flushed to `producer_usage` so a dispatcher restart
rebuilds rather than resets them. Cost control that is structurally incapable of withholding
transactional or auth traffic (§5.1).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-023 | `producer_quota` + `producer_quota_override` (mandatory `valid_to`), in-process fixed-window counters charged **at dispatch**, hard/soft/exempt semantics per class | §5.1, §4.9 | T-018 | L |
| T-024 | `producer_usage` flush every few seconds, counter rebuild from it on dispatcher startup, minute/day row sweeps (7d / 1y) | §5.1, §4.9 | T-023 | M |

Non-negotiable: hard limits on marketing only. A quota must never withhold transactional or
auth traffic (§5.1).

## Build step 7 — Dispatcher HA and resilience

**Outcome.** Sending survives a node dying and a provider misbehaving. Exactly one active
dispatcher per tenant with a standby that takes over on failure — asserted by a forced-failover
test, because the failure mode of getting this wrong is silent double-dispatch — and per-provider
circuit breakers with backoff so an outage degrades instead of amplifying. **Steps 1–7 complete
the minimum viable system** (§14): from here it is a production-shaped service, and everything
after step 11 can ship incrementally.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-025 | Leader election: `pg_try_advisory_lock` over a **direct** (non-PgBouncer) connection, active/standby pair per tenant, plus the forced-failover test asserting exactly one active dispatcher throughout | §9, §2.3, §14 | T-014 | L |
| T-026 | Per-provider circuit breakers (open / half-open probe), exponential backoff with jitter, lease-expiry recovery semantics | §9, §12 | T-025 | M |

## Build step 8 — Quiet hours

**Outcome.** No marketing SMS at 3am. Sends are held against a policy resolved from the
customer's timezone, then their segment, then the institution default, released with uniform
jitter so the end of the window is not a thundering herd, and correct across DST transitions
and non-existent local times. Auth is exempt by construction.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-027 | `quiet_hours_policy`, resolution order (customer tz → segment → institution default), uniform jitter on window end, DST/non-existent-local-time handling via `chrono-tz`, auth exempt | §6.1, §4.10 | T-018, T-017 | M |

## Build step 9 — Scheduling, cancellation, expiry

**Outcome.** Producers can send later and change their mind. Future-dated delivery in absolute
or customer-local time within a bounded horizon, cancellation that is honoured right up to the
provider call and returns `409 Already Sent` after it, and an expiry check that kills a stale
message rather than delivering it late. The precedence between expiry, cancellation, quiet
hours and schedule is defined and tested, not emergent (§6.3).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-028 | Scheduled delivery: `scheduled_for` and `scheduled_local` (shared tz machinery with T-027), maximum horizon enforcement with producer override, outbox row-count metric | §6.2, §4.10 | T-027 | M |
| T-029 | Cancellation (`DELETE /comms/{id}`, pre-provider-call `cancelled_at` re-read, `409 Already Sent`), `expires_at` as the first gate, and the §6.3 precedence table | §6.2, §6.3, §5 | T-028 | M |

## Build step 10 — Customer event feed

**Outcome.** The projection stays true without ingest doing the work. Customer and address
changes — including merges — arrive from the bank's feed and are applied, and a nightly
checksum job tells you when the projection has drifted from source rather than letting it rot
quietly. Note what this does *not* do: it compares, it does not replay (§4.8).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-030 | Event-feed consumer replacing the step-3 stub: customer created/updated, address added/changed/verified/removed, merges via `customer_alias` | §4.8, §4.6, §4.7 | T-016 | L |
| T-031 | Nightly checksum reconciliation job (compare, do not replay) + staleness alerting | §4.8, §5 | T-030 | M |

## Build step 11 — Remaining channels

**Outcome.** All three channels — SMS, email, WhatsApp — behind one API, one ledger and one
gate chain. This is where the project's headline claim becomes literally true: every message the
bank sends a customer is in one place, under the same consent and kill-switch rules, regardless
of how it left the building.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-032 | Email channel behind the same `Sender` trait (SMTP/on-prem Exchange adapter) | §4.10, §13 | T-013, T-026 | M |
| T-033 | WhatsApp channel: adapter, session-window rules, its own opt-in state | §4.6, §4.10 | T-013, T-021 | M |

## Build step 12 — Delivery receipts

**Outcome.** You know what happened after handoff. Provider callbacks arrive at a DMZ binary,
are signature-verified and routed by an opaque per-tenant token, and become events on the
ledger, so a message's status is *delivered* or *bounced* rather than merely *accepted by the
provider*. Receipts that arrive before their message is visible are parked and reconciled
instead of lost, and hard bounces and complaints feed suppression automatically — closing the
loop left manual in step 5.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-034 | `messgr-webhook` (DMZ binary): provider signature verification, routing by opaque per-tenant token, `ON CONFLICT DO NOTHING` event insert, encrypted `provider_payload_ciphertext` with `customer_id` denormalized | §10, §4.4, §4.11, §13 | T-014, T-009 | L |
| T-035 | `orphan_event` table + short-delay reconciliation, and feeding hard bounces/complaints into `suppression` | §10, §4.4, §5 | T-034, T-022 | M |

## Build step 13 — Query API and UI

**Outcome.** The audit ledger becomes usable by people who are not DBAs. Authenticated,
role-scoped REST plus a UI: a customer's full timeline, a message's detail with its event
history, a campaign's reach. Compliance can bulk-export with every access logged; customer
service can look up one customer and provably cannot enumerate. This is the step that answers
"did we send it, when, and what happened to it" without anyone opening a shell.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-036 | `AuthProvider` trait, `MockProvider`, the fatal non-dev guard **in the same commit**, OIDC/PKCE implementation, role model (`customer_service`, `compliance`, `campaign_ops`, `comms_ops`, `admin`) enforced server-side | §11.1, §4.10 | T-008 | L |
| T-037 | `messgr-query`: read-replica-only wiring, REST endpoints per §11, published OpenAPI spec | §11, §11.2, §2.3 | T-036, T-010 | L |
| T-038 | UI (Askama + htmx, same binary): customer timeline, message detail with event history, campaign reach summary | §11, §11.2 | T-037 | L |
| T-039 | Compliance access-audit log + bulk export path; `customer_service` restriction tested (cannot list or enumerate) | §11.1 | T-037 | M |

Timeline queries never join the projection (§4.1, §11.2) — that is a test, not just a
convention.

## Build step 14 — Admin panel

**Outcome.** The system is operable without `psql`. Quota usage is visible, a kill switch can be
engaged from a console that shows the blast radius by class *before* the lever is pulled,
the scheduled queue can be inspected and cancelled from, and producers, templates, quiet-hours
policies, provider config and quota overrides are all editable — with every mutation audited
before/after. The step-4 runbook stops being the primary interface and becomes the fallback.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-040 | Admin panel shell (gated to `comms_ops`/`admin`) + quota dashboard from `producer_usage`, deliberately reading Postgres rather than the metrics stack | §11.3, §5.1 | T-038, T-024 | M |
| T-041 | Kill-switch console: engage/release by scope with mandatory reason, blast-radius count by class before engaging, permanent "auth is unaffected" notice, and the separate two-person-approval auth control | §11.3, §5.2 | T-040, T-019 | M |
| T-042 | Scheduled queue view: pending future-dated messages by producer/campaign/due window, with cancel actions | §11.3, §6.2 | T-040, T-029 | M |
| T-043 | Admin surfaces for producer registry, template approval, quiet-hours policy, provider config, quota overrides — every mutation audited with before/after | §11.3, §4.9, §4.10, §4.4 | T-040 | L |

## Build step 15 — Erasure tooling

**Outcome.** An erasure request can be executed, and evidenced. Crypto-shred renders a
customer's payloads unreadable immediately, a throttled background job physically redacts across
all partitions and vacuums before reporting done, and the compliance report states the date the
backups roll off instead of claiming deletion that has not happened (§7.3). The CI check is the
lasting part: a new table holding customer data cannot silently escape the erasure statements.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-044 | Crypto-shred: destroy DEK, `shredded_at` tombstone, `erasure_request` log with legal basis and `backups_clear_at`, compliance report wording that states the backup date rather than claiming instant deletion | §7.1, §7.3, §4.5 | T-009 | M |
| T-045 | Physical redaction: throttled background job across all partitions, mandatory `VACUUM` before reporting completion, `comms_event` and `customer_address` statements included | §7.2, §7.3 | T-044, T-034 | L |
| T-046 | CI check that every table holding customer data appears in §7.2's erasure statements, computed **against the live schema**, not a hand-maintained list | §7.2, §14 | T-045 | M |

T-046 is the invariant most easily broken by a later table (§7.2's `comms_event` miss). It
should fail loudly the moment someone adds an unlisted PII column.

## Build step 16 — SMS provider failover

**Outcome.** A single SMS provider going down stops being an outage. Ordered failover across
providers, config reloadable without a restart, a rehearsed manual-cutover runbook, and tighter
alerting on OTP failure rates. Deliberately sequenced *before* the OTP fast path: the fast path
is the traffic that most needs a second provider underneath it (§12.1).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-047 | Ordered multi-provider failover for SMS, hot-reloadable provider config (no restart), tested manual-cutover runbook, tighter OTP failure-rate alerting | §12.1, §4.10 | T-026 | L |

Explicitly before step 17, not after (§12.1).

## Build step 17 — OTP fast path

**Outcome.** Customers logging in stop depending on the messaging platform. OTP becomes a
synchronous library call with no queue, no gate chain and no shared process with the dispatcher,
governed by its own `auth_enabled` flag, with the audit record written best-effort and buffered
to local disk when Postgres is unavailable. Concretely: a marketing incident, a full outbox, or
a dead dispatcher can no longer lock customers out of the bank (§3).

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-048 | `sms-sender` library: synchronous provider call, no queue, no gate chain, plus the dedicated `auth_enabled` flag read from the control database on a short interval (fails closed only for that flag) | §3, §5.2, §13 | T-047 | L |
| T-049 | Best-effort audit write: async `comms_request` insert, local-disk buffer on Postgres unavailability, backfill job; auth payload never stored | §3, §7.4, §12 | T-048 | M |

Any proposal that routes OTP through the outbox is a regression (§3) — worth stating in the
ticket body so a later refinement does not "simplify" it.

## Build step 18 — Bulk campaigns and admission control

**Outcome.** Campaign-scale sends without degrading the API for everyone else. A campaign
arrives as an NDJSON stream, lands via `COPY` into staging and then one set-based insert into
ledger and outbox, with consent pre-filtered up front purely as an optimisation — the dispatch
gate stays authoritative. Separately, per-producer ingest rate limiting returns a retryable
`429`, protecting the platform from submission volume as distinct from send volume.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-050 | `POST /comms/bulk`: NDJSON stream → `COPY` into staging → single set-based insert into ledger + outbox, with the consent pre-filter (optimization only; the dispatch gate stays authoritative) | §8, §5 | T-012, T-021 | L |
| T-051 | Ingest admission rate limiting per producer (`429`, retryable) — distinct from send quota, and named as such everywhere | §5.1, §8, §12 | T-007, T-023 | M |

## Build step 19 — Cloud enablement

**Outcome.** messgr becomes a hosted multi-tenant product rather than one on-prem install —
from the same codebase, since on-prem is the N=1 case (§2.1). A regional synchronous OTP
endpoint, a platform console with tenant lifecycle and schema-drift visibility under its own
auth realm, a platform-wide kill switch that fans out to every tenant database, offboarding in
both the destroy and archive forms, and a second region that proves the region-boundary
assertions actually hold rather than being assumed.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-052 | `messgr-otp` binary: per-region synchronous endpoint, mTLS tenant auth, own process pool, independently deployable | §3.1, §13 | T-048 | L |
| T-053 | Platform console served by `messgr-control`: tenant lifecycle, schema-drift view, per-tenant health/volume, `platform_audit` trail — separate binary and auth realm from the tenant panel | §11.4, §4.11 | T-036, T-058 | L |
| T-054 | `platform_kill_switch`: platform/tenant scopes, fan-out to each tenant database, override semantics, non-actionable display in the tenant panel | §5.2, §4.11 | T-053, T-041 | M |
| T-055 | Tenant offboarding: terminate-and-destroy (Transit key destroy, `DROP DATABASE`, AppRole revoke) and terminate-and-archive (disabled surfaces, restricted export, mandatory end date) | §7.7, §4.11 | T-053, T-044 | M |
| T-056 | Second region: independent control DB, Vault cluster, Postgres, binaries; regional endpoint routing; boot-time assertion that a tenant's region matches its control DB | §2.2, §4.11, §13 | T-053 | L |

## Cross-cutting — not tied to one build step

**Outcome.** The difference between code that works and a service that can be run: migrations
applied across the fleet with drift reported, metrics and alerts on the things that actually
fail (outbox depth and age, gate outcomes, provider errors), deployable units for all six
binaries with secrets only from Vault, a rehearsed restore with a measured RTO, and one home for
the sweep jobs. None of it is a feature; without it the steps above are not operable.

| id | title | design refs | depends-on | size |
|---|---|---|---|---|
| T-057 | Fleet migration runner: iterate the tenant registry, apply migrations, record `tenant_schema_version`, report drift; never automatic on process start; expand/contract lint | §13, §4.11, §12 | T-001 | M |
| T-058 | Observability: Prometheus metrics (outbox depth and age, send rate, gate outcomes, provider errors, DEK cache hit rate), alerting rules, per-database IO monitoring for noisy-neighbour identification | §9, §11.3, §12 | T-014 | M |
| T-059 | Deployment packaging: systemd/Compose units for all six binaries, PgBouncer transaction-mode config with dispatchers bypassing it, TOML + env config, secrets only from Vault | §13, §2.3, §7.6 | T-025 | L |
| T-060 | Backup and restore: cluster backup policy, rehearsed single-tenant restore runbook (full-cluster recovery to a side instance, `pg_dump`, load), RTO measured and recorded | §13, §7.3, §12 | T-059 | M |
| T-061 | Sweep jobs: idempotency rows (30d), orphan events, `producer_usage` minute/day rows — one scheduled-maintenance home rather than three ad-hoc ones | §4.3, §4.9, §10 | T-024, T-035 | S |

---

## Deliberately not tickets

Recorded so they are not reintroduced by a well-meaning refinement:

- `campaign_stats` rollup and any OLAP tier — cut (§4.5, §11.2). Trigger to revisit: campaign
  queries exceeding ~30s, and only after deciding how late-arriving receipts reconcile.
- Row Level Security — cut (§2.1). The `current_database()` assertion in T-001 is the whole
  isolation mechanism.
- Marketing share-of-budget throttle — cut (§8). `ORDER BY priority` in the claim query is
  the entire preemption mechanism.
- Weighted quiet-hours jitter — cut (§6.1). Plain uniform jitter.
- A separate scheduler process or second queue — unnecessary; `next_attempt_at` is the
  mechanism (§6.2).
- Moving far-future scheduled rows to a separate `scheduled` table — build only if the
  outbox row-count metric says so (§6.2, metric shipped in T-028).
- Wrapped DEKs in Vault KV instead of Postgres — deferred hardening, not scheduled (§7.6).
  T-003/T-009 must keep the migration path open by keeping `wrapped_dek` opaque.

## Open questions that gate specific tickets

From "Still open" (§ at end of DESIGN.md). These need user/business answers, not design work:

| Open item | Blocks / shapes |
|---|---|
| 1, 15 Backup retention window, regional policy | T-044, T-060 |
| 2 Staleness threshold value | T-017, T-031 |
| 3 Quiet-hours windows + institution default | T-027 |
| 4 OIDC group→role mapping | T-036 |
| 5 Provider selection, idempotency-key support | T-013, T-047 |
| 6 Identifier systems, canonical feed source | T-016, T-030 |
| 7 Verification semantics on the feed | T-020 (defaults to `observe` until answered) |
| 8 Initial quota values, day-boundary tz | T-023 |
| 9 Scheduling horizon confirmation + override holder | T-028 |
| 10 Two-person approval mechanism | T-041 |
| 11 Launch regions | T-005, T-056 |
| 12 Vault edition | T-004 |
| 13 Cloud OTP posture | T-052 |
| 14 Offboarding SLA and archive pricing | T-055 |
| 16 Single-tenant restore RTO | T-060 |
