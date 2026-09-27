# The gate chain (§5)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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
| Quiet hours | See §6. Auth class exempt. | Reschedule |
| Rate limit | Per-provider token bucket, in-process. | Defer, retry next tick |

Consent enforcement was the single most consequential omission in the first draft of this design: sending marketing to an opted-out customer is a regulatory penalty, not a bug. It is now a hard gate with its own terminal state, so suppressed sends are visible and auditable rather than silently dropped.

**Consent keys on `address_id`, not on `customer_id` or on the raw address value.** Keying on the person is wrong because a customer may reasonably opt out of marketing on one address and not another. Keying on the value is worse: a recycled phone number would silently inherit the previous owner's opt-in. Because `customer_address` rows are append-only and a recycled number produces a *new* row, consent on the interval id means the new owner starts with no consent record — and absence of consent defaults to opted-out for marketing. Correct behaviour falls out of the schema rather than needing a cleanup job.

Suppression deliberately keys on the raw `destination_hmac` instead. It is fail-safe (it blocks sending), so over-suppressing a recycled number is the acceptable direction to err; entries carry a review date rather than living forever.

**Automatic suppression from a delivery receipt defaults `review_at` to one year out (T-047).** A `bounced`/`complaint` webhook receipt upserts a suppression row on promotion, not just a manual `suppression add`. No shorter platform default existed anywhere in the codebase to inherit, and the choice is compliance-adjacent, so it was confirmed explicitly rather than assumed. The upsert only ever lengthens an existing entry's `review_at`, never shortens one — the same fail-safe direction as the rest of this gate — so a longer-standing manual entry (e.g. a `regulatory_hold`) is never weakened by a later automatic one.

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

**How the two tiers meet (T-058).** A platform switch reaches the dispatcher and ingest through the same in-process kill-switch cache as a tenant switch: each refresh re-reads the tenant's `kill_switch` table and the control database's `platform_kill_switch` rows for that tenant together, and treats an engaged platform switch as a `global`, `hold` switch. It therefore excludes and — the dangerous half again — *releases* exactly like one, through the same drain ramp; nothing about "release ramps" is specific to the tenant tier. Two consequences were settled while building it. First, a release drain must honour every switch that is still engaged, of either tier: the ramp for one released scope skipping rows another active switch holds is what "a platform switch overrides a tenant's, never the reverse" means once both are draining, and the tenant-tier drain had been getting this wrong on its own (a released `global` switch drained a still-held campaign). Second, a platform operator's "suspend" (§11.4) and a tenant-scope platform switch are not two mechanisms: suspension changes `tenant.status`, which stops ingest and the OTP paths but was never read by the dispatcher, so suspend also engages a tenant-scope switch to hold the queued backlog. A tenant-scope switch alone — without suspension — remains the tool for an incident that must not stop customer login, since no kill switch reaches auth (above).

---

