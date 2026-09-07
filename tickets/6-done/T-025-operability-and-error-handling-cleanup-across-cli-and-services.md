---
id: T-025
title: Operability and error-handling cleanup across CLI and services
project: messgr
depends-on: []
spawned-by: []
impact: medium
complexity: medium
cost: M
---

# T-025 — Operability and error-handling cleanup across CLI and services

## Outcome

`migrate` reports success visibly regardless of `RUST_LOG`; malformed configuration fails loudly and consistently rather than half-panicking and half-silently-defaulting; a malformed Vault response is a returned `Err` rather than a panic; numeric `tenant-config set` args and `provider-config set --channel` are validated instead of silently accepting nonsense; the ~20 remaining `panic!`/`.expect()` sites across the CLI binaries fail as a reported error instead of a crash; `tenant_message_stats` matches every sibling function's domain-error shape; and CI calls the same `just` recipes a developer runs locally.

## Description

Batches seven small, previously-`noted` findings from prior ticket reviews that individually fail the "would this actually be scheduled?" promotion test but together are worth one pass, plus one gap this audit found directly. None are behaviour changes to the product; all are operability. Uniform in kind — every item converts a panic to an `Err`, tightens a validation gap, or fixes a stale doc/CI inconsistency; nothing here adds a new mechanism or surface.

**Two items originally scoped here were split out at refinement** into their own tickets, because
each needed a real design decision and new mechanism, not a mechanical conversion: `T-031`
(`TenantRegistry` eviction — cancelling a background poll loop, not just a `HashMap` TTL) and
`T-032` (health endpoints — a second unauthenticated listener per binary, not a one-line route).
**One item (T-001/F7) was dropped as already fixed**: verified live that `messgr-control --help`
and a bare invocation already print usage without touching `Config::from_env()` —
`Cli::parse()` already runs first in `src/bin/control.rs:370`, and clap itself intercepts
`--help`/a missing subcommand before any of our code runs.

From prior reviews (all currently `noted`, standing as recorded in their tickets):

1. **T-001/F9** — `Command::Migrate` reports success only via `tracing::info!`, filtered by `RUST_LOG` (unset in `.env.example` and CI, and `tracing-subscriber`'s `env-filter` feature defaults to dropping `info` without it — confirmed live: `messgr-control migrate` with `RUST_LOG` unset prints nothing), so `just control-migrate` prints nothing on success while `provision` correctly uses `println!`. Fix: match `provision`'s behaviour.
2. **T-001/F12** — Two contradictory malformed-config policies: `Profile::from_env()` panics on an unrecognized value (by design); `Config::from_env()` silently substitutes a default for an unparseable `DATABASE_MAX_CONNECTIONS` (`src/config.rs:22-25`), under a doc comment that claims uniform loud failure (`src/config.rs:14-16`). Fix: pick one policy (loud, per `Profile`'s precedent) and make the comment match reality.
3. **T-003/F2** — `create_dek`/`unwrap_dek` and `VaultKeyStore::connect`'s settings-build path use `.expect(...)`/`panic!(...)` for base64-decode and Vault-protocol-shape failures (`src/keystore.rs`: the settings-build `panic!` at what is currently line 194, the missing-plaintext `.expect()` at 245, and the two base64-decode `.expect()`s at 248 and 264), despite every one of these functions returning `Result<_, KeyStoreError>`. `KeyStoreError` is currently a tuple struct wrapping only `vaultrs::error::ClientError` — widen it to an enum (`Client(vaultrs::error::ClientError)` / `Protocol(String)`) to carry these too. Confirmed safe: every other caller (`registry.rs`, `vault.rs`, `provision.rs`, `dev_pki.rs`, `ingest/model.rs`, `customer_dek/lifecycle.rs`, `tenant_pepper.rs`, `customer/resolve.rs`) only ever wraps `KeyStoreError` opaquely via `From`, none pattern-match its internals. Fix: propagate as `Err` — a malformed Vault response should not crash the dispatcher.
4. **T-007/F2** — no validation that `tenant-config set`'s numeric args are non-negative. T-007's Review named `--retention-years`, `--schedule-horizon-days`, and `--staleness-max-age-seconds`, but the last of those was never actually shipped as a flag (confirmed: no `staleness` reference anywhere in `src/`) — the real current numeric arg set is `--retention-years` (`i32`), `--schedule-horizon-days` (`Option<i32>`), and `--kill-switch-release-rate` (`Option<i32>`, added later by T-016, not covered by T-007's original finding). Validate all three are non-negative. **T-012/F1** — `provider-config set --channel` accepts unvalidated free text while `template approve --channel` validates against the same three-value closed set via `PossibleValuesParser` (confirmed exact scope from T-012's Review, finding F1: reuse `template::model::channel`'s constants).
5. Roughly twenty remaining `panic!`/`.expect()` sites across the CLI binaries (`src/bin/control.rs`, `src/bin/dispatcher.rs`, `src/bin/ingest.rs`) that should be a reported error and a non-zero exit instead, per the pattern items 2/3 above establish — audit and convert during this ticket rather than one at a time. Env-var-missing-at-startup panics (e.g. `Config::from_env`'s `CONTROL_DATABASE_URL`, `VaultKeyStore::connect_as_tenant`'s `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID`) and `assert_tls_outside_dev`'s refusal to start without TLS outside `profile = dev` are **not** in scope — those are deliberate, already-loud, startup-time configuration failures matching item 2's "loud, per `Profile`'s precedent" policy, not the crash-on-bad-runtime-data class this item targets.

Found directly by this audit, not previously reviewed:

6. **CI inlines `cargo build`/`cargo test`/raw `docker exec`/raw `curl` commands** in the `test` job (`.github/workflows/ci.yml`) instead of calling the `just` recipes (`just build`, `just control-migrate`, `just tablespace-init`, `just vault-dev-init`, `just test`) a developer runs locally — the `fmt`/`clippy` jobs already call `just fmt-check`/`just lint`, so this is scoped to the `test` job only. Two paths for the same operation drift silently; CI should call the recipes. `just tablespace-init`'s container name is hardcoded to `messgr-postgres` (matching local `compose.yml`), which CI's dynamically-named service container won't match — add a `container` parameter (default `messgr-postgres`) so CI can pass its own service container id without duplicating the recipe.
7. **T-028/F5** — `src/stats.rs::tenant_message_stats` returns bare `Result<_, sqlx::Error>` for an unknown-tenant-slug rejection (signalled via `sqlx::Error::Configuration`, the same mechanism `provider_config`/`tenant_config`'s `ConfigureError` use internally), unlike every other same-shaped "resolve slug, connect, act" function (`partition_lifecycle::run_for_tenant` → `PartitionLifecycleError`, `producer::register` → `ProducerError`, `provider_config::configure`/`tenant_config::configure` → `ConfigureError`), which wrap the identical mechanism in a named domain error. Add `StatsError` matching `ConfigureError`'s exact shape (`Database(sqlx::Error)` + `Display`/`Error`/`From`) and update `src/bin/control.rs`'s `Command::Stats` handler.

## Implementation Plan

### 0. Feature branch (mandatory)

```
git checkout main
git checkout -b feat/T-025-operability-and-error-handling-cleanup-across-cli-and-services
```

Local WIP commits as you go. Do not push or open a merge request without explicit user
approval (publish-gated, root-path child — tidy WIP into atomic commits before presenting,
rules §0/§4 item 7).

### Prerequisite gate (hard)

None. `depends-on: []`.

### Confirmed design decisions (do not deviate without asking)

1. **Scope is exactly the 7 items in the Description**, all mechanical (panic→`Err`, a
   validation gap, a stale doc line, a CI/justfile drift) — no new mechanism or surface. Item 1
   (T-001/F7) confirmed already fixed and dropped; items 7 and 8 from the original filing split
   out to `T-031`/`T-032`.
2. **`main()`'s "startup" boundary in each binary is out of scope; everything after it is in
   scope for panic→`Err` conversion.** "Startup" = connecting to the control DB, loading
   `Config`, connecting to Vault, loading TLS material, and (dispatcher-only) the per-tenant
   setup sequence before any claim loop or listener starts serving — a process that can't reach
   its own required infrastructure has nothing useful to do, and panicking loudly there matches
   the accepted `Config::from_env` precedent (already the case, not changed here). In scope:
   `messgr-control`'s subcommand bodies (the domain operation IS the point of a one-shot CLI
   invocation, unlike infra bootstrap) — roughly twenty sites in `src/bin/control.rs`, matching
   Description item 5's estimate almost exactly. `messgr-dispatcher`/`messgr-ingest`'s own
   bin-level panics are therefore **not** touched by this ticket (they're all in their startup
   sequences per this rule) — only `src/keystore.rs` (item 3) and `src/stats.rs` (item 7) get
   touched inside those binaries' call graphs.
3. **One exception inside the in-scope region stays a panic**: `src/bin/dispatcher.rs`'s
   `refresh_draining.write().expect("draining lock poisoned")` (inside the ongoing kill-switch
   refresh loop, so technically post-startup) is left alone — a poisoned lock reflects a prior,
   unrelated panic already having occurred elsewhere in the process, and converting it requires
   making `run_refresh_loop`'s `on_delta` callback fallible, a signature change disproportionate
   to this ticket's mechanical scope. Not this ticket's problem to solve.
4. **`KeyStoreError` widens from a tuple struct to an enum** (`Client(vaultrs::error::ClientError)`
   / `Protocol(String)`) to carry base64-decode and Vault-protocol-shape failures (item 3).
   Confirmed safe: every caller (`registry.rs`, `vault.rs`, `provision.rs`, `dev_pki.rs`,
   `ingest/model.rs`, `customer_dek/lifecycle.rs`, `tenant_pepper.rs`, `customer/resolve.rs`)
   only ever wraps it opaquely via `From`; none pattern-match its internals.
5. **`src/bin/control.rs`'s subcommand-body panic conversion (item 5) uses a `run()` wrapper**,
   not a `Result`-returning `main()` directly: extract everything in `main()` from the
   `match cli.command { ... }` block onward into `async fn run(cli: Cli, config: Config,
   control_pool: PgPool) -> Result<(), String>`, replacing every in-scope `panic!(...)`/
   `.unwrap_or_else(|err| panic!(...))`/`.expect(...)` with `return Err(format!(...))` /
   `.map_err(|err| format!(...))?`, preserving the exact existing message text. `main()` becomes:
   ```rust
   #[tokio::main]
   async fn main() -> std::process::ExitCode {
       let cli = Cli::parse();
       if matches!(cli.command, Command::Version) { /* unchanged */ }
       tracing_subscriber::fmt::init();
       let config = Config::from_env();
       let control_pool = db::connect(&config.control_database_url, config.database_max_connections)
           .await
           .expect("failed to connect to control database"); // startup, decision 2 — unchanged
       match run(cli, config, control_pool).await {
           Ok(()) => std::process::ExitCode::SUCCESS,
           Err(err) => { eprintln!("error: {err}"); std::process::ExitCode::FAILURE }
       }
   }
   ```
   No new dependency (no `anyhow`) — plain `String` errors match this file's existing
   `format!("... : {err}")` message style exactly.

### Tasks

#### Task 1 — `src/bin/control.rs`: `Command::Migrate` success reporting (item 1)

Change the `tracing::info!("control database migrations applied");` inside `Command::Migrate`
to `println!("control database migrations applied");`, matching `Provision`'s existing
`println!` pattern. Keep the `tracing` call too if useful for structured logs, or drop it — the
point is that success is visible with `RUST_LOG` unset, which only `println!` guarantees.

#### Task 2 — `src/config.rs`: uniform loud failure on malformed config (item 2)

Change `DATABASE_MAX_CONNECTIONS` handling so a *missing* var still defaults to 20, but a
*present-but-unparseable* one panics (matching `Profile::from_env`'s policy) instead of
silently substituting the default:
```rust
let database_max_connections = match env::var("DATABASE_MAX_CONNECTIONS") {
    Ok(v) => v
        .parse()
        .unwrap_or_else(|_| panic!("DATABASE_MAX_CONNECTIONS must be a valid u32, got {v:?}")),
    Err(_) => 20,
};
```
Extend `Config::from_env`'s doc comment to state the policy explicitly: "Panics with a clear
message on a missing required variable, or on one present but unparseable — failing loudly
beats failing confusingly on first use," so the comment actually matches what the code now does.

#### Task 3 — `src/keystore.rs`: propagate Vault protocol/decode failures as `Err` (item 3)

- Widen `KeyStoreError` (currently `pub struct KeyStoreError(vaultrs::error::ClientError);`) to:
  ```rust
  #[derive(Debug)]
  pub enum KeyStoreError {
      Client(vaultrs::error::ClientError),
      Protocol(String),
  }
  ```
  Update its `Display`/`Error::source`/`From<vaultrs::error::ClientError>` impls to match on
  both variants (`Client` keeps today's exact behaviour; `Protocol` displays the string as-is,
  `source()` returns `None` for it).
- `connect_settings` (currently ~line 194): change
  `.unwrap_or_else(|err| panic!("failed to build Vault client settings: {err}"))` to
  `.map_err(|err| KeyStoreError::Protocol(format!("failed to build Vault client settings: {err}")))?`.
- `create_dek` (currently ~lines 245, 248): change the missing-plaintext `.expect(...)` and the
  base64-decode `.expect(...)` to `.ok_or_else(|| KeyStoreError::Protocol("vault returned no \
  plaintext for DataKeyType::Plaintext".into()))?` and
  `.map_err(|err| KeyStoreError::Protocol(format!("vault returned invalid base64 plaintext: \
  {err}")))?` respectively.
- `unwrap_dek` (currently ~line 264): same base64-decode conversion as above.
- Leave `assert_tls_outside_dev`'s `panic!` untouched (decision 2 — a security refusal-to-start,
  not a malformed-response case) and the two `VAULT_ROLE_ID`/`VAULT_WRAPPED_SECRET_ID`
  missing-env-var panics in `connect_as_tenant` untouched (decision 2 — startup config, same
  class as `Config::from_env`'s).

#### Task 4 — CLI input validation gaps (item 4)

- `src/bin/control.rs`'s `TenantConfigCommand::Set`: add
  `value_parser = clap::value_parser!(i32).range(0..)` to `retention_years`,
  `schedule_horizon_days`, and `kill_switch_release_rate` (all currently plain `i32`/`Option<i32>`
  with no range check) so a negative value is rejected at parse time with clap's own error,
  before the subcommand body ever runs.
- `src/bin/control.rs`'s `ProviderConfigCommand::Set` and `::List`: change `channel: String` to
  the same `value_parser = clap::builder::PossibleValuesParser::new([channel::SMS, channel::EMAIL,
  channel::WHATSAPP])` `TemplateCommand::Approve` already uses (`crate::template::model::channel`
  is already imported). Update/remove the now-false doc comment on `Set`'s `channel` field
  ("free text, not validated... no shared enforcement exists between the two tables yet").

#### Task 5 — `src/bin/control.rs`: convert subcommand-body panics to `Err` (item 5)

Per decisions 2 and 5: extract the `match cli.command { ... }` block into `async fn run(...)`,
converting every panic!/.expect()/.unwrap_or_else(panic) inside it to `?`/`return Err(...)`,
preserving message text verbatim. This covers `Provision`, `Producer::{Register,Disable,List}`,
`DevPki::{Bootstrap,IssueCert}` (including its file-write `.expect()`s), `TenantConfig::{Set,Show}`,
`CustomerDek::PreProvision`, `Template::{Approve,Show,List,Render}` (including the
`--body-file`/`--var` parsing panics), `ProviderConfig::{Set,List}`, `PartitionLifecycle::Run`,
and `Stats` (coordinate with Task 7's `StatsError` — its `?` now propagates a `StatsError` via
`.map_err(|err| format!("failed to compute stats for tenant {tenant_slug:?}: {err}"))?`, same
message text as today). Update `main()` to the wrapper shape in decision 5. The four
`#[cfg(test)]`-module `.expect("parsing ... must succeed")`/`.expect("expected X")` clap-invariant
unwraps are test code, not production paths — leave them.

#### Task 6 — CI calls the same recipes a developer runs locally (item 6)

- `justfile`: change `tablespace-init` to accept a container name, defaulting to today's
  hardcoded value:
  ```just
  tablespace-init container="messgr-postgres":
      docker exec {{container}} mkdir -p /var/lib/postgresql/tablespaces/messgr_cold
      docker exec {{container}} chown postgres:postgres /var/lib/postgresql/tablespaces/messgr_cold
      docker exec {{container}} psql -U messgr -d control -c \
          "CREATE TABLESPACE messgr_cold LOCATION '/var/lib/postgresql/tablespaces/messgr_cold'" || true
  ```
- `.github/workflows/ci.yml`'s `test` job: replace `cargo build` → `just build`;
  `cargo run --bin messgr-control -- migrate` → `just control-migrate`; the inlined
  `docker exec` tablespace block → `just tablespace-init ${{ job.services.postgres.id }}`; the
  inlined Vault-mount/key/approle `curl` block → keep the existing health-poll loop (CI-specific,
  waiting for the service container), then call `just vault-dev-init` for the actual
  mount/key/approle setup (its hardcoded `localhost:8200`/`messgr-dev-root-token` already match
  this job's `VAULT_ADDR`/`VAULT_TOKEN` env values exactly); `cargo test` → `just test`.

#### Task 7 — `src/stats.rs`: `StatsError` matching sibling shape (item 7)

Add, mirroring `provider_config::configure::ConfigureError` exactly:
```rust
#[derive(Debug)]
pub enum StatsError {
    Database(sqlx::Error),
}
// Display: "stats operation failed: {err}"; Error::source: Some(err); From<sqlx::Error>: Database
```
Change `tenant_message_stats`'s return type from `Result<Vec<ChannelStatusCount>, sqlx::Error>`
to `Result<Vec<ChannelStatusCount>, StatsError>` (the internal `sqlx::Error::Configuration`
unknown-slug signal is unchanged, just now wrapped). Update `src/bin/control.rs`'s `Command::Stats`
handler for the new error type (folds into Task 5's conversion at that call site).

### Acceptance test

```
just build
just lint
just test
just docs-check
```

Additionally, exercise each converted path directly:
- `RUST_LOG` unset, `just control-migrate` (or `cargo run --bin messgr-control -- migrate`):
  confirm `control database migrations applied` prints (Task 1).
- `DATABASE_MAX_CONNECTIONS=not-a-number cargo run --bin messgr-control -- migrate`: confirm a
  panic with a clear message, not a silent default (Task 2).
- `cargo run --bin messgr-control -- tenant-config set --tenant-slug x --retention-years -1 ...`:
  confirm clap rejects it before the subcommand runs (Task 4).
- `cargo run --bin messgr-control -- provider-config set --channel bogus ...`: confirm clap
  rejects it (Task 4).
- Trigger one converted subcommand failure (e.g. `producer register` against an unregistered
  tenant slug): confirm `error: ...` prints to stderr with the same message text a panic would
  have carried, exit code non-zero, no Rust backtrace noise (Task 5).
- Run the exact CI-equivalent sequence locally: `just build && just control-migrate && just
  tablespace-init && just vault-dev-init && just test`, confirming it's identical to what
  `.github/workflows/ci.yml`'s `test` job now runs (Task 6).

### Docs update (mandatory when user-facing)

None of the seven items change user-facing CLI syntax except Task 4's new validation
constraints (a negative retention-years or an invalid channel now fails at parse time with a
clap-generated message) — this is tightening, not adding, a documented contract, so no
`docs/user-manual/` change is required. `just docs-check` still run per the acceptance test to
confirm nothing broke.

### Finish (mandatory)

1. Acceptance test green; `just build`/`just lint`/`just test`/`just docs-check` clean.
2. No docs changes needed (above).
3. Write a summary: files touched per task, decisions honoured, anything deferred.
4. Suggest a Conventional Commit message, e.g.:
   ```
   fix(operability): convert CLI panics to reported errors, close validation gaps (T-025)
   ```
5. Tidy WIP commits into a small number of atomic, correctly typed/scoped commits before
   presenting (root-path child, rules §0/§4 item 7) — default to preserving them on merge
   (rebase/keep-history) rather than squashing.
6. Commit locally on the ticket branch. Publish only per commit policy (no push/MR without
   explicit user approval). Under `layout = "in-tree"`, before pushing verify the remote base is
   not behind (`git fetch origin main && git diff --name-only origin/main...HEAD | grep
   '^tickets/'` must print nothing) — then push and open the merge request. Hand back to the
   user.

## Review

- [x] Reviewer independence settled (step 0): **independent** — fresh session, no memory of authoring `feat/T-025-operability-and-error-handling-cleanup-across-cli-and-services`; audits run directly, not delegated.
- [x] Implementation audit — acceptance test re-run, tasks & criteria verified (steps 1, 2)
- [x] Quality audit (step 3)
- [x] Consistency audit (step 4)
- [x] Documentation audit (step 4a) — no docs ship for this ticket's changes (tightening, not adding, a documented contract); `just docs-check` clean
- [x] Docs-readability pass (step 4b) — **conscious skip**: no `.adoc`/`.md` files changed by this ticket
- [x] Findings recorded with severity, class, disposition (step 5)
- [x] Ticket moved (step 6)
- [x] Other references / governing documents reconciled — nothing in `DESIGN.md` or `AGENTS.md` asserts anything this branch changed; no reconciliation needed (step 7)
- [x] Remaining-tickets impact sweep done (step 8) — `T-031`/`T-032` (`spawned-by: [T-025]`) checked; neither depends on any behavior this ticket touched
- [x] Summary + commit message & MR attributes presented for approval (step 9)

**Implementation audit.** `just build`, `just lint` (matches CI's `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt --all -- --check` exactly), `just test` (all 155 tests across every suite, 0 failed), `just docs-check` all green on `feat/T-025-operability-and-error-handling-cleanup-across-cli-and-services`. Every one of the ticket's manual acceptance steps re-run directly against live Postgres/Vault containers and confirmed:
- Task 1: `RUST_LOG` unset, `messgr-control migrate` prints `control database migrations applied`.
- Task 2: `DATABASE_MAX_CONNECTIONS=not-a-number messgr-control migrate` panics with `DATABASE_MAX_CONNECTIONS must be a valid u32, got "not-a-number"`, not a silent default.
- Task 4: `tenant-config set --retention-years -1` and `provider-config set --channel bogus` both rejected by clap before the subcommand body runs (`invalid value 'bogus' ... [possible values: sms, email, whatsapp]`).
- Task 5: `producer register` against an unregistered tenant slug prints `error: failed to register producer ...: no tenant registered with slug "does-not-exist"` to stderr, exit code 1, no panic/backtrace.
- Task 6: `just vault-dev-init`/`just tablespace-init` run clean against the local containers, byte-for-byte the same calls CI's `test` job now makes.
- Task 7: `messgr-control stats --tenant-slug does-not-exist` reports `stats operation failed: error with configuration: ...`, matching `ConfigureError`'s exact shape/message convention.

All 7 tasks done in the files the plan names. All 5 confirmed decisions honoured — verified directly against the diff: `KeyStoreError` widened to the enum exactly as specified with no caller pattern-matching its internals; the `refresh_draining.write().expect(...)` exception in `src/bin/dispatcher.rs` untouched; no new dependency added (`Cargo.toml`/`Cargo.lock` unchanged); `main()`/`run()` split matches decision 5's shape verbatim, including the three `VaultKeyStore::connect(...).expect(...)` call sites inside `Provision`/`DevPki`/`CustomerDek` staying panics per decision 2's "connecting to Vault is startup-class regardless of call site."

**Quality/consistency audit.** Every converted panic→`Err` preserves its original message text (spot-checked against `main`'s pre-ticket text for every site). Task 4's new clap-rejection tests are real negative tests — they assert `result.is_err()` on a parse that must fail, which can and would go red if the `value_parser`/`PossibleValuesParser` were removed (addendum step 3's mutation-test bar). `src/bin/dispatcher.rs`/`src/bin/ingest.rs` correctly untouched outside `keystore.rs`/`stats.rs` (both binaries' own panics are all in their startup sequences per decision 2, out of scope). No new table, column, or secret-handling surface — addendum step 2 items 2, 4, 5, 6 don't apply to this diff. `justfile`'s `vault-dev-init` hardcoded `localhost:8200`/`messgr-dev-root-token` confirmed to exactly match `ci.yml`'s `test` job env (`VAULT_ADDR`/`VAULT_TOKEN`).

| id | severity | class | disposition | description | evidence | suggestion |
|---|---|---|---|---|---|---|
| F1 | non-blocking | other | note and close | Task 5's plan prose says "the four `#[cfg(test)]`-module `.expect()`/`panic!()` clap-invariant sites are left untouched," but there are five (`src/bin/control.rs` pre-ticket lines 932, 957, 968, 1052, 1067 on `main`, unchanged by this ticket). Zero behavior impact — the implementation correctly left all five alone, matching the stated intent; only the plan's own count is off by one. | `src/bin/control.rs` test module, `git show main:src/bin/control.rs \| grep -n "panic!\|\.expect("` | Nothing to fix in code. If anyone revisits this ticket's plan text, correct "four" to "five." |
| F2 | non-blocking | test-gap | note and close | The new `KeyStoreError::Protocol` paths added in Task 3 (malformed Vault response: missing plaintext in `create_dek`, invalid base64 in `create_dek`/`unwrap_dek`, an unbuildable `VaultClientSettings` in `connect_settings`) have no test exercising them — a mutation deleting the `.map_err`/`.ok_or_else` conversion would not be caught by any red test. | `src/keystore.rs:207-211,258-263,266-271,288-293`; `tests/keystore.rs` only exercises the `Client` variant (real Vault-rejection paths) | Addendum step 3 frames this class of gap as advisory, not blocking. Real dev-mode Vault always returns well-formed base64/a present plaintext, so triggering these paths needs fault-injection infra this codebase doesn't have yet — building it is disproportionate to this ticket's mechanical scope. Leave as noted; revisit if a real malformed-response incident ever occurs. |

Disposition summary: 2 non-blocking findings, both **note and close** (F1, F2). 0 blocking.

cost: estimated M, actual M

## History

- 2026-09-02 — created (TO DO). source: audit: batches nine noted operability findings from prior ticket reviews (T-001/F7,F9,F12; T-003/F2; T-007/F2; T-012/F1) with two found directly by the 2026-09-02 design/implementation audit (TenantRegistry has no eviction; no health endpoint on any binary), plus CI inlining cargo/docker rather than calling just recipes.
- 2026-09-05 — TO DO → READY: plan complete. Item 1 dropped (already fixed); items 7/8 split to T-031/T-032; re-graded complexity low → medium.
- 2026-09-05 — TO DO → READY: plan complete
- 2026-09-05 — READY → IN DEVELOPMENT: picked up
- 2026-09-06 — IN DEVELOPMENT → IN REVIEW: acceptance green, all 7 tasks done as planned.
- 2026-09-06 — IN DEVELOPMENT → IN REVIEW: acceptance green
- 2026-09-07 — IN REVIEW → DONE: review clean: 2 non-blocking findings, both note-and-close; acceptance green
- 2026-09-07 — MERGED: PR #38 (`5beb467`) into `main`
