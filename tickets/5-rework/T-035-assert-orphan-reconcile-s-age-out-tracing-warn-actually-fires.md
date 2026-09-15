---
id: T-035
title: Assert orphan-reconcile's age-out tracing::warn! actually fires
project: messgr
depends-on: []
spawned-by: [T-033]
impact: low
complexity: low
cost: S
---

# T-035 — Assert orphan-reconcile's age-out tracing::warn! actually fires

## Outcome

`orphan_reconcile::reconcile::run`'s age-out `tracing::warn!` (T-033) has a test that fails if
the warn stops firing or drops a field, closing the one part of T-033's headline behavior that
currently has zero coverage.

## Description

T-033's review (finding F6) found that nothing asserts the `tracing::warn!` T-033 added on the
orphan-reconcile age-out path actually fires. T-033's own Implementation Plan (Task 6)
anticipated this gap and pre-authorized skipping it, on the stated condition that capturing a
`tracing::warn!` in a test would require a new dev-dependency (`tracing_test`) it didn't want to
add for this alone. That condition turns out to be false: `tracing-subscriber` (with the
`env-filter` feature) is already a direct dependency (`Cargo.toml`), and a custom
`tracing_subscriber::layer::Layer` collecting emitted events into a shared `Vec`/`Mutex` needs no
additional crate.

Scope: add a `tracing::subscriber::with_default` (or a project-appropriate equivalent) around a
call to `orphan_reconcile::reconcile::run_for_tenant` in `tests/orphan_reconcile.rs`, using a
minimal capturing layer, and assert the captured event carries `tenant_slug`, `orphan_id`,
`provider_ref`, and `event_type` when a row ages out. Also worth a look while there: T-033's
`unconfigured_tenant_still_ages_out_at_the_hardcoded_default` test is functionally identical to
the pre-existing `no_match_at_cap_deletes_the_row` (same cap, same shape) — decide whether it
earns its own name (a fallback-path regression guard, kept deliberately parallel to the
configured-cap test next to it) or should be dropped in favor of just extending the existing one
with a warn assertion.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-035-orphan-reconcile-warn-coverage
```

### Prerequisite gate (hard)

None. No `depends-on:`. `spawned-by: T-033` is already merged to `main` (PR #50, `06ef25b`).

### Confirmed design decisions (do not deviate without asking)

1. **Capture via a custom `tracing_subscriber::Layer`, not the `tracing_test` crate.** `registry`
   is a default feature of `tracing-subscriber` (confirmed resolved in `Cargo.lock`, pulled in by
   `sharded-slab`/`thread_local`), so `tracing_subscriber::registry().with(...)` is available with
   no new dependency — the premise T-033's Task 6 got wrong.
2. **Install with `tracing::subscriber::set_default`, not `with_default`.** `with_default` takes
   a synchronous closure and cannot wrap an `.await`; `set_default` returns a `DefaultGuard` that
   stays active for its lifetime, which spans the `run_for_tenant(...).await` call cleanly on the
   single-threaded runtime `#[tokio::test]` already uses by default in this file (confirmed: no
   test in `tests/orphan_reconcile.rs` uses `#[tokio::test(flavor = "multi_thread")]`) — the whole
   async body runs on one OS thread, so the thread-local subscriber stays valid across the await.
3. **Implement both `record_str` and `record_debug` on the capturing `Visit`.** `tracing`'s `%`
   shorthand (`orphan_id = %orphan.id`) routes through `record_debug` with a wrapper whose `Debug`
   forwards to the value's own `Display` (no added quoting); a bare `&str` field (`tenant_slug`)
   routes through `record_str`, whose default impl (delegating to `record_debug`) would otherwise
   wrap it in `Debug`-derived quotes. Implementing both directly avoids that mismatch — every
   captured field is compared as its own unquoted text.
4. **Assert the warn's message by substring, every field by exact string equality**, since fields
   captured via decision 3 are unquoted and stable, but the human-readable message text is not a
   contract worth pinning verbatim (`reconcile.rs`'s own wording may reasonably change later).
5. **Keep both `configured_reconcile_attempts_cap_is_honored` and
   `unconfigured_tenant_still_ages_out_at_the_hardcoded_default` as separate tests** (resolving
   the Description's open question) — they exercise genuinely different code paths (a configured
   `tenant_config` row vs. none), and the warn assertion this ticket adds to each is what makes
   the second one non-tautological against the pre-existing `no_match_at_cap_deletes_the_row`,
   not a reason to merge or drop it.

### Tasks

#### Task 1 — capturing test helper (`tests/orphan_reconcile.rs`)

Add near the top of the file, after the existing helper functions:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};

#[derive(Default)]
struct FieldMap(HashMap<String, String>);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .entry(field.name().to_string())
            .or_insert_with(|| format!("{value:?}"));
    }
}

struct CapturingLayer(Arc<Mutex<Vec<HashMap<String, String>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut fields = FieldMap::default();
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

/// Installs a subscriber that captures every WARN-level event's fields for
/// the lifetime of the returned guard (T-035) -- drop it (or let it fall out
/// of scope) once the call under test has returned.
fn capture_warn_events()
-> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<HashMap<String, String>>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CapturingLayer(events.clone()));
    (tracing::subscriber::set_default(subscriber), events)
}
```

If any field's actual captured text does not match what decision 3 predicts (verify by printing
`events` once before asserting), fix the `Visit` impl rather than the assertions -- the point of
this ticket is a test that fails if the warn's real shape drifts, not one tuned to whatever the
first run happens to print.

#### Task 2 — assert the warn in `configured_reconcile_attempts_cap_is_honored`

Wrap the existing `run_for_tenant(...).await` call: install `let (_guard, events) =
capture_warn_events();` immediately before it, then after the existing assertions add:

```rust
let events = events.lock().unwrap();
let warn = events
    .iter()
    .find(|f| f.get("message").is_some_and(|m| m.contains("exceeded reconcile_attempts_cap")))
    .expect("expected the age-out warn to fire");
assert_eq!(warn.get("tenant_slug").map(String::as_str), Some(tenant.slug.as_str()));
assert_eq!(warn.get("orphan_id").map(String::as_str), Some(orphan_id.to_string()).as_deref());
assert_eq!(warn.get("provider_ref").map(String::as_str), Some("never-matches"));
assert_eq!(warn.get("event_type").map(String::as_str), Some("delivered"));
```

#### Task 3 — assert the warn in `unconfigured_tenant_still_ages_out_at_the_hardcoded_default`

Same pattern as Task 2: install the capture guard before `run_for_tenant(...).await`, then assert
the same four fields against that test's own `orphan_id`/`"never-matches"`/`"delivered"` values.

### Acceptance test

```
just build
just test    # tests/orphan_reconcile.rs, specifically the two tests Tasks 2-3 touch
just lint
```

Both touched tests pass with the new assertions; every other test in the file is unaffected.
`just docs-check` is not run — this ticket ships no user-facing change (Docs update below).

### Docs update (mandatory when user-facing)

Not applicable. This ticket adds test coverage only; no CLI, HTTP, or documented-behaviour
surface changes.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint` clean.
2. Write a summary of files touched and decisions made.
3. Suggest a Conventional Commit message, e.g.:

   ```
   test(orphan-reconcile): assert the age-out tracing::warn! actually fires (T-035)
   ```

4. Tidy WIP commits into a small number of atomic commits before presenting (root-path child,
   rules §0).
5. Commit locally on the ticket branch. Do not push or open a merge request without user
   approval; present the commit message and, once approved, finalize, verify the remote base
   is not behind, push, and open the merge request. Hand back to the user.

## Review

- [x] Reviewer independence settled (step 0): **delegated** — the reviewing agent authored the
  branch in this same session, so the implementation/quality/consistency/docs audits (steps
  2-4a) were run by a fresh, independent sub-agent briefed adversarially (no memory of writing
  the code). Its findings were re-verified by hand before recording (below) — one finding it
  could not fully discharge (live acceptance-test execution; its sandbox had no
  Postgres/Vault) was independently re-run and extended by the orchestrating reviewer, who
  found a defect the delegated pass could not have caught without that live execution.
- [x] Implementation audit — tasks and confirmed decisions verified against the diff; acceptance
  test re-run live multiple times (see F1: **not consistently green**) (steps 1, 2)
- [x] Quality audit (step 3) — see F1
- [x] Consistency audit (step 4) — no findings; no duplicate capture helper exists elsewhere in
  `tests/`; hard invariants 1 and 3 not implicated (test-only diff)
- [x] Documentation audit (step 4a) — confirmed accurate: no CLI/HTTP/documented-behaviour
  surface touched; `just docs-check` correctly not run
- [x] Docs-readability pass — skipped, no `.adoc`/`.md` changed by this ticket (step 4b)

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | blocking | correctness | — | The two touched tests are flaky under the default parallel test harness: `tracing::subscriber::set_default` interacts with `tracing-core`'s process-wide, per-callsite `Interest` cache, not a per-thread one. `no_match_at_cap_deletes_the_row` (and any other concurrently-running test) exercises the exact same `tracing::warn!` call site (`reconcile.rs:210`) with no subscriber installed; if its thread reaches that callsite first, tracing caches `Interest::never()` for it globally, and neither touched test's `CapturingLayer::on_event` is ever invoked again for the rest of the process — even though the warn genuinely fires, on the correct thread, with the guard correctly installed. | Reproduced live (real Postgres/Vault, not the delegated reviewer's sandbox): `unconfigured_tenant_still_ages_out_at_the_hardcoded_default` failed 1/25, 1/14, then 1/6 direct-binary runs (`target/debug/deps/orphan_reconcile-*`, no cargo overhead), always the same test, always `tests/orphan_reconcile.rs:6xx: expected the age-out warn to fire`. Added temporary thread-id diagnostics (reverted, not committed): on a reproduced failure, the capture guard's install thread and the `tracing::warn!` call-site thread were **identical** (`ThreadId(9)` both), yet `CapturingLayer::on_event` never printed — ruling out a cross-thread/false-guard-scope bug and pointing at the interest-cache race. Never reproduces running either touched test alone (40+ isolated runs, 0 failures). | Root cause is the interaction between per-test `set_default` and the global per-callsite interest cache — a known class of hazard with this pattern under a parallel test harness. I tried the standard documented mitigation (`tracing::callsite::rebuild_interest_cache()` called right after `set_default` inside `capture_warn_events()`) and stress-tested it (80 direct-binary runs): it did **not** reliably fix the race and the failure rate was *higher* (9/80) than baseline, so it is not a safe drop-in fix — whoever reworks this needs their own stress-test loop (dozens of direct binary runs, not one green `cargo test`) to validate whatever fix is chosen before trusting it. Candidates worth evaluating: serializing the tests that share this call site (`serial_test`'s `#[serial]`, a new dev-dependency — arguably justified now, given this specific documented hazard with the dependency-free approach); or running just these two tests with `--test-threads=1`-equivalent isolation. |

cost: estimated S, actual S

**Disposition summary:** 1 blocking (F1, correctness) — ticket moves to `5-rework/` for a
scoped fix. No non-blocking findings.

## History

- 2026-09-15 — created (TO DO). source: review: T-033's review (finding F6) found the age-out
  `tracing::warn!` has no test coverage, and that the ticket's own stated reason for skipping it
  (no `tracing_test` dependency) doesn't hold — `tracing-subscriber` is already a direct
  dependency and can capture the event without a new one.
- 2026-09-15 — TO DO → READY: plan complete
- 2026-09-15 — READY → IN DEVELOPMENT: picked up
- 2026-09-15 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-15 — IN REVIEW → REWORK: F1 (blocking, correctness) — the two touched tests are flaky
  under the default parallel test harness (tracing per-callsite interest-cache race with
  `set_default`); reproduced live, root-caused, one candidate fix tried and found insufficient.
  See `## Review` for full detail.
