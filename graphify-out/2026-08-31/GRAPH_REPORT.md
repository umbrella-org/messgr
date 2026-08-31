# Graph Report - messgr  (2026-08-31)

## Corpus Check
- 60 files · ~84,604 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 604 nodes · 953 edges · 35 communities (31 shown, 4 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 42 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `43eb61bd`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- AGENTS.md
- Releasing messgr
- messgr — Design
- Tasks
- Profile
- producer.rs
- provision_tenant
- Tasks
- PLAN.md — provisional ticket list for building DESIGN.md
- Implementation Plan
- Tasks
- Implementation Plan
- Implementation Plan
- Implementation Plan
- tenant_vault.rs
- db.rs
- Producer
- Tenant
- The "validate ticket T-NNN" protocol
- Brine
- `tickets/` — the feature flow
- vault.rs
- resolve_producer
- record
- messgr
- producer/dev_pki.rs
- ClientError
- Result
- VaultClient
- cert_repo.rs

## God Nodes (most connected - your core abstractions)
1. `PLAN.md — provisional ticket list for building DESIGN.md` - 24 edges
2. `register_producer()` - 22 edges
3. `Profile` - 21 edges
4. `provision_tenant()` - 20 edges
5. `messgr — Design` - 17 edges
6. `KeyStoreError` - 15 edges
7. `VaultKeyStore` - 15 edges
8. `disable_producer()` - 15 edges
9. `list_producers()` - 15 edges
10. `provision_test_tenant()` - 15 edges

## Surprising Connections (you probably didn't know these)
- `idempotent_reprovision_does_not_mint_a_second_secret_id()` --calls--> `provision_tenant()`  [INFERRED]
  tests/tenant_vault.rs → src/tenant/provision.rs
- `tenant_a_vault_credentials_cannot_read_tenant_bs_dek()` --calls--> `provision_tenant()`  [INFERRED]
  tests/tenant_vault.rs → src/tenant/provision.rs
- `idempotent_reregistration_does_not_silently_reenable_a_disabled_producer()` --calls--> `resolve_producer()`  [INFERRED]
  tests/producer.rs → src/producer/resolve.rs
- `resolve_producer_distinguishes_disabled_from_unknown()` --calls--> `resolve_producer()`  [INFERRED]
  tests/producer.rs → src/producer/resolve.rs
- `resolve_producer_never_leaks_across_tenants()` --calls--> `resolve_producer()`  [INFERRED]
  tests/producer.rs → src/producer/resolve.rs

## Import Cycles
- None detected.

## Communities (35 total, 4 thin omitted)

### Community 0 - "AGENTS.md"
Cohesion: 0.05
Nodes (34): Board rule, Brine (start here), Corrections on the record, Hard invariants, Project configuration, Reading order, Response style, What this project is (+26 more)

### Community 1 - "Releasing messgr"
Cohesion: 0.25
Nodes (6): Changelog, [Unreleased], Cutting a release, One-time setup this depends on — NOT YET CONFIRMED, Releasing messgr, Validating locally (no publish)

### Community 2 - "messgr — Design"
Cohesion: 0.04
Nodes (49): 10. Delivery receipts, 11.1 Authentication and authorization, 11.2 Query patterns, 11.3 Admin panel, 11.4 Platform console, 11. Query API and UI, 12.1 Single-provider risk — accepted, with a caveat, 12. Failure modes and degraded operation (+41 more)

### Community 3 - "Tasks"
Cohesion: 0.09
Nodes (21): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+13 more)

### Community 4 - "Profile"
Cohesion: 0.05
Nodes (41): Send, Config, Self, String, assert_tls_outside_dev(), connect_client(), Dek, guard_allows_https_address_outside_dev() (+33 more)

### Community 5 - "producer.rs"
Cohesion: 0.16
Nodes (43): main(), audit(), disable_producer(), disable_producer_inner(), DisableOutcome, list_producers(), ProducerError, register_producer() (+35 more)

### Community 6 - "provision_tenant"
Cohesion: 0.15
Nodes (28): current_schema_version(), ensure_database_exists(), provision_tenant(), ProvisionError, ProvisionOutcome, Display, Error, Formatter (+20 more)

### Community 7 - "Tasks"
Cohesion: 0.06
Nodes (32): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Findings, Finish (mandatory), History (+24 more)

### Community 8 - "PLAN.md — provisional ticket list for building DESIGN.md"
Cohesion: 0.08
Nodes (24): Build step 0 — Vault, Build step 10 — Customer event feed, Build step 11 — Remaining channels, Build step 12 — Delivery receipts, Build step 13 — Query API and UI, Build step 14 — Admin panel, Build step 15 — Erasure tooling, Build step 16 — SMS provider failover (+16 more)

### Community 9 - "Implementation Plan"
Cohesion: 0.09
Nodes (22): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+14 more)

### Community 10 - "Tasks"
Cohesion: 0.09
Nodes (22): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+14 more)

### Community 11 - "Implementation Plan"
Cohesion: 0.10
Nodes (20): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+12 more)

### Community 12 - "Implementation Plan"
Cohesion: 0.10
Nodes (19): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+11 more)

### Community 13 - "Implementation Plan"
Cohesion: 0.11
Nodes (18): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), family: T-NNN             # optional single umbrella id (same child); groups pickup order on the board; NEVER gates; omit if none, Finish (mandatory), History (+10 more)

### Community 14 - "tenant_vault.rs"
Cohesion: 0.36
Nodes (10): control_database_url(), drop_test_tenant(), idempotent_reprovision_does_not_mint_a_second_secret_id(), login_as_tenant(), provisioning_creates_a_mount_scoped_to_exactly_this_tenant(), PgPool, String, VaultClient (+2 more)

### Community 15 - "db.rs"
Cohesion: 0.20
Nodes (18): PathBuf, PgConnection, PgConnectOptions, Cli, Command, DevPkiCommand, ProducerCommand, String (+10 more)

### Community 16 - "Producer"
Cohesion: 0.24
Nodes (16): Producer, DateTime, String, Utc, Uuid, find_by_cert_subject(), find_by_name(), insert() (+8 more)

### Community 17 - "Tenant"
Cohesion: 0.23
Nodes (16): DateTime, Option, String, Utc, Uuid, Tenant, find_by_slug(), insert_provisioning() (+8 more)

### Community 18 - "The "validate ticket T-NNN" protocol"
Cohesion: 0.13
Nodes (14): 1. Load context, 2. Implementation audit — *was the ticket fully implemented?*, 3. Quality audit — *was it done to industry best practice?*, 4. Consistency audit — *inconsistencies, contradictions, errors, ambiguities, redundancies, duplications*, 4a. Documentation audit — *coverage, whole-tree consistency, build health*, 4b. Docs-readability pass — *optional second opinion on changed prose*, 5. Classify and record findings — severity, then disposition, 6. Move the ticket (+6 more)

### Community 19 - "Brine"
Cohesion: 0.15
Nodes (12): Brine, Install & register, Notes, Procedure: audit the board, Procedure: implement a ticket, Procedure: make it a ticket, Procedure: refine a ticket, Procedure: rework a ticket (+4 more)

### Community 20 - "`tickets/` — the feature flow"
Cohesion: 0.17
Nodes (11): 0. Child-projects (the multi-project model), 1. Status is the directory — one source of truth, 2. Statuses, 3. IDs, priority, dependencies, and lineage, 4. The READY gate, 5. Findings — severity, then disposition, 6. The board — a generated index (agent rule), 7. Ticket structure (+3 more)

### Community 21 - "vault.rs"
Cohesion: 0.35
Nodes (10): ensure_transit_mount(), policy_hcl_for(), policy_hcl_names_exactly_the_two_keystore_paths(), provision_vault(), ClientError, Option, Result, String (+2 more)

### Community 22 - "resolve_producer"
Cohesion: 0.19
Nodes (12): ResolutionError, resolve_producer(), ResolvedIdentity, Display, Error, Formatter, From, Option (+4 more)

### Community 23 - "record"
Cohesion: 0.36
Nodes (7): record(), Error, Option, PgPool, Result, Uuid, Value

### Community 30 - "producer/dev_pki.rs"
Cohesion: 0.37
Nodes (13): ClientError, GenerateCertificateResponse, KeyStoreError, Profile, Result, assert_dev_profile(), bootstrap(), ensure_pki_mount() (+5 more)

### Community 36 - "cert_repo.rs"
Cohesion: 0.36
Nodes (11): delete_producer_cert(), find_producer_cert(), ProducerCert, Error, Option, PgPool, Result, String (+3 more)

## Knowledge Gaps
- **253 isolated node(s):** `Board rule`, `Corrections on the record`, `Hard invariants`, `Project configuration`, `Reading order` (+248 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **4 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Profile` connect `Profile` to `producer.rs`, `provision_tenant`, `db.rs`?**
  _High betweenness centrality (0.048) - this node is a cross-community bridge._
- **Why does `provision_tenant()` connect `provision_tenant` to `Profile`, `producer.rs`, `tenant_vault.rs`?**
  _High betweenness centrality (0.026) - this node is a cross-community bridge._
- **Why does `KeyStoreError` connect `Profile` to `vault.rs`, `provision_tenant`?**
  _High betweenness centrality (0.025) - this node is a cross-community bridge._
- **Are the 13 inferred relationships involving `register_producer()` (e.g. with `main()` and `connect_tenant_pool()`) actually correct?**
  _`register_producer()` has 13 INFERRED edges - model-reasoned connections that need verification._
- **Are the 11 inferred relationships involving `provision_tenant()` (e.g. with `main()` and `connect_tenant_pool()`) actually correct?**
  _`provision_tenant()` has 11 INFERRED edges - model-reasoned connections that need verification._
- **What connects `Board rule`, `Corrections on the record`, `Hard invariants` to the rest of the system?**
  _253 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `AGENTS.md` be split into smaller, more focused modules?**
  _Cohesion score 0.04878048780487805 - nodes in this community are weakly interconnected._