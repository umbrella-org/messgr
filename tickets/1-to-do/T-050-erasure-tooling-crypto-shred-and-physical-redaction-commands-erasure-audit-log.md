---
id: T-050
title: Erasure tooling: crypto-shred and physical-redaction commands, erasure audit log
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: high
cost: XL
---

# T-050 — Erasure tooling: crypto-shred and physical-redaction commands, erasure audit log

## Outcome

After this ships, an erasure request can actually be executed and evidenced: crypto-shredding
(default) or physical redaction (on regulator/legal instruction) runs against a real customer,
and the compliance team gets a report stating the actual completion date rather than a claim of
instant deletion.

## Description

Build-order step 15 (§14), the last of the two "erasure *tooling* can wait until step 15" pieces
`13-build-order.md` calls out explicitly — the *key discipline* (per-customer DEKs from first
write) has been in place since step 2 (T-008); this ticket is the tooling that acts on it.
T-024 already shipped the mechanical CI check that every PII-holding table is covered by
erasure or a named exemption, and is explicit that it is "not a pull-forward of the full erasure
feature" — this ticket is that feature. Scope, per `06-pii-retention.md` §7:

- **`erasure_request` table** and an operator-facing command to file one.
- **Mode 1 — crypto-shredding (default, §7.1).** Set `customer_dek.shredded_at`; O(1), no
  partition rewrite.
- **Mode 2 — physical redaction (§7.2, on regulator/legal instruction).** `UPDATE` (not `DELETE`)
  against every table §7.2 already lists as covered (`customer_address`, `customer_external_id`,
  `comms_request`, `comms_event`, `consent`) — the row skeleton must survive so campaign reach
  and delivery stats stay reconcilable. Runs against T-024's live `COVERED`/`EXEMPT`
  classification, so this ticket and T-024 must agree on which tables need a statement versus a
  reasoned exemption. Cold partitions (§7.5, T-014) must stay writable for this to reach them —
  already a stated constraint on the partition-lifecycle design, not something this ticket needs
  to change. `VACUUM` on affected partitions is mandatory before reporting completion (the
  pre-update tuple holds the old ciphertext until vacuumed).
- **Erasure audit log** — what was erased, when, which mode, by whom/on what instruction.
- **`erasure_request.backups_clear_at`** — states the date the backup window (§7.3, per-cluster,
  not per-tenant) makes erasure genuinely complete, rather than claiming instant deletion for
  either mode. Still-open item #1 (`14-decisions-and-open-questions.md`) — the actual backup
  retention window value — is a business/ops decision this ticket needs answered at refinement,
  not assumed.
- **Third erasure mode (full row purge)** exists per §7.2 as "explicitly-authorized," "the
  nuclear option" that corrupts historical counts — confirm at refinement whether this ticket
  builds it or leaves it as a documented-but-unbuilt escape hatch; §7.2's own language suggests
  the latter ("do not offer it as a routine choice").

Soft coupling: relies on T-024's exemption list staying accurate — any new customer-linkable
table added after this ships must still pass T-024's CI check, which this ticket does not
change.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-19 — created (TO DO). source: audit: build-order step 15, remaining gap identified when auditing unticketed steps against the board
