# Procedure: audit the board

When asked to audit the board (or after a burst of moves, as a self-check):

```
pickle board audit
```

It verifies the flow's invariants mechanically: `BOARD.md`'s ticket rows match a fresh render
of the ticket files (the board is generated) — checked in two tiers: every row matching but
only the generated layout (WIP lines, per-child sections) being out of date is a *warning*, a
row itself disagreeing with the tickets is an *error*; both are fixed by `pickle board sync`.
Ids are unique and match filenames; frontmatter is complete with legal
grade values, no key repeated, and a `project:` that names a registered child; `depends-on:`
targets exist and none is the ticket itself; `spawned-by:` targets exist and no ticket cites
itself (but lineage never gates); all seven status directories exist (a missing one is an
error, an empty one with no `.gitkeep` a warning); per-child WIP limits hold; each ticket's
last History transition matches its directory (and an over-long transition/merge line warns);
in-development tickets have all dependencies done (warning if a done dependency has no
`merged` History line); every ticket in `6-done/` carries a `merged` History line of its own
(also a *warning*, never an error — merging is the human's and may lag, so the audit cannot
tell "not merged yet" from "merge line forgotten"; `7-dropped/` is exempt, having nothing to
merge); and every status's own **gate table** (rules §4/§7) holds — each
required section or `### ` sub-heading present with a non-empty body. A **blocking** row unmet
(the READY-gate plan and its seven steps, `## Review` on `5-rework/`) is an *error*; an
**advisory** row unmet (`## Outcome`, T-083) is only ever a *warning*. Neither checks content
quality, only structural presence; `6-done/`/`7-dropped/` declare no gate rows and are
therefore never flagged *by that table* (the merge-line warning above is a separate check).
Fix every error it reports — an error is a broken invariant, not a judgement call.
