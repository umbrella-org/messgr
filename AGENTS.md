# AGENTS.md

Guidance for AI agents working in this repository.

## What this project is

**messgr** — a centralized communications orchestration system and audit ledger for a bank: every SMS, email, and WhatsApp message sent to a customer, in one place, with an API and a UI over it.

**Status: design only.** `DESIGN.md` is the single artifact. No code exists yet. Do not scaffold an implementation unless asked.

`DESIGN.md` is long because it records *why* decisions were made and what was rejected, not just what was chosen. That rationale is the valuable part — when editing, preserve it. Several sections deliberately document mistakes made during design (see "Corrections on the record" below); do not tidy those away.

## Reading order

| Need | Section |
|---|---|
| Orientation | §1–2 (what it is, architecture, tenancy) |
| Any schema change | §4 (data model) — 11 subsections |
| Anything touching sending | §5 (gate chain), §6 (timing), §9 (dispatcher) |
| Anything touching PII | §7 — read all of it before proposing a change |
| Current state of play | "Decisions taken" (24 rows) and "Still open" (16 items), at the end |

Every decision in the table cites the section that justifies it. Follow the citation before changing anything.

## Hard invariants

These were each arrived at by working through a failure mode. Breaking one reintroduces a specific bug, so do not change them without reading the cited section and saying explicitly what you are trading.

1. **Auth/OTP never goes through the queue** (§3). It is synchronous, bypasses the gate chain, and shares no process with the dispatcher. The whole point is that a marketing incident cannot stop customers logging in. Any proposal that routes OTP through the outbox is a regression.
2. **The ledger is self-contained** (§4.1). `customer_id` and the destination live on every row. The customer timeline must never join the customer projection — the projection is a cache, and an audit ledger cannot depend on a cache to be readable.
3. **All gates run at dispatch, never at ingest** (§5, §6.2). A message scheduled three weeks ago is checked against consent as it stands at send time. Validating at submission is a silent compliance hole.
4. **Consent keys on `customer_address.id`**, not on `customer_id` and not on the address value (§5). Address rows are append-only, so a recycled phone number gets a new row and cannot inherit the previous owner's opt-in.
5. **Quotas never block transactional or auth traffic** (§5.1). Hard limits on marketing only. A quota that can withhold a fraud alert has turned cost control into an availability risk.
6. **Every table holding customer data appears in §7.2's erasure statements.** This is the invariant most easily broken by adding a table. Third-party payloads are the classic miss — see "Corrections" below.
7. **Per-customer DEKs from the first write** (§7, §14). Crypto-shredding only works if every payload was written under a customer-scoped key. Cannot be retrofitted.
8. **One codebase; on-prem is the N=1 case of the multi-tenant system** (§2.1). No build flags, no second deployment path.
9. **Dispatchers bypass the connection pooler** (§2.3). Session advisory locks and `LISTEN` do not survive transaction pooling, and the failure mode is silent double-dispatch.
10. **Producer and tenant identity come from mTLS, never from the request body** (§4.9, §11.1).

## Corrections on the record

Three claims were made confidently during design and later found wrong. They are documented in place because the reasoning error is instructive and repeatable:

- **Crypto-shredding does not reach all backups** (§7.3). Wrapped DEKs live in Postgres, so restoring a backup restores the key.
- **Postgres PITR is cluster-level, not per-database** (§13). "Per-tenant point-in-time restore" was overstated; restoring one tenant means full-cluster recovery to a side instance, then logical extraction.
- **RLS on top of database-per-tenant was over-engineering** (§2.1). Removed. With one database per tenant there is no tenant filter for a query to forget.

The pattern in the first two: **backup and recovery boundaries are coarser than the logical boundaries a design draws.** Check that alignment explicitly rather than assuming it. When you find an error like this in your own earlier reasoning, correct it in the document and say so plainly — do not quietly patch it.

## Working style in this repository

**Challenge before implementing.** The user asks for this repeatedly and it has materially improved the design. When asked to add something, look for the contradiction it creates with existing decisions before writing it. Several rounds of "challenge this" have removed more from the design than they added.

**Ask when genuinely uncertain; don't assume.** Standing instruction from the user. This applies to decisions only the user can make (business constraints, regulatory posture, deployment realities) — not to things discoverable in the document.

**Prefer cutting to adding.** The stated design principle is boring technology: the fewest moving parts that satisfy the requirements. Things cut for being unjustified — a `campaign_stats` rollup, a marketing share-of-budget throttle, RLS, weighted jitter — are recorded with their reasoning so they are not reintroduced. If you find yourself adding a mechanism that duplicates an existing one, cut instead.

**Keep the decisions table and open-questions list current.** Any change to a decision updates its row and every section that cites it. After editing, grep for stale cross-references — section numbers, build-step numbers, and superseded terminology drift easily in a document this size.

**Build order is numbered and load-bearing.** Steps carry sequencing constraints with stated reasons (encryption before first write, projection before gates, kill switches before volume). Renumbering means re-checking every reference to a step number.

## Response style

A caveman-mode plugin (`lite`) is active in the user's environment: drop filler, hedging, and pleasantries; keep all technical substance; full sentences retained at this level. **Prose written into `DESIGN.md` and any other artifact is normal professional writing** — the compression applies to conversational replies only.

Never add Claude attribution or co-author trailers to commits, PRs, or any output. This overrides default harness behaviour.

<!-- pickle:begin -->
## Brine (start here)

**Start at [`tickets/BOARD.md`](tickets/BOARD.md)** — the generated index of every ticket by
status. No feature is built directly from a chat message or a raw idea — work enters only as a
ticket whose Implementation Plan has met the READY gate. A *review finding* is different: it
earns a **disposition** (rules §5), and most are resolved without a new ticket.

- The flow engine is the **brine skill** at `.agents/skills/brine/`. It holds
  the rules (`resources/tickets-README.md`), the ticket template
  (`resources/TEMPLATE.md`), and the review protocol
  (`resources/review-protocol.md`). Agents that read `.agents/skills/` find it there
  directly; `pickle install --agent claude` adds a `.claude/skills/brine` view for
  Claude Code. The directory is pickle-owned — `pickle install` and `pickle upgrade`
  both replace it wholesale, so keep hand-written notes outside it.
- Triggers: "make it a ticket", "refine ticket T-NNN", "implement ticket T-NNN", "rework ticket
  T-NNN", "validate ticket T-NNN" (or "review ticket T-NNN"), "audit the board".

### Project configuration

- **Build target.** Every ticket targets one registered child-project via `project:`
  frontmatter (`pickle project list`). Registered child-projects: `messgr`.
- **Branch & commit.** Conventional Commits with the **ticket id in brackets at the end of
  the subject** (e.g. `feat(cli): add board audit (T-2)`) for child-project code. Ticket/board
  bookkeeping uses its own `board: T-NNN <verb phrase>` form instead — grammar and scope in
  the rules §0. Branch per child:
  - `messgr`: `feat/T-NNN-<slug>`
- **WIP limits** (per child):
  - `messgr`: `3-in-development/` ≤ 1 · `4-in-review/` ≤ 1
- **Commit policy.** Child-projects are **publish-gated**: local WIP commits are encouraged;
  **no push / no merge request without explicit user approval**; after approval, finalize
  (squash or keep history) + push + open the MR — **merging is always the human's**.
  Overarching bookkeeping (tickets, board, docs) may be committed automatically,
  always with **explicit pathspecs** (`git add <paths>`, never `git add -A`/`.`).
- **Where commits land.** Code goes on the child's feature branch; **ticket and board
  bookkeeping is committed on the base branch**, never on a feature branch — a squash-merge
  folds or drops it and the board then disagrees with the tickets it indexes. This covers a
  review's own moves too, and it is why a reviewer on a feature branch reads the ticket from
  the base branch. This project uses the `in-tree` layout, where the board and the code share
  one repository, which is what makes the rule load-bearing here.
  `pickle hooks install` enforces it locally, once per clone: a `pre-commit`
  hook refuses the commit, and a `pre-push` hook refuses the push if it still slipped through
  (bypass either with `--no-verify`).

### Board rule

`tickets/BOARD.md` is **generated** — regenerated wholesale from the ticket files by
`pickle ticket new`, `pickle ticket move` and `pickle board sync`. **Never edit it by
hand**; hand-written planning notes go in `tickets/NOTES.md`. Every ticket move = move
the file + one dated `## History` line, and the board regenerates. Prefer
`pickle ticket move` — it does all of it atomically.
<!-- pickle:end -->
