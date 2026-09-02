---
id: T-NNN
title: <short title>
project: <child-project name>   # which registered child-project this ticket targets
                                # (a name from `pickle project list` / pickle.toml)
depends-on: []              # hard dependencies only (other T-NNN ids, any child-project); [] if none
spawned-by: []              # lineage only — ticket(s) this was born from; [] if none; NEVER gates pickup
# family: T-NNN             # optional single umbrella id (same child); groups pickup order on the board; NEVER gates; omit if none
impact: critical|high|medium|low
complexity: high|medium|low
cost: S|M|L|XL               # implementation effort — see tickets/README.md §3
# unrefined tickets may use adjacent-pair ranges (low-medium, medium-high, S-M, M-L, L-XL);
# refinement collapses them to a single value
---

# T-NNN — <title>

## Outcome

<!-- 1–3 sentences, in user-observable terms: "After this ships, <who> can <do/see what>."
Descriptive, not evaluative — state what changes, not whether it was worth it (that
judgement belongs to grading, not this section). Worked example: "After this ships, you
can decide whether a TO DO ticket is worth refining by reading its first two lines,
instead of reconstructing the payoff from a mechanism narrative." Checked by
`pickle board audit` as a warning (never a gate) when absent, empty, or still this
placeholder — see tickets/README.md §7. This one placeholder is an HTML comment, not
the `<…>` form the other sections use, precisely so leaving it in place trips that
warning: the check strips HTML comments and asks whether anything is left. -->

## Description

<The current spec of the feature, in prose. Kept up to date as understanding evolves —
this is what the ticket *is*, in its latest form. Note soft couplings to other tickets
here (cross-reference by id) — hard dependencies go in `depends-on:` frontmatter instead.
The `project:` frontmatter names which child-project this feature targets; a cross-child
soft coupling (e.g. a frontend feature that pairs with a backend one) is noted here, a hard
one goes in `depends-on:`. If this ticket was born from another one — a review finding, a
board audit, a refinement split — record that parent in `spawned-by:` (lineage, never a
blocker).>

## Implementation Plan

<Empty while in TO DO. Filled in during exploration/refinement; must satisfy the READY
gate (tickets/README.md §4) before the ticket moves to `2-ready/`. Structure:>

### 0. Feature branch (mandatory)

Before any change, create a feature branch **inside the target child-project's repo**
(the `project:` frontmatter names it; its path is in pickle.toml):

```
cd <child-project path>
git checkout main            # or the agreed base
git checkout -b feat/T-NNN-<slug>
```

Do all work on this branch, committing locally as you go (WIP commits are encouraged —
crash-safety and diffable rework rounds). Publish only per the project's commit policy
(default: never push the child-project or open a merge request without explicit user
approval): end with a summary and a suggested commit message (see Finish, below), tidying WIP
commits into atomic ones first for a root-path child (`path = "."`, tickets/README.md §0); once
the user approves, finalize the branch (squash to the approved commit, or — the root-path
default — keep the tidied history; the user chooses at approval time).
Under `layout = "in-tree"` only, **before pushing, verify the remote base is not behind your
local base** —
`git fetch origin <base> && git diff --name-only origin/<base>...HEAD | grep '^tickets/'` must
print nothing, or push `origin <base>` first (rules §0 explains why, and why the default
`umbrella` layout has nothing to check here) — then push and open the
merge request — **merging is always the human's.**

> **Project configuration wins.** The branch name above uses the flow's default prefixes
> (`feat/`, `T`); the commit policy stated is also a default. The project's `AGENTS.md` /
> `pickle.toml` states what is actually configured — it wins on any disagreement.

> If this ticket depends on an un-merged branch (in any child-project), **stop and tell the
> human** rather than building on top of it.

### Prerequisite gate (hard)

<Any preconditions that must hold before starting — merged branches, clean tree, other tickets
in `6-done/` and merged in their child (should already be reflected in `depends-on:`). Keep this
heading and write "none" if there are none — rules §4 checks that the heading is present with a
non-empty body, so deleting it fails that check instead of satisfying it.>

### Confirmed design decisions (do not deviate without asking)

<Numbered, unambiguous decisions the implementer must honour. Pull any project-wide decisions
from the project's own docs / `AGENTS.md`. Write each as one numbered item whose leading bold
run is the decision statement, e.g. `1. **The check never writes to the ticket tree.** It is a
read-only report, so a failure can be retried without cleanup.` — the rationale follows the bold
run. Number in one unbroken list and never renumber: another ticket may cite an item by its
ordinal as `<ID> decision <N>`. See tickets/README.md §7 for the full convention.>

### Tasks

#### Task 1 — <name>
<What to build, where, and how. Reference exact paths in the target child-project.>

#### Task 2 — <name>
<…>

### Acceptance test

<Concrete, runnable checks that prove the ticket is done. Use the child-project's configured
build/test/lint/docs commands and the specific behaviour to exercise, with expected results.
A reviewer must be able to re-run these verbatim.>

### Docs update (mandatory when user-facing)

<Which docs to add/update and where to register them. Keep this heading and write "no
user-facing surface" if it ships none — rules §4 checks that the heading is present with a
non-empty body, so deleting it fails that check instead of satisfying it.>

### Finish (mandatory)

1. Acceptance test green; the child-project's build/validate commands clean.
2. Docs updated and registered.
3. Write a **summary** of everything done (files touched, decisions made, anything deferred).
4. Suggest a **Conventional Commit message** for the human: `<type>(<scope>): <description>`
   (Conventional Commits; `<scope>` is optional — per spec, omit the parens entirely rather
   than filling them with a placeholder like `all` when the change is genuinely broad and
   has no single scope. `<scope>` must never be the ticket id itself; the ticket id is
   appended in brackets at the end of the subject line), e.g.:

   ```
   <type>[(<scope>)]: <concise summary> (T-NNN)

   <body — what and why>
   ```

5. **Tidy up before presenting** — for a root-path child (`path = "."`, tickets/README.md §0),
   interactive-rebase the branch's WIP commits into a small number of atomic, correctly
   typed/scoped commits before presenting them: this is what replaces squash-on-merge as the
   default for that case. A child at a nested path can skip this step (squash still applies
   there).
6. Commit locally on the ticket branch. Publish only per the project's commit policy
   (default: do **not** push or open a merge request without user approval). Present the
   commit message; only after approval finalize the branch (squash, or — the root-path default
   above — keep the tidied history; the user chooses at approval time). Under `layout =
   "in-tree"` only, before pushing verify
   the remote base is not behind your local base — `git fetch
   origin <base> && git diff --name-only origin/<base>...HEAD | grep '^tickets/'` must print
   nothing, or push `origin <base>` first (rules §0) — then push and open the merge request
   (merging is always the human's). Hand back to the user.

## Review

<Empty until IN REVIEW. Filled in by the review protocol (`review-protocol.md`): first the
findings table, using the canonical column skeleton and `class` vocabulary defined once in
`review-protocol.md` §5 (do not restate the column list or the `class` values here).
Legal disposition values, and which one is the default, are defined in `tickets/README.md §5`.
Then add a one-line disposition summary, a `cost: estimated …, actual …` line beneath it, the
verdict, notes from any scoped re-review, and the id of any ticket a finding was spawned into or
absorbed by. A rework round adds its own sub-section here —
`### Rework fix record — round N (commit 6f0f135)` for one commit,
`(commits 91c4de2..b33b6ad)` for several (the tip *before* the fix, then the tip after), or
`no commits this round — <why>` — naming what was fixed against each finding: that commit or
range is what the following scoped re-review reads (`review-protocol.md` §1).>

## History

<!-- append-only; newest last. One line per status transition, dated YYYY-MM-DD, in the form
     `OLD → NEW: one-clause reason`. The first line is
     `created (TO DO). source: <field-use|self-host|review|audit|chat>: …` (the provenance
     class vocabulary is defined in `tickets/README.md §1`); a human merge is recorded as
     `merged to <base> (<MR ref>[, <commit>])` — a commit reference (short SHA, and a full
     commit link where the remote resolves to a known hosting URL) is recommended alongside the
     MR ref so the line traces straight to what shipped. A plan edit made after the ticket left
     `2-ready/` is its own non-transition line, `plan amended inline: <what changed and why>`
     (`tickets/README.md §1`) — it carries no `OLD → NEW` and is never flagged by the
     over-long-line warning that applies to transition/merge lines. Examples:
     - 2026-07-13 — TO DO → READY: implementation plan complete
     - 2026-07-14 — READY → IN DEVELOPMENT: picked up, branch feat/T-NNN-<slug>
     - 2026-07-14 — plan amended inline: dropped task 3, the config file it targeted was removed
       by T-KKK after this ticket went READY
     - 2026-07-15 — IN DEVELOPMENT → IN REVIEW: acceptance test green
     - 2026-07-16 — IN REVIEW → DONE: review clean; 6 non-blocking, all dispositioned
       (3 fixed in review, 2 absorbed by T-KKK, 1 → T-MMM)
     - 2026-07-17 — merged to main (MR !12, a1b2c3d)
-->

- YYYY-MM-DD — created (TO DO). source: <field-use|self-host|review|audit|chat>: <prose>
