---
id: T-010
title: Template store: immutable versioned templates, approval metadata, render path
project: messgr
depends-on: [T-009]
spawned-by: []
family: T-007
impact: medium
complexity: medium
cost: M
---

# T-010 — Template store: immutable versioned templates, approval metadata, render path

## Outcome

After this ships, an operator can approve a versioned, locale-specific template body through
`messgr-control template approve`, and any Rust caller (T-011's ingest path, first) can render
one against a variables map with `{{key}}` substitution — producing the exact text a compliance
review can reproduce for a given `template_id`/`version` years later.

## Description

Build the template store per design §4.4: immutable `(template_id, version, locale)` rows with
mandatory approval metadata, plus a render path that substitutes `{{key}}` placeholders in a
template body against a caller-supplied variable map. Ships as its own `messgr-control template`
CLI (approve/show/list/render), following the T-005/T-007 precedent of a bare operator-driven
surface — the audited, role-gated admin UI (§11.1, §11.3) is `T-042`'s scope, not this one's. Part
of the step-2 ticket family (`family: T-007`; see T-007).

`depends-on: [T-009]` is a sequencing/family dependency, not a functional one, re-verified during
this refinement: this ticket's migration creates only the `template` table and never reads or
writes `comms_request`. The `template_id`/`template_version` columns that pin a version onto the
ledger row already exist — T-009 added them, with no `REFERENCES` clause (T-009 decision 1) — so
"version pinning onto the ledger row" is already shipped; this ticket supplies the table that
column *names*, not a change to it. T-009 is `6-done/` and merged either way, so the dependency is
satisfied regardless.

Consent (`T-020`) and suppression (`T-021`) — also documented in §4.4 — remain out of scope, as
T-009 already noted.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .
git checkout main
git checkout -b feat/T-010-template-store
```

Root-path child (`path = "."`, pickle.toml): WIP commits encouraged during the work, then
interactive-rebased into atomic, correctly scoped commits before the summary is presented (rules
§0). Do not push and do not open a merge request without explicit user approval. Ticket and board
bookkeeping is committed on `main`, never on this branch.

### Prerequisite gate (hard)

- `T-009` is in `6-done/` and merged to `main` (PR #11, `65c5e70`) — confirmed above: not a
  functional prerequisite for this ticket's own migration, but satisfied regardless.
- Clean working tree before branching.
- Local stack up: `just db-up`, then `just control-migrate`, then `just vault-dev-init` — the
  integration tests provision real tenants.

### Confirmed design decisions (do not deviate without asking)

1. **Schema matches DESIGN.md §4.4's `template` `CREATE TABLE` verbatim.** No `tenant_id` column
   (the table lives in the tenant database, §2.1, matching T-007/T-009 precedent), no
   `REFERENCES`/`CHECK` beyond what the design's own snippet declares, no `status`/draft column —
   every row this table ever holds is already approved (`approved_by`/`approved_at` are
   `NOT NULL`), so there is no pending-approval state to model.
2. **Immutability is enforced by application code, not a database trigger or `REVOKE UPDATE`.**
   No `UPDATE`/`DELETE` statement is ever written against `template` in `repo.rs`; a content
   change is always a new row with `version` incremented. No other table in this codebase guards
   immutability at the role/grant level, and adding the first one here would be new machinery
   this ticket doesn't need (boring technology).
3. **Render placeholder syntax: literal `{{key}}` tokens, substituted by plain string scanning —
   no templating crate.** `key` is matched with surrounding whitespace trimmed (`{{ key }}` also
   matches). A key present in the body but absent from the caller's variable map is a hard render
   error (`RenderError::MissingVariable`) — never silently left as literal `{{key}}` or blanked; a
   bank must not send customer-facing content with an unsubstituted placeholder. A key present in
   the variable map but never referenced by the body is silently ignored. An unterminated `{{`
   with no matching `}}` is also a hard error (`RenderError::UnterminatedPlaceholder`), not passed
   through as literal text.
4. **`channel` is validated at the CLI boundary against a closed set**, following
   `verification_mode`'s precedent (T-007 decision 5): `template::model::channel::{SMS, EMAIL,
   WHATSAPP}` — plain `&'static str` constants, no new Rust enum — matching DESIGN.md §4.4's own
   comment (`sms | email | whatsapp`); `clap`'s `PossibleValuesParser` rejects anything else
   before it reaches the database. `locale` stays unvalidated free text, matching
   `tenant_config.default_locale`'s precedent — DESIGN.md never enumerates a closed locale set.
5. **Approval is atomic at creation — there is no separate "create" step.**
   `messgr-control template approve` is the only way a row is ever written; it takes the body
   directly via `--body-file` (bodies can be multi-line) and stamps `approved_by = --actor`,
   `approved_at = now()`. Re-approving an existing `(template_id, version, locale)` is **rejected**
   — never idempotent or upserted — matching the design's own "changes create a new version"
   (§4.4): a real change goes through a new `version`.
6. **Every `approve` call, including a rejected one, writes exactly one `platform_audit` row**
   (`template.approve`), following the `producer.register`/`tenant_config.set` convention (T-005
   decision 4, T-007 decision 8) — outcome `created` or `rejected` (never `idempotent`, per
   decision 5).
7. **No admin UI, no role-based approval workflow, no separate audit-trail viewer.** DESIGN.md
   §11.1/§11.3 assigns "template approval" to the `admin` role's admin panel, and PLAN.md's T-042
   (`depends-on: [T-039]`) is explicitly that ticket. This one ships the schema, the render path,
   and the bare `messgr-control` CLI an operator drives directly — the same relationship T-005/
   T-007 have to their own eventual admin surfaces.

### Tasks

#### Task 1 — Tenant migration

Create `migrations/tenant/0005_template.sql`:

```sql
-- Template store (DESIGN.md §4.4, T-010): immutable versioned template
-- bodies with mandatory approval metadata. No tenant_id column, matching
-- 0002_tenant_config.sql / 0004_ledger_outbox_schema.sql (§2.1) -- the
-- tenant already is the database. No REFERENCES/CHECK beyond what the
-- design's own CREATE TABLE declares. No status/draft column -- every
-- row is already approved (approved_by/approved_at NOT NULL); see T-010
-- decision 1.

CREATE TABLE template (
    template_id text        NOT NULL,
    version     int         NOT NULL,
    channel     text        NOT NULL,
    locale      text        NOT NULL,
    body        text        NOT NULL,
    approved_by text        NOT NULL,
    approved_at timestamptz NOT NULL,
    PRIMARY KEY (template_id, version, locale)
);
```

No new runner needed — `provision_tenant` already runs `sqlx::migrate!("./migrations/tenant")`
against every tenant; an existing dev tenant picks this up on its next (idempotent) re-provision.

#### Task 2 — Model

Add `src/template/model.rs`:

- `Template` — `#[derive(Debug, Clone, sqlx::FromRow)]`, one field per column: `template_id:
  String`, `version: i32`, `channel: String`, `locale: String`, `body: String`, `approved_by:
  String`, `approved_at: chrono::DateTime<chrono::Utc>`.
- `pub mod channel { pub const SMS: &str = "sms"; pub const EMAIL: &str = "email"; pub const
  WHATSAPP: &str = "whatsapp"; }` per decision 4, following `src/tenant_config/model.rs`'s
  `pub mod verification_mode` shape.

#### Task 3 — Render

Add `src/template/render.rs`:

- `RenderError` enum — `MissingVariable(String)`, `UnterminatedPlaceholder(String)` — with
  `Display`/`Error` impls, in the shape of `ProducerError`.
- `pub fn render(body: &str, variables: &std::collections::HashMap<String, String>) ->
  Result<String, RenderError>` implementing decision 3's scan: repeatedly find the next `{{`,
  copy everything before it verbatim, find the matching `}}` (error `UnterminatedPlaceholder` if
  none), trim the key between them, look it up in `variables` (error `MissingVariable` if absent),
  append the value, and continue scanning after the closing `}}`; append whatever remains once no
  more `{{` is found.
- Unit tests in `#[cfg(test)] mod tests`: no placeholders returns the body unchanged; a
  `{{ key }}` with surrounding whitespace resolves; an unreferenced extra key in `variables` is
  ignored; a missing key returns `MissingVariable`; an unterminated `{{` returns
  `UnterminatedPlaceholder`.

#### Task 4 — Repository

Add `src/template/repo.rs`, following `src/producer/repo.rs`'s shape:

- `pub async fn find(pool: &PgPool, template_id: &str, version: i32, locale: &str) ->
  Result<Option<Template>, sqlx::Error>` — `SELECT` by the primary key, `fetch_optional`.
- `pub async fn list_versions(pool: &PgPool, template_id: &str) -> Result<Vec<Template>,
  sqlx::Error>` — `SELECT ... WHERE template_id = $1 ORDER BY version, locale`.
- `pub async fn insert(pool: &PgPool, template_id: &str, version: i32, channel: &str, locale:
  &str, body: &str, approved_by: &str, approved_at: chrono::DateTime<chrono::Utc>) ->
  Result<(), sqlx::Error>` — not itself idempotent (a second insert for the same primary key
  violates it); `approve_template` (Task 5) is what makes the overall operation reject cleanly,
  matching `producer::repo::insert`'s documented convention.

#### Task 5 — Approve, show, list, and render-preview operations

Add `src/template/approve.rs`, following `src/tenant_config/configure.rs`'s resolve-then-connect
shape:

- `ApproveError` enum — `Database(sqlx::Error)`, `Rejected(String)`, `Render(RenderError)` — with
  `Display`/`Error`/`From<sqlx::Error>` impls, in the shape of `ProducerError`.
- `pub struct ApproveOutcome { pub outcome: &'static str }` — always `"created"` (rejection is
  returned as `Err`, per decision 5; there is no `"idempotent"`).
- `pub async fn approve_template(control_pool: &PgPool, base_db_url: &str, tenant_slug: &str, template_id: &str, version: i32, channel: &str, locale: &str, body: &str, profile: Profile, actor: &str) -> Result<ApproveOutcome, ApproveError>`:
  resolve `tenant_slug` via `tenant::repo::find_by_slug` (audit `rejected`, `tenant_id: None`, on
  `None`); connect the tenant pool; `repo::find` the primary key — `Some` audits `rejected` and
  returns `Err(Rejected(...))` ("already approved; approve a new version instead"); `None` calls
  `repo::insert` with `approved_by = actor`, `approved_at = Utc::now()`, audits `created`, returns
  `Ok`. Close the tenant pool before returning, matching `register_producer`'s shape.
- `pub async fn show_template(...) -> Result<Option<Template>, ApproveError>` — resolve, connect,
  `repo::find`, close; mirrors `show_tenant_config`.
- `pub async fn list_template_versions(...) -> Result<Vec<Template>, ApproveError>` — resolve,
  connect, `repo::list_versions`, close; mirrors `list_producers`.
- `pub async fn render_preview(..., variables: &std::collections::HashMap<String, String>, ...) ->
  Result<String, ApproveError>` — resolve, connect, `repo::find` (`Err(Rejected("no such
  template_id/version/locale"))` on `None`), call `render::render` mapping `RenderError` through
  `ApproveError::Render`, close.
- `async fn audit(...)` helper writing one `platform_audit` row per decision 6, in the shape of
  `producer::register::audit` (detail JSON: `template_id`, `version`, `channel`, `locale`,
  `outcome`).

#### Task 6 — Wiring

Add `src/template/mod.rs` (`pub mod approve; pub mod model; pub mod render; pub mod repo;`) and
register `pub mod template;` in `src/lib.rs`.

#### Task 7 — `messgr-control template` subcommands

Extend `src/bin/control.rs` with a `Template { command: TemplateCommand }` variant (added after
`CustomerDek`, matching Task 6's registration order):

- `Approve { tenant_slug, template_id, version: i32, channel: String, locale: String, body_file:
  PathBuf, actor: String }` — `--channel` uses `clap::builder::PossibleValuesParser::new([SMS,
  EMAIL, WHATSAPP])` per decision 4; reads `body_file` via `std::fs::read_to_string`; calls
  `approve_template`; prints `outcome=<outcome>`.
- `Show { tenant_slug, template_id, version: i32, locale: String }` — calls `show_template`;
  prints each field on success, or `not found` on `None`.
- `List { tenant_slug, template_id }` — calls `list_template_versions`; prints one line per
  `(version, locale, channel, approved_by, approved_at)`.
- `Render { tenant_slug, template_id, version: i32, locale: String, var: Vec<String> }` —
  `--var` repeatable, each in `key=value` form, parsed with `str::split_once('=')` into a
  `HashMap`; calls `render_preview`; prints the rendered body.

Add a unit test (`Cli::try_parse_from`) asserting an invalid `--channel` value fails to parse
(decision 4), matching `verification_mode`'s existing test convention.

#### Task 8 — Integration tests

Add `tests/template.rs`, following `tests/tenant_config.rs`'s conventions exactly (`unique_name`,
real provisioning via `provision_tenant`, `drop_test_tenant`-style best-effort cleanup). Cover:

1. `approve_template` creates a row; `show_template` round-trips every field, including
   `approved_by`/`approved_at`.
2. Re-`approve_template` for the same `(template_id, version, locale)` is rejected, and writes a
   `template.approve`/`rejected` `platform_audit` row (T-005/F1 pattern, asserted from the start).
3. `list_template_versions` returns every version/locale for a `template_id`, ordered by version.
4. `render_preview` with every placeholder supplied substitutes correctly, including a
   `{{ key }}` with surrounding whitespace.
5. `render_preview` with a missing variable returns `ApproveError::Render(RenderError::
   MissingVariable(_))`, and the caller never sees a partially-substituted string.
6. `approve_template` against an unknown `--tenant-slug` is rejected and writes a
   `template.approve`/`rejected` `platform_audit` row with `tenant_id IS NULL`.

### Acceptance test

```
just db-up
just control-migrate
just vault-dev-init
just fmt
just lint      # cargo clippy -- -D warnings, must be clean
just test      # cargo test, all green including tests/template.rs, render.rs's unit tests, and the new control.rs unit test
```

Then exercise the CLI end to end against a real tenant:

```
just provision acme eu tenant_acme operator@example.com
printf 'Hi {{ name }}, your balance is {{ balance }}.' > /tmp/t010-body.txt
cargo run --bin messgr-control -- template approve --tenant-slug acme \
    --template-id balance-alert --version 1 --channel sms --locale en-GB \
    --body-file /tmp/t010-body.txt --actor operator@example.com
cargo run --bin messgr-control -- template show --tenant-slug acme \
    --template-id balance-alert --version 1 --locale en-GB
cargo run --bin messgr-control -- template list --tenant-slug acme --template-id balance-alert
cargo run --bin messgr-control -- template render --tenant-slug acme \
    --template-id balance-alert --version 1 --locale en-GB \
    --var name=Jordan --var "balance=£120.00"
```

Expected: `approve` prints `outcome=created`; `show` prints all seven fields; `list` shows the one
row; `render` prints `Hi Jordan, your balance is £120.00.`. Re-running the identical `approve`
command exits non-zero ("already approved").

Verify the audit trail:

```
psql postgres://messgr:messgr@localhost:5432/control \
     -c "SELECT action, detail->>'outcome' FROM platform_audit WHERE action = 'template.approve' ORDER BY at"
```

Expected: a `created` row, then a `rejected` row from the repeat-`approve` step above.

### Docs update (mandatory when user-facing)

User-facing surface: the new `messgr-control template` subcommands.

- `README.md` — add a "### Templates" section after "### Customer DEK pre-provisioning"
  (matching Task 7's command registration order), documenting `template approve|show|list|render`,
  the immutable-per-version shape, the `{{key}}` render syntax and its hard-fail-on-missing-
  variable behaviour, and that the audited role-gated admin approval workflow is `T-042`'s scope.
- `justfile` — add `template-approve`/`template-show`/`template-list`/`template-render` recipes in
  the `control-plane` group, mirroring `producer-register`/`tenant-config-set`. `template-render`
  accepts a single `--var` pair as a positional parameter, with a comment (matching
  `customer-dek-pre-provision`'s existing precedent) directing multi-variable renders to invoke
  `cargo run --bin messgr-control -- template render` directly with repeated `--var` flags.
- `DESIGN.md` §4.4 — add one short paragraph after the existing "Templates are immutable..."
  paragraph documenting the render placeholder syntax (`{{key}}`, whitespace-trimmed) and the
  hard-fail-on-missing-variable behaviour (decision 3) — this is new information the `CREATE
  TABLE` snippet alone doesn't carry, not a correction of anything already written.

### Finish (mandatory)

1. Acceptance test green; `just fmt`, `just lint`, `just test` all clean.
2. README, justfile, and DESIGN.md §4.4 updated per the docs step.
3. Write a summary: files touched, decisions honoured, anything deferred.
4. Suggested Conventional Commit message:

   ```
   feat(template): add template store, render path, and CLI (T-010)

   Adds the fifth tenant migration (DESIGN.md §4.4): the immutable,
   versioned template table with mandatory approval metadata. Ships a
   render path ({{key}} placeholder substitution, hard error on a
   missing variable), a repository, approve/show/list/render
   operations, and a messgr-control template subcommand an operator
   drives directly -- platform-wide role-based approval and an admin
   UI are T-042's scope. Every approve call, including a rejected
   re-approval attempt, writes a platform_audit row.
   ```

5. Root-path child: interactive-rebase WIP commits into a small number of atomic, correctly
   scoped commits (migration / model+render / repo+approve / CLI / tests / docs is a natural
   split) before presenting them.
6. Commit locally on the ticket branch. Do **not** push or open a merge request without explicit
   user approval. On approval, keep the tidied history (root-path default), verify
   `git fetch origin main && git diff --name-only origin/main...HEAD | grep '^tickets/'` prints
   nothing (in-tree layout, rules §0), then push and open the merge request. Merging is the
   human's.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-08-31 — created (TO DO). source: chat: filed from PLAN.md's build-step-2 decomposition; member of the step-2 ticket family (umbrella T-007)
- 2026-08-31 — TO DO → READY: plan complete
