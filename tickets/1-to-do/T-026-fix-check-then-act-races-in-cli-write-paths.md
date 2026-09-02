---
id: T-026
title: Fix check-then-act races in CLI write paths
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-026 — Fix check-then-act races in CLI write paths

## Outcome

Four known check-then-act races in single-operator CLI write paths are closed: a rejected template approval always writes its `platform_audit` row even under concurrent callers, and the equivalent races in producer registration, tenant-config set, and dev-PKI root generation are fixed the same way. The tolerance that let these stand as `noted` — "single-operator CLI, no concurrent-caller expectation" — is recorded as expired now that the admin panel and platform console (build steps 14, 19) will remove that assumption.

## Description

Three reviews independently found the same shape of bug and each time judged it not worth a dedicated ticket on its own — correctly, at the time, since messgr had no concurrent-caller story yet. Batched together, four instances of the same pattern across three tickets pass the promotion test:

1. **T-010/F1** — `approve_template_inner`'s check-then-insert (`repo::find` then, only if `None`, `repo::insert`) is not atomic. Two concurrent `template approve` calls for the same `(template_id, version, locale)` can both pass the existence check; the loser's `INSERT` then fails on the primary-key constraint as a raw `ApproveError::Database`, skipping the `rejected`-audit branch entirely — contradicting decision 6's "every approve call, including a rejected one, writes exactly one row" (`src/template/approve.rs`).
2. **Same shape, `producer::register::register_producer_inner`** (`find_by_name` then `insert`) — named in T-010's own review as an existing instance, not previously flagged.
3. **Same shape, `tenant_config::configure::set_tenant_config_inner`** — also named in T-010's review as a pre-existing instance.
4. **T-006/F3** — the dev-PKI root-CA-generation race (`has_issuer` check, then `cert::ca::generate`) is not mitigated the way the equivalent mount-enable race is; two concurrent `bootstrap` callers that both observe no issuer yet can each generate a root, since Vault's PKI engine allows multiple issuers per mount with no "already exists" error to catch. `bootstrap`'s own doc comment claims a guarantee ("never mints a second root CA") that isn't held under concurrency (`src/producer/dev_pki.rs`).

Fix each with the pattern `ensure_pki_mount` (in the same file as #4) already uses: re-check after a failed/redundant write, or make the write itself idempotent (`ON CONFLICT DO NOTHING` plus a re-fetch, matching `src/customer/resolve.rs`'s own race-handling shape for provisional customers). #1's fix must specifically preserve the "always writes exactly one `platform_audit` row" guarantee on the losing side, not just avoid the panic.

Record explicitly in this ticket, and cross-reference from DESIGN.md if it isn't already: this tolerance was accepted under a single-operator assumption. The admin panel (step 14) and platform console (step 19) both introduce multiple concurrent human operators; re-open this class of finding if either surfaces a fifth instance.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: batches four check-then-act races noted across prior reviews (T-010/F1 and its two named sibling instances in producer registration and tenant-config set; T-006/F3's dev-PKI root race), each individually accepted under a single-operator-CLI tolerance this audit records as expiring once the admin panel/platform console land.
