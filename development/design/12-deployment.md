# Deployment (§13)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

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

**Single-tenant restore RTO (T-019), as a function of cluster size — planning estimates, not measurements; no runbook has been rehearsed yet.** Assume ~300 GB/hour for the full-cluster base-restore-plus-WAL-replay step, ~150 GB/hour for the subsequent single-tenant `pg_dump`/restore step (dump/restore is slower than raw block replay — indexes rebuild), and a fixed ~4-hour provisioning/runbook-execution overhead, applied to §2.1's sizing table:

| Cluster profile | Full-cluster restore | Tenant dump/restore | Total RTO |
|---|---|---|---|
| ~10 TB, small tenants packed (§2.1) | ~33 hours | ~5 hours (0.8 TB tenant) | **~1.8 days** |
| ~10 TB, one mid-size tenant (§2.1) | ~33 hours | ~53 hours (8 TB tenant) | **~3.8 days** |
| ~40 TB, one top-of-range tenant (§2.1) | ~133 hours | ~267 hours (40 TB tenant) | **~17 days** |

The top row is quotable today. The bottom row is not: at the top of the 100k–5M/day range, the dump/restore step alone — extracting a tenant whose own footprint *is* most of the cluster — costs over a week under these throughput assumptions, before any provisioning delay. This is a real gap, not a rounding error, and it should not be smoothed into a single "RTO" figure quoted to every tenant regardless of size. Closing it needs one of: parallelized dump/restore (`pg_dump -j`) validated against a rehearsed runbook, standing warm-spare recovery capacity sized for the largest tenant rather than the average one, or a retention/tiering conversation with top-of-range tenants about what RTO their volume actually permits. None of those is decided here — this ticket sizes the problem, it does not solve it.

**Note on the stack:** Rust is well-suited to the dispatcher's concurrency profile. The friction to plan for is integration surface rather than the language itself — SAML/OIDC, on-prem SMTP or Exchange, and bank middleware clients all have thinner Rust ecosystems than JVM or .NET. Budget time for those adapters, or front them with a small existing service where a mature client already exists.

---

