See @AGENTS.md for all project guidance.

<!-- pickle:begin -->
## Brine (start here)

**Start at [`tickets/BOARD.md`](tickets/BOARD.md)** — the generated index of every ticket by
status. No feature is built directly from a chat message or a raw idea — work enters only as a
ticket whose Implementation Plan has met the READY gate. A *review finding* is different: it
earns a **disposition** (rules §5), and most are resolved without a new ticket.

- The flow engine is the **brine skill** at `.agents/skills/brine/`. It holds
  the rules (`resources/tickets-README.md`), the ticket template
  (`resources/TEMPLATE.md`), and the review protocol
  (`resources/review-protocol.md`). Claude Code sees it via `.claude/skills/brine`.
  The directory is pickle-owned — `pickle upgrade` replaces it wholesale, so keep
  hand-written notes outside it.
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
