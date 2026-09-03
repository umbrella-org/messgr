---
id: T-019
title: Size one tenant's ledger and derive cluster limits
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: low
cost: S
---

# T-019 — Size one tenant's ledger and derive cluster limits

## Outcome

A written sizing document states, for a tenant across the design's 100k–5M messages/day range: expected `comms_request`/`comms_event` storage per year, a recommended cluster size and tenants-per-cluster count, a quoted single-tenant restore RTO, and the point at which the 18-month slow-storage migration (§7.5) starts to matter. No code changes.

## Description

DESIGN.md's own Still-open #15 states the prerequisite this ticket exists to close: "no per-tenant storage estimate exists anywhere in this document." The ledger holds rendered message bodies (§7) for 7 years across a 50× volume range (100k–5M/day, §1) — an email-heavy tenant's `payload_ciphertext` dominates its own size and therefore the cluster's, so nothing about capacity planning can proceed until one tenant is sized.

This ticket is a document, not a migration or a code change. It must produce a concrete number (or a small table across the volume range) for:

- Average row size for `comms_request` and `comms_event` at realistic payload sizes per channel (SMS ~160 bytes, email/WhatsApp larger), including ciphertext overhead (AES-256-GCM nonce + tag) and the encrypted `provider_payload_ciphertext`.
- Annual storage growth per tenant across the 100k–5M/day range, and the resulting 7-year total.
- From that: how many tenants of what size profile reasonably share a Postgres cluster (§2.1's "not thousands of small tenants" assumption needs a number behind it).
- The 18-month cold-storage migration point (§7.5) restated in GB rather than months, so ops can plan the slower tablespace's capacity.
- A single-tenant restore RTO (§13's full-cluster-recovery-to-a-side-instance procedure), as a function of the cluster size this sizing produces.

Unblocks the deferred DESIGN.md commit recording this ticket's number (§2.1, §7.5, §13) and closes Still-open #15 (single-tenant restore RTO), which names this ticket's number as its explicit unresolved prerequisite. **Correction made during refinement:** the original Description also claimed this closes Still-open #14 (regional backup policy). It doesn't — #14 is which actual retention window a region commits to, a negotiation with that region's tenants over compliance needs, not a number sizing derives. (Separately, #14 reads as a near-duplicate of Still-open #1, which asks the same backup-window question — that's a pre-existing overlap in DESIGN.md, out of scope here; flagging it rather than fixing it inline so it isn't lost.)

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-019-size-tenant-ledger
```

Local WIP commits as work proceeds. Publish only per the project's commit policy (root-path
child — tidy WIP into atomic commits before presenting; no push/MR without user approval).

### Prerequisite gate (hard)

None. This ticket reads DESIGN.md §1, §2.1, §4.1, §4.4, §7, §7.3, §7.5, §13 and writes prose
back into the same file; it has no code dependency and no `depends-on:` ticket.

### Confirmed design decisions (do not deviate without asking)

1. **Average rendered-payload size per channel: SMS 160 bytes (already fixed by §1's own
   constraint line), email 3,000 bytes, WhatsApp 400 bytes.** No real template inventory exists
   yet to measure from, so email/WhatsApp are estimates confirmed with the user during
   refinement — call them out as assumptions in the written document, not as measured facts, so
   a future revision knows to replace them once real templates exist.
2. **Representative per-tenant traffic mix: 60% SMS / 25% email / 10% WhatsApp / 5% auth**,
   confirmed with the user during refinement for the same reason as decision 1. `class=auth`
   messages carry `channel=sms` and `payload_ciphertext IS NULL` (§7.4) — they cost a
   `comms_request` row but no payload bytes.
3. **`comms_event` rows per message: 3 on average** — `queued` and `sent` (dispatch-internal,
   no `provider_payload_ciphertext`), plus one terminal provider-sourced event (`delivered`,
   `failed`, or `bounced`) carrying an average 500-byte raw delivery-receipt JSON, encrypted.
   This undercounts tenants with heavy `read`/`complaint` event traffic; state it as a baseline,
   not a ceiling.
4. **Physical storage multiplier: 1.6×** applied to raw row bytes to approximate index overhead
   (`comms_request` carries four indexes, `comms_event` one) and page/TOAST overhead. This is a
   rule-of-thumb, not a measurement — say so in the document.
5. **Target cluster storage ceiling for the tenant-packing calculation: ~10 TB usable per
   cluster**, confirmed with the user during refinement. State it as the planning assumption it
   is, not a hard platform limit.
6. **Retention baseline for the headline table: 7 years** (the design's default; `tenant_config`
   already makes this per-tenant configurable — note that a shorter tenant retention scales the
   whole table down linearly, no separate derivation needed).
7. **Backup retention window for the RTO estimate: 90 days**, matching §7.3's own worked
   example — reuse it rather than inventing a second number.

### Tasks

#### Task 1 — Compute average row size for `comms_request` and `comms_event`

Using decisions 1–4, compute, per channel/class (SMS marketing/transactional, email, WhatsApp,
auth) and as the blended average under the decision-2 mix:

- `comms_request`: fixed columns (`tenant_id`, `id`, `created_at`, `customer_id`, `channel`,
  `class`, `template_id`, `template_version`, `campaign_id`, `destination_hmac` — a fixed
  32-byte HMAC, `destination_ciphertext`, `producer_id`, `final_status`, `finalized_at`) plus
  `payload_ciphertext` (payload + AES-256-GCM's 12-byte nonce + 16-byte tag + varlena header),
  plus Postgres per-tuple overhead (~24-byte header + line pointer + null bitmap).
- `comms_event`: fixed columns (`comms_request_id`, `customer_id`, `occurred_at`, `event_type`,
  `provider_ref`, `provider_status`) plus `provider_payload_ciphertext` on the one
  provider-sourced event in three (decision 3), same AES-GCM overhead.

Show the arithmetic in the document, not just the answer — a reviewer or a future revision
needs to see which number to change when a real template lands.

#### Task 2 — Blend into an average bytes-per-message figure and project annual/7-year growth

Blend Task 1's per-channel row sizes under the decision-2 mix into one bytes-per-message
figure (`comms_request` + `comms_event`), apply the decision-4 multiplier, then project across
the design's stated 100k–5M messages/day range (§1):

- messages/year = messages/day × 365
- bytes/year = messages/year × blended bytes/message
- 7-year total = 7 × bytes/year (steady-state; note this as a simplification — it ignores
  organic volume growth within the 7-year window, which would make the true total larger)

Present as a small table with at least the 100k/day and 5M/day endpoints, and a
representative mid-point (e.g. 1M/day).

#### Task 3 — Derive tenants-per-cluster and update §2.1

Compare Task 2's per-tenant 7-year totals against the decision-5 cluster ceiling. Expect (and
state plainly if true) that the top of the range does not pack cleanly: a single 5M/day tenant's
7-year footprint can approach or exceed the whole cluster ceiling by itself, while a 100k/day
tenant's footprint is small enough that several fit comfortably. Write the conclusion into §2.1
as a size-tiered packing statement (e.g. small tenants share densely, the largest tenants get a
cluster closer to themselves alone), replacing the unquantified "not thousands of small tenants"
line with an actual number range.

#### Task 4 — Restate the §7.5 migration point in GB and update it

§7.5 currently states the cold-storage migration point as "18 months." Using Task 2's
bytes/year for each range endpoint, restate it in GB: how much data sits on the fast tablespace
at any time (roughly 1.5 years of accumulation) before the oldest partitions start moving to
slow storage, for a small and a large tenant. Add this as a sentence or short table inline in
§7.5.

#### Task 5 — Derive a single-tenant restore RTO and update §13

Using Task 3's cluster size and §13's existing three-step procedure (full-cluster PITR to a
side instance, `pg_dump` the tenant, load into production), state an RTO as a function of
cluster size: assume a full-cluster restore-and-WAL-replay throughput and a separate
dump/restore throughput (state both assumed rates explicitly as planning estimates, not
measurements — this design has no rehearsed runbook yet per §13's own text), sum with a
provisioning/human-runbook overhead, and quote a range (e.g. "N–M hours/days for a
~10 TB cluster, assuming spare recovery capacity already stands by; add provisioning time
otherwise"). Add this to §13's restore-procedure paragraph.

#### Task 6 — Update Still-open and Decisions taken

- Resolve Still-open #15: replace it with a line recording the numbers this ticket produced
  (or a short summary plus a pointer to §2.1/§7.5/§13), following the pattern item #9 already
  uses for a partially-resolved item (`~~struck~~` original text, then what remains open, if
  anything still is).
  Leave Still-open #14 untouched — this ticket does not close it (see the Description
  correction above).
- Add a new row to **Decisions taken** recording the sizing numbers and their status as
  estimates (not measurements), citing §2.1, §7.5, §13 — following the existing table's format
  (Decision / Consequence columns, one terse row).

### Acceptance test

- `just docs-check` passes.
- Every number introduced in §2.1, §7.5, §13, Still-open, and Decisions taken traces to the
  arithmetic shown in this ticket's own write-up (Tasks 1–5) — a reviewer re-derives at least
  one endpoint (100k/day or 5M/day) by hand from the stated assumptions and gets the same
  order-of-magnitude answer.
- Every number introduced carries an explicit "assumption, not a measurement" qualifier
  somewhere near it (inline or via a footnote-style aside), matching this document's existing
  convention (§7.3's corrections, §12.1's caveat) of never presenting an estimate as settled
  fact.
- `grep -c "^## " DESIGN.md` and the section-number cross-references in `AGENTS.md`'s reading-order
  table still match reality (no section was renumbered by this change — only prose was added
  inside existing sections).

### Docs update (mandatory when user-facing)

DESIGN.md itself is the only doc — this ticket's tasks *are* the docs update (§2.1, §7.5, §13,
Still-open, Decisions taken). No separate doc, no user-facing surface outside DESIGN.md.

### Finish (mandatory)

1. Acceptance test green; `just docs-check` clean.
2. §2.1, §7.5, §13, Still-open, and Decisions taken all updated and internally consistent.
3. Write a summary: the blended bytes/message figure, the 100k/1M/5M-per-day table, the
   tenants-per-cluster conclusion, the §7.5 GB restatement, and the quoted RTO range — plus
   which numbers are assumptions pending real template data.
4. Suggested commit message: `docs(design): size one tenant's ledger, derive cluster limits and restore RTO (T-019)`.
5. Tidy WIP commits into atomic ones (root-path child).
6. Commit locally on the ticket branch; present for approval before push/MR, per commit policy.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-02 — created (TO DO). source: audit: design/implementation audit of DESIGN.md against the shipped code and ticket reviews; closes Still-open #15's stated prerequisite.
- 2026-09-03 — TO DO → READY: implementation plan complete, all seven gate items present.
- 2026-09-03 — TO DO → READY: plan complete
- 2026-09-03 — READY → IN DEVELOPMENT: picked up
