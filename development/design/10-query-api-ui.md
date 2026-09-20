# Query API and UI (§11)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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
GET    /campaigns/{id}/reach     aggregate reach/delivery-status counts for a campaign

GET    /producers/{id}/usage     live quota consumption, current minute + day windows
GET    /producers/{id}/quota     configured limits and any active override
```

> **Correction (T-048):** `GET /campaigns/{id}/reach` was missing from this route list even
> though §11.2's "campaign reach summary" query pattern and §11.1's `campaign_ops` role both
> require one — found during T-048's refinement and added here.

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

