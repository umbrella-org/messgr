# Rate limiting and dispatcher topology (§9)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 9. Rate limiting and dispatcher topology

Provider rate limits need a single enforcement point. Multiple dispatcher processes have no shared view of the budget; a shared token bucket in Postgres means every send contends on one row, and static per-instance quota partitioning is wasteful and breaks when an instance dies.

**Chosen approach: one active dispatcher process per tenant, handling all of that tenant's channels.**

**Correction (2026-09-29, §2.5): for SMS, the provider rate limit is enforced by porth, not here.** porth paces each operator link to its contracted rate. The single enforcer in this process now enforces the handoff cap (§5), which keeps the backlog in the outbox, and the producer quota counters (§5.1). The per-tenant choice still stands on those two and on blast radius.

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

**Correction (2026-09-29): the circuit breakers were never built, and they are cut.** Build step 7 listed them, and T-021 shipped only per-message backoff with jitter. That backoff already gives what §12 asks of a down provider: back off, hold the outbox, drain on recovery. For SMS a breaker could not even see the failure it exists for. porth accepts and queues, so an operator outage never reaches the dispatcher as an error (§2.5). If a down email or WhatsApp provider ever makes per-message retries a measurable load, a breaker is the fix, added with that evidence.

**The remaining shared resource is the Postgres cluster itself, and this is not solved — only bounded.** Postgres has no per-database CPU or IO quota. `CONNECTION LIMIT` caps concurrency, not resource consumption: one tenant's 2M-row `COPY` on the primary, or a runaway analytical query on the replica, degrades every tenant on that cluster regardless of connection caps.

What is actually available: per-tenant `CONNECTION LIMIT`, per-tenant `statement_timeout`, and per-database IO monitoring to identify the culprit quickly. What closes it properly is the escape hatch — relocating a tenant to its own cluster is a `pg_dump` and restore rather than a redesign, which is a genuine benefit of the database boundary. Treat noisy-neighbour risk as a monitored operational condition with a known remediation, not as a solved problem.

---

