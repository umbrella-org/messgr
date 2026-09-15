---
id: T-039
title: Dispatcher leader election (HA)
project: messgr
depends-on: []
spawned-by: []
impact: critical
complexity: medium
cost: M
---

# T-039 — Dispatcher leader election (HA)

## Outcome

After this ships, only one dispatcher instance per tenant can ever claim and send messages —
mechanically enforced, not by operational convention — so a second instance started by accident
(bad deploy, stuck rolling restart) cannot double-send.

## Description

Closes the second half of build-order step 7 (§9, DESIGN.md). Retry/backoff (the first half)
shipped in T-021; leader election did not — confirmed directly in code:
`src/bin/dispatcher.rs`'s own doc comment says "leader election (`pg_try_advisory_lock`) is still
a later ticket (build order step 7's other half)," and `src/dispatcher/repo.rs:193` says "exactly
one dispatcher instance runs per tenant today (no leader election yet, T-013 decision 3)."

This is also AGENTS.md hard invariant 9 ("dispatchers bypass the connection pooler") — the
invariant's rationale (session advisory locks and `LISTEN` don't survive transaction pooling,
failure mode is silent double-dispatch) is currently unenforced, resting on the assumption that
nobody ever runs two instances. Per §2.3, the dispatcher already holds a **direct** (non-pooled)
Postgres connection for `LISTEN` — this ticket adds `pg_try_advisory_lock` on that same
connection at startup, holding the lock for the process's lifetime, and exiting/retrying if it
can't acquire it.

Build-order.md's own isolation-suite requirement (line 33) already specifies the acceptance bar:
"Dispatcher leader election holds under a forced failover, over a direct connection, with exactly
one active dispatcher observed throughout (§2.3)" — this is not a new test to invent, it's a test
already committed to that currently has nothing to exercise.

## Implementation Plan

<!-- empty until refined; must meet the READY gate before moving to 2-ready/ -->

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis, next after the gate-chain batch (T-036-T-038) — closes AGENTS.md invariant 9's live
  exposure, already flagged in-code as pending.
