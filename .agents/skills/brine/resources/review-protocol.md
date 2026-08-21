# The "validate ticket T-NNN" protocol

The canonical, self-contained procedure to follow whenever the human says **"validate ticket
T-NNN"** or **"review ticket T-NNN"** (synonyms). Any agent that sees either phrase runs
these steps in order. Read the project's
`AGENTS.md`/`CLAUDE.md` first for the ground rules and the project's configured commands, then
the workflow rules (this skill's `resources/tickets-README.md`), then follow this.

**Layered review addenda (extra project-specific audit rules keyed to these step numbers).**
Pickle supports two optional addendum layers, applied **on top of — never instead of** — this
procedure, in this order:

1. the **overarching** addendum (`review_addendum` at the top of `pickle.toml`) — rules true
   of the whole project (cross-child consistency, commit-title convention, …);
2. the **per-child** addendum (`review_addendum` inside the ticket's target `[[project]]`) —
   that child-project's tech-specific checks.

Reviewing a ticket runs **this generic protocol → overarching addendum → the ticket's own
child addendum** (whichever exist).

> **Scope.** A review does **not** re-implement the ticket. It verifies, audits, and records
> findings *into the ticket* (the board regenerates from the ticket moves). A **blocking** finding becomes a scoped
> rework pass on the same ticket (§5, §6a). A **non-blocking** finding takes one of the four
> dispositions defined in **the rules §5**, whose default is to record and close. Exactly one of
> the four permits a fix during the review itself; the rules state its bar, it is deliberately
> narrow, and it is **never silent** — every disposition is written into the findings table. That
> recording is what keeps this scope rule true in substance while letting a reviewer correct a
> typo it would be absurd to schedule.
>
> **Review on the ticket's feature branch.** The work under review lives on the un-merged
> `feat/T-NNN-<slug>` branch in the ticket's target child-project — check it out to audit and
> to re-run the acceptance test. Create no *additional* review branch. Local commits on the
> ticket branch are allowed at any time; publishing follows the project's configured commit
> policy (default: never push a child-project or open a merge request without explicit user
> approval — see step 9). End with a summary, the commit message(s) and merge-request attributes
> for approval, and a next-ticket suggestion.
>
> **In the `in-tree` layout, read the ticket from the base branch.** This applies when the board
> and the code share one repository — `layout = "in-tree"` in `pickle.toml`. There, bookkeeping is
> committed on the base branch, not on the feature branch (the rules §0), so a branch cut before
> the ticket's move to `4-in-review/` landed shows a **stale ticket file** in its worktree — an
> older Implementation Plan, a missing History line, a status the board contradicts. Checking out
> the feature branch to audit the code is precisely what exposes you to it, which is why the
> instruction sits next to the one above. The branch is authoritative for the *code*; the base
> branch is authoritative for the *ticket and the board*. Read them with
> `git show <base>:tickets/4-in-review/T-NNN-*.md` (or from a checkout of the base branch), and
> record this review's own findings and moves on the base branch too.
>
> Under the default `umbrella` layout, checking out the *child's* branch never exposes you to
> this: the board lives in the overarching project, a different repository from the child whose
> branch you just checked out, so there is only one copy of the ticket and the child's worktree
> was never a candidate to hold a stale one. But the overarching project's *own* worktree is a
> copy like any other, and the identical hazard applies to it if it is ever checked out on a
> feature branch of its own (rules §0) — rare, since bookkeeping there is usually committed
> straight to its base branch, but not impossible. Read the ticket directly only when the
> overarching project's worktree is itself on its base branch; otherwise read it from there the
> same way, `git show <base>:tickets/4-in-review/T-NNN-*.md`.
>
> **Project configuration wins.** The branch prefix (`feat/`), the ticket-id prefix (`T`, in
> every `T-NNN` here), the commit policy above, and any WIP limit named elsewhere in this
> protocol are the flow's defaults. The project's `AGENTS.md` marker block renders what
> `pickle.toml` actually configures for each child, and it wins on any disagreement.

---

## 1. Load context

- Locate the ticket: `tickets/4-in-review/T-NNN-*.md`. Under `layout = "in-tree"`, read it **as it
  exists on the base branch** — if the feature branch is already checked out,
  `git show <base>:tickets/4-in-review/T-NNN-*.md` rather than the worktree copy, since a stale
  read there makes a review audit the wrong plan (see the box above). Under the default `umbrella`
  layout the child's branch is never the hazard — read the ticket from the overarching project's
  worktree directly, unless *that* worktree is itself on a feature branch of its own, in which
  case read it from its base branch the same way (see the box above). Then read it in full:
  `## Description`,
  `## Implementation Plan` (its acceptance test, tasks, and confirmed decisions are the
  checklist for step 2), and `## History`.
- If this is a **scoped re-review** (the ticket was previously in `tickets/5-rework/`), read the
  existing `## Review` section first — only the findings listed there need re-verification; do
  not re-audit the whole feature from scratch.
- Read any project-wide decisions and the configured build/validate commands from the project's
  `AGENTS.md` and the ticket's target `[[project]]` block in `pickle.toml`.
- Check `depends-on:` frontmatter — every listed ticket must be in `tickets/6-done/` **with
  its feature branch merged to the base of _its own_ child-project's repo** (done ≠ merged; a
  cross-child dependency is satisfied only once its branch is merged in the child it targets;
  check the board's `merged` column / the dependency's History).

## 2. Implementation audit — *was the ticket fully implemented?*

Verify against the **actual project tree** (the ticket's target child-project), not the prose:

- Every **task** in the Implementation Plan is done, in the files it names.
- Every **acceptance criterion** is met. **Re-run the acceptance test verbatim** and record the
  result. Run the child-project's configured build / test / lint / docs commands as applicable.
- Every **confirmed design decision** was honoured, including any project-specific rules named
  in `AGENTS.md`.

Record each item as **met / partially met / not met**, with evidence (path, command output,
line reference).

## 3. Quality audit — *was it done to industry best practice?*

- Idiomatic, correct code for the language and framework.
- Test coverage adequate for the change; tests actually assert behaviour.
- Error handling, edge cases, security (input validation, secret handling, injection).
- Docs accurate, complete, and registered.
- Prompt/config content (if applicable) unambiguous and internally consistent.

## 4. Consistency audit — *inconsistencies, contradictions, errors, ambiguities, redundancies, duplications*

Search both **within** the new code and **between** it and the rest of the project:

- Caller ↔ callee contract drift.
- Contradictory instructions across modules/docs.
- Ambiguous or under-specified behaviour.
- Redundant or duplicated logic/content that should be shared.
- Errors of fact, dead references, stale paths.

Use project-wide search — do not eyeball a single file.

## 4a. Documentation audit — *coverage, whole-tree consistency, build health*

If the project ships documentation:

1. **Coverage.** If the ticket ships or changes anything user-facing (a command, flag,
   behaviour, config option, workflow), confirm the docs actually cover it, in the right place,
   registered/linked, cross-referenced rather than duplicated. Missing coverage is a
   **blocking** finding.
2. **Whole-tree consistency sweep.** Search the **entire** docs tree — not only the pages the
   ticket touched — for contradictions, factual errors, stale references, duplication that
   should be shared, and broken links/anchors.
3. **Build the docs** with the project's configured docs command; fix any source errors inline
   (a broken doc build is a build-correctness bug, not new behaviour) and record the result.

Follow any project-specific documentation rules named in `AGENTS.md`.

## 4b. Docs-readability pass — *optional second opinion on changed prose*

If the host environment provides a **docs-readability reviewer** — a read-only,
suggestions-only reviewer for documentation prose — run it over the `.adoc`/`.md` files the
ticket changed, present each suggestion, and apply the approved ones yourself. Examples of
hosts: a `docs_readability` tool in a pi session, a `docs-readability` subagent in an
opencode session; any other session can shell out to
`opencode run --agent docs-readability --file <changed.adoc|.md> "…"` where that subagent is
configured.

This step is **genuinely optional and never blocks a review**: the reviewer only *suggests*
(it must not edit files), and reviewing without it — no reviewer configured, or the session
cannot reach one — is a sanctioned **conscious skip**, recorded in the ticket's `## Review`
(tick the checklist line with a "skipped: …" note). Its suggestions are readability polish,
not findings: apply or discard them during the review; they never enter the findings table
and never move a ticket to `tickets/5-rework/`.

## 5. Classify and record findings — severity, then disposition

Write directly into the ticket's own **`## Review`** section (no separate file).

A **findings table**, with severity and disposition as **separate columns**. This is the
canonical skeleton — paste it in and fill rows, rather than restating the column list in prose
(a prose list lets every author reinvent the header, and reviews whose tables are not shaped the
same way twice cannot be compared):

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
<!-- | F1 | blocking | correctness | — | one-line description | file:line or command output | what to do | -->

`severity` and `disposition` are defined in **the rules §5**, which is their single source of
truth — do not restate the list of dispositions here. `class` is defined once, here — do not
restate it elsewhere.

### The `class` column — closed vocabulary

One word per row, from this closed list only (do not add, rename or re-order it — its value is
comparability across reviews, which a vocabulary that shifts under you destroys). Each value
carries a one-line test:

| class | test |
|---|---|
| `correctness` | ships wrong behaviour or wrong output |
| `test-gap` | coverage missing, deleted, or tautological (a test that cannot fail) |
| `docs-gap` | user-facing docs missing, wrong, or in the wrong place (includes `CHANGELOG.md`) |
| `stale-xref` | a reference this branch made false: line anchors, cross-references, comments, or plan prose describing behaviour that changed |
| `plan-wrong` | **reserved** — a *confirmed design decision* was false or unworkable. Plan prose merely made stale by the branch is `stale-xref`, not this |
| `spec-unclear` | shipped prose that is self-contradictory, ambiguous, or under-specified for execution |
| `design` | asymmetry, narrowing, dead code, or performance — no behaviour change |
| `other` | none of the above; if this exceeds ~10% of rows, the vocabulary is wrong — say so |

Two worked examples. A byte-widened `unicode.IsSpace` scan that emits invalid UTF-8 is
`correctness`, even if the surrounding code and its comments read as correct. A scope rule that is
satisfied by both of its own branches is `spec-unclear`, not `docs-gap` — the documentation
exists, it just cannot be executed against.

- **Blocking** — breaks the golden path, ships wrong behaviour, contradicts a locked decision
  (cite it as `<ID> decision <N>` — rules §7), or is missing required docs coverage
  (4a.1). **Do not fix it inline**, and leave the
  disposition cell `—`: a blocking finding is not dispositioned, it is fixed. It still carries a
  `class` — the kind-of-defect axis is most valuable precisely on the blocking rows. The ticket
  moves to `tickets/5-rework/` (step 6a) for a scoped fix, then back to `tickets/4-in-review/`
  for a scoped re-review.
- **Non-blocking** — quality/consistency/polish that doesn't block shipping. Give each one
  **exactly one** disposition per the rules §5, and record it. The default is to note and close;
  a follow-up ticket must pass that section's promotion test — *"would this actually be
  scheduled?"* — and must be **batched by theme**, one ticket carrying several findings rather
  than one ticket per finding. Spawn it with
  `pickle ticket new … --spawned-by "T-NNN"` (from this skill's `resources/TEMPLATE.md`, graded
  per the rules §3); the lineage is provenance only and does **not** make the follow-up wait on
  the ticket under review. Reference the new id in the table; a finding absorbed by an existing
  ticket cites that ticket's id. The reviewed ticket proceeds to `tickets/6-done/` (step 6b).

Close with a one-line **disposition summary** under the table, counting each disposition and
naming the ids involved — so the shape of the review is legible without reading every row. It is
also the number to watch: a review that mints a ticket per finding is not reviewing, it is
deferring.

Directly under the disposition summary, add a second one-line closer recording cost actual vs
estimate:

```
cost: estimated <S|M|L|XL>, actual <S|M|L|XL>[ — <one clause, only when they differ>]
```

The reviewer writes this line, having just re-run the acceptance test and read the branch. The
estimate is copied verbatim from the ticket's `cost:` frontmatter; the frontmatter itself is
**not** rewritten — the divergence between estimate and actual is the datum, and overwriting the
estimate would erase it.

## 6. Move the ticket

**6a. If any blocking finding exists:** move `tickets/4-in-review/T-NNN-*.md` →
`tickets/5-rework/`; append a `## History` line. A fix pass (**"rework ticket T-NNN"** — see
the skill's procedure) then addresses *only the listed findings* on the same branch and moves
the ticket back to `tickets/4-in-review/` (History line on each move); a **scoped re-review**
verifies just those findings and concludes via 6a/6b again.

**6b. If there are no blocking findings** (zero, or only non-blocking with every one
dispositioned): move `tickets/4-in-review/T-NNN-*.md` → `tickets/6-done/`; append a `## History`
line noting the verdict and the disposition summary, including any spawned ids.

## 7. Update other references

- **`BOARD.md` needs no hand edit** — it is generated, and the `pickle ticket move` in step 6
  (and any `pickle ticket new` for spawned tickets) already regenerated it. If it looks stale,
  run `pickle board sync`; never edit it by hand.
- Any ticket or doc that referenced this ticket by id.

## 8. Impact sweep — *did T-NNN change assumptions for later tickets?*

Re-read every ticket in `tickets/2-ready/` and `tickets/1-to-do/` that lists this ticket in
`depends-on:` (or references it in Description) and check whether the implementation invalidated
any assumption they encode. Patching the affected ticket **is** the expected outcome — record the
correction in its History. This sweep is a spawn gate like any other, so anything it turns up that
is not a patch takes a disposition per the rules §5, and a new ticket needs the promotion test.

## 9. Finish

- Summarize: what was verified, findings by severity, the ticket's new status, any newly-spawned
  tickets, and the remaining-tickets impact.
- **Child-project (build target):** present the full Conventional Commit message —
  `<type>(<scope>): <description>` (`<scope>` optional and never the ticket id; the ticket id
  is appended in brackets at the end of the subject line, e.g. `(T-42)`), plus body — **and the
  merge-request attributes** (title, description, target branch) **to the user for approval**.
  Local WIP commits on the ticket branch are fine at any time — the approval gate governs
  **publishing**. For a root-path child (`path = "."`, tickets/README.md §0), tidy the WIP
  commits into a small number of atomic, correctly typed/scoped commits before presenting them,
  and default to keeping that history on merge rather than squashing. Only after the user
  approves all attributes, finalize the branch (squash to the single approved commit, or — the
  root-path default above — keep the tidied history; the user chooses at approval time).
  Under `layout = "in-tree"`, **before pushing, verify the remote base is not behind your local
  base**: `git fetch origin <base> && git diff --name-only origin/<base>...HEAD | grep
  '^tickets/'` must print nothing. §0 explains the three-dot choice, the fetch, and the failure
  this catches. Any output means push `origin <base>` first and re-check. An installed
  `pre-push` hook (§0) performs the same check automatically on push, but it does not replace the
  manual step: hooks are per-clone and bypassable with `--no-verify`, so run the check by hand
  regardless of whether the hook is armed. Under the default `umbrella` layout this check has
  nothing to find — the child's repository contains no `tickets/` path to leak — so skip it.
  Only then push and **create the merge request** in that child-project's repo.
  Publishing follows the
  project's configured commit policy (default: never push a child-project or open a merge
  request without approval); **merging is always the human's.**
- **Overarching project** (tickets, board, bookkeeping): commit per the project's commit
  policy — may be automated, always with **explicit pathspecs** (`git add <paths>`, never
  `git add -A`/`.`) so deliberately-untracked material is never swept in. A commit that only
  changes ticket/board state takes the `board: T-NNN <verb phrase>` form rather than a
  Conventional Commit — grammar and scope in the rules §0.
- **Suggest the next ticket**: the next item in `BOARD.md`'s READY section (impact order,
  respecting `depends-on:` and per-child WIP limits), whether its prerequisite gate is
  satisfied — **explicitly stating whether the human must merge the just-reviewed branch first**
  (a dependent ticket may not start until the dependency is done *and merged* in its own child,
  rules §3) — and offer to pick it up.

---

### Checklist (paste into the ticket's `## Review` section)

- [ ] Implementation audit — acceptance test re-run, tasks & criteria verified (step 2)
- [ ] Quality audit (step 3)
- [ ] Consistency audit (step 4)
- [ ] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a, if the project ships docs)
- [ ] Docs-readability pass on the ticket's changed `.adoc`/`.md` files, or a conscious skip recorded (step 4b, optional)
- [ ] Findings recorded in the ticket's `## Review` with severity, **class**, **and** disposition per the rules §5; disposition summary line present, and a `cost: estimated …, actual …` line beneath it (step 5)
- [ ] Ticket moved to `tickets/6-done/` or `tickets/5-rework/`; `## History` appended (step 6)
- [ ] Other references updated if needed; board regenerated by the move (step 7)
- [ ] Remaining-tickets impact sweep done (step 8)
- [ ] Summary + child-project commit message & MR attributes presented for approval (commit/push/MR per the project's commit policy; merge stays human), **and under `layout = "in-tree"` only, remote base verified not behind local (`origin/<base>...HEAD` carries no `tickets/` path, after a fetch) before pushing** — a `pre-push` hook, if armed, checks this too, but the manual check still runs — + overarching-repo bookkeeping committed per policy + next-ticket suggestion (step 9)
