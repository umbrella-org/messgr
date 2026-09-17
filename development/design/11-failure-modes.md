# Failure modes and degraded operation (§12)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 12. Failure modes and degraded operation

Centralizing communications creates a single point of failure. The mitigations must be explicit:

| Failure | Behaviour |
|---|---|
| messgr fully down | OTP unaffected (§3). Transactional and marketing queue at the producer side; producers must treat `POST /comms` failures as retryable and buffer. |
| Postgres primary down | Ingestion fails (retryable). Dispatchers stall — which incidentally means nothing is being sent, so the urgency of a kill switch drops. UI degrades to read-only against the replica. Recovery is standard Postgres failover. |
| Primary degraded but sending continues, and a kill switch is needed | **The awkward case.** The admin panel writes switches to the primary, so a partially-failed primary is exactly when the control is hardest to reach. Mitigations: the kill-switch write path is a single tiny transaction on its own small connection pool, so it survives conditions that starve bulk traffic; and the runbook includes the direct `psql` statement to engage a switch, tested and kept alongside the on-call notes. Do not let the only path to stopping the system be a web form. |
| One provider down | Circuit breaker opens; that channel's queued messages back off and retry, draining when the provider recovers. Other channels unaffected. **Except OTP — see §12.1.** |
| Dispatcher crash | Leader election (T-039, §9): the crashed process's session ends, Postgres releases the advisory lock, and the standby acquires it and takes over within seconds, sweeping the tenant's stale leases (§4.2's correction) immediately after acquiring leadership, before claiming again. At-least-once delivery — see below. |
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

