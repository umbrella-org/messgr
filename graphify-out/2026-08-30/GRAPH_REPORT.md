# Graph Report - messgr  (2026-08-30)

## Corpus Check
- 52 files · ~81,676 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 599 nodes · 953 edges · 33 communities (32 shown, 1 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 42 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `c0abb41f`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- AGENTS.md
- Releasing messgr
- messgr — Design
- producer.rs
- KeyStoreError
- register.rs
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
- Tasks
- record
- messgr
- Profile
- control.rs
- cert_repo.rs

## God Nodes (most connected - your core abstractions)
1. `Profile` - 25 edges
2. `PLAN.md — provisional ticket list for building DESIGN.md` - 24 edges
3. `register_producer()` - 22 edges
4. `provision_tenant()` - 20 edges
5. `KeyStoreError` - 18 edges
6. `messgr — Design` - 17 edges
7. `VaultKeyStore` - 15 edges
8. `disable_producer()` - 15 edges
9. `list_producers()` - 15 edges
10. `provision_test_tenant()` - 15 edges

## Surprising Connections (you probably didn't know these)
- `disable_keeps_the_cert_mapping_and_is_idempotent()` --calls--> `disable_producer()`  [INFERRED]
  tests/producer.rs → src/producer/register.rs
- `idempotent_reregistration_does_not_silently_reenable_a_disabled_producer()` --calls--> `disable_producer()`  [INFERRED]
  tests/producer.rs → src/producer/register.rs
- `register_and_disable_against_an_unknown_tenant_slug_are_rejected_and_audited()` --calls--> `disable_producer()`  [INFERRED]
  tests/producer.rs → src/producer/register.rs
- `resolve_producer_distinguishes_disabled_from_unknown()` --calls--> `disable_producer()`  [INFERRED]
  tests/producer.rs → src/producer/register.rs
- `cert_subject_already_bound_to_a_different_tenant_is_rejected()` --calls--> `list_producers()`  [INFERRED]
  tests/producer.rs → src/producer/register.rs

## Import Cycles
- None detected.

## Communities (33 total, 1 thin omitted)

### Community 0 - "AGENTS.md"
Cohesion: 0.05
Nodes (34): Board rule, Brine (start here), Corrections on the record, Hard invariants, Project configuration, Reading order, Response style, What this project is (+26 more)

### Community 1 - "Releasing messgr"
Cohesion: 0.25
Nodes (6): Changelog, [Unreleased], Cutting a release, One-time setup this depends on — NOT YET CONFIRMED, Releasing messgr, Validating locally (no publish)

### Community 2 - "messgr — Design"
Cohesion: 0.04
Nodes (49): 10. Delivery receipts, 11.1 Authentication and authorization, 11.2 Query patterns, 11.3 Admin panel, 11.4 Platform console, 11. Query API and UI, 12.1 Single-provider risk — accepted, with a caveat, 12. Failure modes and degraded operation (+41 more)

### Community 3 - "producer.rs"
Cohesion: 0.17
Nodes (34): register_producer(), ResolutionError, resolve_producer(), ResolvedIdentity, Display, Error, Formatter, From (+26 more)

### Community 4 - "KeyStoreError"
Cohesion: 0.10
Nodes (27): Send, assert_tls_outside_dev(), connect_client(), Dek, guard_allows_https_address_outside_dev(), guard_allows_non_https_address_in_dev(), guard_panics_for_non_https_address_outside_dev(), KeyStore (+19 more)

### Community 5 - "register.rs"
Cohesion: 0.15
Nodes (25): main(), audit(), disable_producer(), disable_producer_inner(), DisableOutcome, list_producers(), ProducerError, register_producer_inner() (+17 more)

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
Cohesion: 0.18
Nodes (14): create_dek_and_unwrap_dek_round_trip(), create_dek_returns_a_distinct_key_each_call(), store(), unwrap_dek_rejects_a_ciphertext_from_a_different_mount(), control_database_url(), drop_test_tenant(), idempotent_reprovision_does_not_mint_a_second_secret_id(), login_as_tenant() (+6 more)

### Community 15 - "db.rs"
Cohesion: 0.32
Nodes (12): PgConnection, PgConnectOptions, assert_current_database(), assert_current_database_panics_on_the_mismatch_before_acquire_would_catch(), connect(), connect_with_expected_database(), Error, PgPool (+4 more)

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

### Community 22 - "Tasks"
Cohesion: 0.09
Nodes (21): 0. Feature branch (mandatory), Acceptance test, Confirmed design decisions (do not deviate without asking), Description, Docs update (mandatory when user-facing), Finish (mandatory), History, Implementation Plan (+13 more)

### Community 23 - "record"
Cohesion: 0.36
Nodes (7): record(), Error, Option, PgPool, Result, Uuid, Value

### Community 30 - "Profile"
Cohesion: 0.23
Nodes (14): GenerateCertificateResponse, assert_dev_profile(), bootstrap(), ensure_pki_mount(), guard_allows_dev(), guard_panics_outside_dev(), has_issuer(), issue_cert() (+6 more)

### Community 31 - "control.rs"
Cohesion: 0.26
Nodes (9): PathBuf, Cli, Command, DevPkiCommand, ProducerCommand, String, Config, Self (+1 more)

### Community 36 - "cert_repo.rs"
Cohesion: 0.36
Nodes (11): delete_producer_cert(), find_producer_cert(), ProducerCert, Error, Option, PgPool, Result, String (+3 more)

## Knowledge Gaps
- **253 isolated node(s):** `messgr`, `When to use`, `Install & register`, `Project configuration (in `pickle.toml` + the `AGENTS.md` marker block)`, `The rules (summary — full text in `resources/tickets-README.md`)` (+248 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Profile` connect `Profile` to `producer.rs`, `KeyStoreError`, `register.rs`, `provision_tenant`, `tenant_vault.rs`, `db.rs`, `control.rs`?**
  _High betweenness centrality (0.062) - this node is a cross-community bridge._
- **Why does `KeyStoreError` connect `KeyStoreError` to `vault.rs`, `Profile`, `provision_tenant`?**
  _High betweenness centrality (0.029) - this node is a cross-community bridge._
- **Why does `provision_tenant()` connect `provision_tenant` to `producer.rs`, `register.rs`, `Profile`, `tenant_vault.rs`?**
  _High betweenness centrality (0.028) - this node is a cross-community bridge._
- **Are the 13 inferred relationships involving `register_producer()` (e.g. with `main()` and `connect_tenant_pool()`) actually correct?**
  _`register_producer()` has 13 INFERRED edges - model-reasoned connections that need verification._
- **Are the 11 inferred relationships involving `provision_tenant()` (e.g. with `main()` and `connect_tenant_pool()`) actually correct?**
  _`provision_tenant()` has 11 INFERRED edges - model-reasoned connections that need verification._
- **What connects `messgr`, `When to use`, `Install & register` to the rest of the system?**
  _253 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `AGENTS.md` be split into smaller, more focused modules?**
  _Cohesion score 0.04878048780487805 - nodes in this community are weakly interconnected._