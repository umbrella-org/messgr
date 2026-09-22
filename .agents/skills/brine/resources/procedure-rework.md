# Procedure: rework a ticket

When asked to rework ticket T-NNN (a review found blocking findings):

Under `layout = "in-tree"`, before reading the ticket, resolve its current status from the base
branch rather than trusting the worktree — `git ls-tree -r --name-only <base> -- tickets |
grep -- "/T-NNN-"`, then `git show <base>:<that path>` — and, once any pre-existing
`feat/T-NNN-*` branch for this ticket is checked out (a resumed pickup; a fresh one has no branch
yet), run `pickle doctor` and resolve any stale-ticket-branch warning first.

1. The ticket must be in `5-rework/` on the base branch (per the lookup above) — if not, stop and
   explain.
2. Read the ticket's `## Review` section: the **blocking findings are the entire scope**.
   Implement nothing else — any new work needs a new ticket.
3. On the **same** `feat/T-NNN-<slug>` branch (in the target child's repo), fix only the listed
   findings (local commits per the commit policy — they make the re-review diffable). Note the
   branch tip **before your first fix commit**: that is what the re-review diffs against.
4. Re-run the acceptance test and the child's build/validate commands until green.
5. Record what was fixed against each finding in `## Review`, and **re-read the replacement text
   you just wrote** before handing back — nothing else audits it before it ships, and you are its
   cheapest reader. Head the record `### Rework fix record — round N (commit <sha>)` for a single
   commit, or `(commits <the tip you noted in step 3>..<tip after>)` for several — the form
   `git diff <before>..<after>` takes as written — or `no commits this round — <why>` for none.
   Record the SHAs as they stand when you hand back, and do not tidy the branch here: the tidy
   belongs to publishing, and it rewrites them (§1's fallback covers a record whose SHAs no longer
   resolve). The scoped re-review reads that diff (`resources/review-protocol.md` §1).
6. `pickle ticket move T-NNN in-review --reason "findings fixed"` and hand back for a **scoped
   re-review**.
