# Procedure: make it a ticket

When asked to turn an idea, finding, or request into a ticket:

1. **Determine the target child-project.** If the request doesn't make it obvious and more than
   one child is registered (`pickle project list`), **ask which child-project the feature
   targets** — do not guess.
2. **Deduplicate first.** Read the titles (and, where plausible matches exist, Descriptions)
   of every ticket in every **non-terminal** directory (`1-to-do/` through `5-rework/`),
   plus the titles in `6-done/`. If the idea substantially overlaps an existing ticket, **do
   not file a duplicate** — extend or cross-reference the existing ticket and tell the user
   which ticket absorbed it. If the overlap is partial, ask the user whether to merge or split.
3. **Author** the ticket: run `pickle ticket new "<title>" --project <name>` to allocate the
   next id for the child's `ticket_prefix` (`max(existing ids sharing that prefix across *all*
   status dirs) + 1` — the counter is per-prefix, not global), scaffold from
   `resources/TEMPLATE.md` into `1-to-do/` with `project:` set, and regenerate the board. When the
   ticket is born from another one, add `--spawned-by "T-NNN[,T-MMM]"`. The title must be a
   single line and the ids must be `T-NNN`, or the command rejects the invocation and writes
   nothing — put multi-line context in the Description, not the title. If the command rejects,
   fix the offending argument (collapse the title to a single line; shorten an over-long one;
   correct any malformed id) and retry before proceeding. Then fill in the `## Outcome` (1–3 sentences, in
   user-observable terms: what changes when this ships) and the Description prose.
4. **Grade it** (impact / complexity / cost) **against the existing backlog** — re-grade
   neighbours if the comparison shifts them.
5. Note any soft couplings (including cross-child ones) in the Description; hard `depends-on:`
   only with user sign-off. **Record where the ticket came from.** `ticket new` scaffolds a
   placeholder `created (TO DO). source: pickle ticket new` — overwrite it, exactly as you just
   overwrote the `## Outcome` placeholder, with a **provenance class** followed by the prose
   reason: `created (TO DO). source: <field-use|self-host|review|audit|chat>: <prose>` (the five
   classes are defined in rules §1). Whenever the source is another ticket, also put that
   ticket's id in `spawned-by:` (lineage; it never blocks pickup, so it needs no sign-off) and
   use the `review` class on the same line. Confirm the `created` History line is present, is
   classed, and the tree is clean (`pickle board audit`).
