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

## 0. Reviewer independence — who runs the audits

An agent that just wrote a branch is a poor auditor of it: the code reads as obviously correct
to whoever just decided, line by line, that it was — the same reasoning that produced it is the
reasoning that would have to catch a flaw in itself. This is the same bias the ticket-pickup gate
already guards against by requiring a spawned, unbiased sub-agent (this skill's
`resources/tickets-README.md` §8); a review needs the same handoff, because the reviewer may be
auditing work it just wrote rather than work it merely inherited.

**Trigger.** When the agent about to run this review authored the branch under review in this
same session, delegate the audits (steps 2 through 4a) to an independent reviewer: spawned
fresh, with no memory of writing the code, briefed adversarially and instructed to find defects
rather than confirm the work. Hand it the ticket as step 1 reads it, the branch to audit, and
the child's configured commands — an independent reviewer starts with no context, and one left
to find its own can audit a stale ticket or the wrong branch. A reviewer with no hand in the
branch is already independent — nothing needs delegating.

**Boundary.** Delegation covers the audits only. Classification and severity, the four
dispositions, moving the ticket, and the approval gate (step 9) stay with the orchestrating
reviewer — delegating those would replace the reviewer rather than de-bias it.

**Verify before recording.** An independent reviewer has no stake in the outcome, but equally no
context, so expect it to report things that are wrong. Re-verify every delegated finding by hand
before it enters the findings table (step 5) — delegation buys independence, not accuracy.

**Record which happened, every time.** Independent, delegated, or skipped — the review's
checklist says which. This is not only a degradation notice: a review that ran its own audits
because the reviewer had no hand in the branch records that too. Silence cannot distinguish an
independent review from a self-review, which is what makes the archive unauditable later.

**When the host cannot spawn an independent reviewer**, the next-best handoff is a fresh session
with no memory of writing the branch (see below). When neither is available, this step degrades
to a recorded conscious skip, the same shape as step 4b's below: it never blocks the review, but
the skip is written into the checklist rather than left silent.

**Session and tier.** Entering review is a natural point to start a fresh session at a heavier
reasoning tier — the judgement calls in step 5 benefit from both independence from whoever
implemented the ticket and stronger reasoning. Once the verdict is reached, the remaining steps
(record, move, publish) are mechanical and do not need that tier — offer to drop back down rather
than carrying the heavier session through the publish steps by default.

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
  existing `## Review` section first. The scope is **the findings listed there, plus the diff that
  closed them** — not a re-audit of the whole feature from scratch. Read the new text or code the
  fix pass wrote and audit it as you would any other new work, classing whatever you find from §5's
  vocabulary, whether or not a listed finding names that ground: a fix's own replacement text is
  the one part of the branch nothing has audited yet. Two mechanics:
  - **Getting the diff.** The fix pass records its commits in `## Review` under
    `### Rework fix record — round N (commit <sha>)` for a single commit — read that one with
    `git show <sha>` — or `(commits <tip before the fix>..<tip after>)` for several, which
    `git diff <tip before the fix>..<tip after>` takes as written. Mind which is which: the single
    form names the fix commit itself, the pair opens with the commit *before* the fix. A round that
    committed nothing says `no commits this round — <why>`, which closes the question rather than
    leaving you to hunt. If that record is missing or its SHAs no longer resolve — a tidy at publish
    time rewrites them — reconstruct the range from the branch log instead of skipping the read, and
    treat the missing or broken record as a finding of its own (never blocking on its own).
  - **The bound.** This is **this round's** fix diff, not every rework diff since the first
    review — each round reads its predecessor's new text, so nothing goes unread and no round
    re-reads the whole branch.
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
- Errors of fact, dead references, stale paths — including the project's own governing
  documents (its design of record, conventions, the locked-decisions guide, decisions log, and
  whatever else `AGENTS.md` itself names). Step 7 states what to do with any this branch made
  false; surface them here so they are classified before the ticket moves (step 6).

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

Follow any project-specific documentation rules named in `AGENTS.md`. This sweep is scoped to the
docs tree the project *ships*; the project's own governing documents are a different surface —
see step 7.

## 4b. Docs-readability pass — *optional second opinion on changed prose*

If the host environment provides a **docs-readability reviewer** — a read-only,
suggestions-only reviewer for documentation prose — run it over the `.adoc`/`.md` files the
ticket changed, present each suggestion, and apply the approved ones yourself. Examples of
hosts: a `docs_readability` tool in a pi session, a `docs-readability` subagent in an
opencode session; any other session can shell out to
`opencode run --agent docs-readability --file <changed.adoc|.md> "…"` where that subagent is
configured.

This step is **genuinely optional and never blocks a review**. The reviewer only *suggests* (it
must not edit files), and reviewing without it — no reviewer configured, the session cannot reach
one, or its output cannot be trusted (below) — is a sanctioned **conscious skip**, recorded in the
ticket's `## Review` (tick the checklist line with a "skipped: …" note). Its suggestions
are readability polish, not findings: apply or discard them during the review; they never enter
the findings table and never move a ticket to `tickets/5-rework/`.

**Verify every suggestion's quoted "current text" against the file before presenting it.** The
reviewer is a model being asked to quote source text, which is the same shape of risk step 0
already names for a delegated audit: *"delegation buys independence, not accuracy"* — an outside
reviewer's claim is not automatically true just because it arrived with a suggestion attached.
Locate each quoted string in the file the suggestion names, comparing **words and punctuation
only** and ignoring layout — line wrapping, indentation, and any per-line prefix the format adds,
such as a blockquote marker or a list bullet carried onto a wrapped line. Layout is how the file
is set, not what it says, so a quote is **verbatim** when the words and punctuation match in
order, wherever the source happens to break. A quote that still does not match under that
comparison is **discarded, not repaired** — guessing which real passage was meant is the same
invention this check exists to catch, now with a reviewer's authority behind it. If most of a run
fails this way, discard the run and re-invoke once. If the re-invoked run fails the same way,
take the conscious skip above — a reviewer whose output cannot be trusted is one of its
sanctioned grounds — rather than retrying indefinitely. Record how many suggestions were
discarded as fabricated on the same checklist line that carries this step's conscious-skip note —
bookkeeping on an optional pass, not a finding, so it never becomes a findings-table row.
Also discard, on the same terms, any suggestion that changes content rather than wording —
deleted emphasis that carries meaning, a dropped documented path, a swapped precise term are
content edits whatever the reviewer labels them; the reviewer's own instructions already forbid
this, so discarding one is confirming it followed its own constraints, not applying a new rule.

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
the skill's procedure) then addresses *only the listed findings* on the same branch, records the
commits it produced in `## Review` (the rework fix record — §1), and moves
the ticket back to `tickets/4-in-review/` (History line on each move); a **scoped re-review**
verifies those findings *and reads the diff that closed them* (§1), then concludes via 6a/6b again.

**6b. If there are no blocking findings** (zero, or only non-blocking with every one
dispositioned): move `tickets/4-in-review/T-NNN-*.md` → `tickets/6-done/`; append a `## History`
line noting the verdict and the disposition summary, including any spawned ids.

**6c. A blocking finding first identified at step 7, 8 or 9 — after 6b has already moved the
ticket to the terminal `6-done/`.** `6-done/` declares no outbound transition, so the finding
cannot take the `5-rework/` route above, and it is not dispositioned either — the rules §5's
four dispositions, `new ticket` included, are defined only for non-blocking findings. **It is
filed as its own ticket instead**: `pickle ticket new … --spawned-by "T-NNN"` (this skill's
`resources/TEMPLATE.md`, graded per the rules §3) — the same command the `new ticket`
disposition uses, but not that disposition, since a blocking finding is never dispositioned.
More than one such finding in the same review pass is **batched by theme**, exactly as the rules
§5 batches non-blocking follow-ups: findings of the same theme join one filed ticket rather than
each starting a new one; findings of different themes are filed separately. Append a dated
`## History` line to the concluded ticket for each ticket filed, recording its id — the archive
stays terminal, but the pointer is not silent — and a finding that joins a ticket already filed
needs no second line. Steps 7, 8 and 9 below take this route for a blocking finding about the
ticket that just moved; their own text otherwise covers the non-blocking case.

## 7. Update other references — and reconcile the project's governing documents

- **`BOARD.md` needs no hand edit** — it is generated, and the `pickle ticket move` in step 6
  (and any `pickle ticket new` for spawned tickets) already regenerated it. If it looks stale,
  run `pickle board sync`; never edit it by hand.
- Any ticket or doc that referenced this ticket by id.
- **The project's governing documents** — its design of record, conventions, the
  locked-decisions guide, decisions log, and whatever else `AGENTS.md` itself names — get
  reconciled against what the branch shipped, or an explicit note recording why not.

A governing document this branch made false is a finding like any other: surfaced by the
consistency audit (step 4), classed `stale-xref`, and classified at step 5 on the rules §5's
ordinary terms. **This step defines no severity of its own.** A document merely lagging the
branch matches none of the rules §5's blocking categories — it breaks no golden path, ships no
wrong behaviour, and contradicts no *ticket's* locked decision — so it is non-blocking there,
without this step needing to say so twice.

Within the review's reach — the repository holding the branch under review, or any repository
this review is already authorised to commit to under the project's commit policy — **reconcile it
here and record the edit**, taking the rules §5 disposition for prose this branch made false with
no behaviour change; it is never a silent edit. Out of reach, record the reach limit as the reason
and disposition it per the rules §5. A falsehood first noticed at this step rather than at step 4
takes the same route.

"Or record why not" is not a blanket alternative. The legitimate grounds are that the document is
right and the ticket's own prose was loose; that the reconciliation belongs to a ticket that owns
the document; that it is out of the review's reach (above); or that the reconciliation is too
large to make in this review, which takes a follow-up-ticket disposition per the rules §5. A
review that leaves a governing document asserting something the code no longer does, and says
nothing about why, has not finished.

This is a different surface from 4a's: 4a audits the docs the project *ships*, while a governing
document is the one the *next ticket is written from*, which is why it needs naming separately.

## 8. Impact sweep — *did T-NNN change assumptions for later tickets?*

Re-read every ticket in `tickets/2-ready/` and `tickets/1-to-do/` that lists this ticket in
`depends-on:` (or references it in Description) and check whether the implementation invalidated
any assumption they encode. Patching the affected ticket **is** the expected outcome — record the
correction in its History. This sweep is a spawn gate like any other, so anything it turns up that
is not a patch takes a disposition per the rules §5, and a new ticket needs the promotion test. A
blocking finding about the ticket that just concluded, rather than about a dependent ticket, is
filed per step 6c instead of taking a disposition.

## 9. Finish

- A blocking finding noticed only now — writing the summary is often when the whole change is
  finally seen at once — takes step 6c's route before anything below is presented.
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

- [ ] Reviewer independence settled (step 0): audits run independently, delegated, or a recorded conscious skip — name which
- [ ] Implementation audit — acceptance test re-run, tasks & criteria verified; on a scoped re-review, the diff that closed the findings also read for new defects (steps 1, 2)
- [ ] Quality audit (step 3)
- [ ] Consistency audit (step 4)
- [ ] Documentation audit — coverage, whole-tree sweep, docs build clean (step 4a, if the project ships docs)
- [ ] Docs-readability pass on the ticket's changed `.adoc`/`.md` files — every suggestion's quoted text verified against the file, any fabricated ones discarded and counted — or a conscious skip recorded (step 4b, optional)
- [ ] Findings recorded in the ticket's `## Review` with severity, **class**, **and** disposition per the rules §5; disposition summary line present, and a `cost: estimated …, actual …` line beneath it (step 5)
- [ ] Ticket moved to `tickets/6-done/` or `tickets/5-rework/`; `## History` appended (step 6)
- [ ] Other references updated if needed; governing documents reconciled, or an explicit note why not; board regenerated by the move (step 7)
- [ ] Remaining-tickets impact sweep done (step 8)
- [ ] Summary + child-project commit message & MR attributes presented for approval (commit/push/MR per the project's commit policy; merge stays human), **and under `layout = "in-tree"` only, remote base verified not behind local (`origin/<base>...HEAD` carries no `tickets/` path, after a fetch) before pushing** — a `pre-push` hook, if armed, checks this too, but the manual check still runs — + overarching-repo bookkeeping committed per policy + next-ticket suggestion (step 9)
