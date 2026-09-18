---
id: T-041
title: Cancellation endpoint with dispatch-time race check
project: messgr
depends-on: []
spawned-by: []
impact: high
complexity: medium
cost: M
---

# T-041 — Cancellation endpoint with dispatch-time race check

## Outcome

After this ships, a producer can cancel a not-yet-sent message via `DELETE /comms/{id}`; if the
dispatcher already holds the lease and is about to send, the cancel loses and the caller gets
`409 Already Sent` instead of a false "cancelled" success.

## Description

Closes the cancellation half of build-order step 9 (§6.2: "Cancellation is mandatory, not
optional. Anything schedulable must be cancellable"). Verified in code: `outbox.cancelled_at`
exists as a column (`src/dispatcher/model.rs`) and is carried through every SELECT/INSERT
(`src/dispatcher/repo.rs`, `src/ingest/repo.rs`), but nothing in the codebase ever sets it to a
non-NULL value, and nothing branches on it — there is no cancel endpoint on `messgr-ingest`
(`src/bin/ingest.rs` registers only `POST /comms`), and the dispatcher's send path does not
re-check the column before calling the provider.

Per §6.2's exact mechanism: "`DELETE /comms/{id}` sets `outbox.cancelled_at`. The race is real —
a cancel can arrive while the dispatcher holds the lease — so the dispatcher **re-reads
`cancelled_at` immediately before the provider call**, and the API returns `409 Already Sent`
when it lost. Reporting a cancellation that did not happen is worse than failing to cancel." Both
halves (the endpoint and the dispatcher-side race check) are needed together — shipping the
endpoint alone would let a cancel silently lose the race and still report success.

Soft coupling: shares the outbox row and ingest/dispatcher code paths touched by T-040 (scheduled
delivery/expiry wiring) — no hard dependency, since cancellation of an immediately-claimable
message is meaningful on its own, but refining both together may be more efficient than
sequencing them if picked up close together.

## Implementation Plan

### 0. Feature branch (mandatory)

```
cd .   # messgr is the root-path child (path = "." in pickle.toml)
git checkout main
git checkout -b feat/T-041-cancellation-endpoint-with-dispatch-time-race-check
```

WIP commits locally as you go. Publish (push / MR) only after user approval, per the project's
commit policy — see Finish.

### Prerequisite gate (hard)

None. `depends-on: []`; branch cuts clean from `main`. T-040 (the soft coupling noted in the
Description) is merged, so no sequencing concern remains.

### Confirmed design decisions (do not deviate without asking)

1. **`DELETE /comms/{id}` is scoped to the creating producer** (user-confirmed during
   refinement). `outbox.producer_id`/`comms_request.producer_id` already record who created
   each row (used today for quota/kill-switch scoping, §5.1/§5.2); a request against another
   producer's id is treated identically to an unknown id — `404`, never `403` — so the endpoint
   leaks no information about whether an id belongs to another producer.
2. **An id not found in `outbox` is disambiguated via `comms_request.final_status`, which needs
   a new index** (user-confirmed during refinement). `outbox` rows are deleted the moment a
   message reaches a terminal state (`repo::write_terminal`), so "not in outbox" alone cannot
   tell "never existed / wrong producer" (`404`) apart from "already resolved" (`409`).
   `comms_request` is partitioned by `created_at` with no index on `id` alone (only
   `(customer_id, created_at)`, `(final_status, created_at)`, `(campaign_id, created_at)`,
   `(destination_hmac, created_at)`) — task 1 adds one. This also lays groundwork for §10's
   future `GET /comms/{id}`, which needs the identical id-only lookup.
3. **`final_status = 'cancelled'` on the fallback lookup still means success (`204`), not
   `409`** — it means a previous cancel already ran to completion (the dispatcher claimed the
   row, saw `cancelled_at` set, and terminal-wrote it), which is the same outcome this call was
   asking for. `409 Already Sent` is reserved for every other non-NULL `final_status` (`sent`,
   `failed`, `expired`, `suppressed_list`, `suppressed_consent`, `unverified_address`,
   `discarded`) — the message reached a terminal state this call did not cause and cannot
   undo. Using one status for every non-cancelled terminal reason (rather than a bespoke code
   per reason) matches the ticket's scope — only the dispatch-race case is named in the
   Outcome/§6.2.
4. **The dispatcher re-reads `cancelled_at` exactly once, immediately before the provider
   call — not as an earlier gate alongside Expiry/Suppression/Verification/Consent.** Those
   four run early specifically to skip wasted gate/decrypt/DEK work on a message already known
   dead; a cancellation re-check placed there would still miss the race §6.2 exists to close
   (`DELETE` can land at any point up to send, including after an early check runs). One check,
   placed last, is both correct and the minimum mechanism — matches §6.2's own wording
   ("re-reads `cancelled_at` immediately before the provider call") literally, and needs no
   second code path for the pre-existing-cancellation case since the same check catches both.
5. **Cancelling sets `outbox.cancelled_at` idempotently** (`COALESCE(cancelled_at, now())`) —
   a second `DELETE` on an already-cancelled, still-queued row returns `204` again rather than
   leaving the original `cancelled_at` unclear or erroring, matching `DELETE`'s expected HTTP
   idempotency.

### Tasks

#### Task 1 — Migration: index `comms_request(id)`

New file `migrations/tenant/0015_comms_request_id_index.sql`:

```sql
-- comms_request(id) index (DESIGN.md §6.2, §10, T-041) — comms_request is
-- partitioned by created_at with no index on id alone; DELETE /comms/{id}
-- (and the future GET /comms/{id}, §10) both look up by id only. Declaring
-- the index on the partitioned parent propagates it to every existing
-- partition and every partition T-014's create-ahead job creates later.
CREATE INDEX ON comms_request (id);
```

#### Task 2 — `IngestError` variants

`src/ingest/model.rs`: add `IngestError::CommsRequestNotFound` (→ `404`) and
`IngestError::AlreadySent` (→ `409`), with `Display` arms ("comms request not found" /
"comms request already reached a terminal state"), placed in `IntoResponse` next to the
existing `TemplateNotFound` (`404`) entry and as a new `409` arm respectively.

#### Task 3 — `repo::cancel`

`src/ingest/repo.rs`: add

```rust
pub enum CancelOutcome {
    Cancelled,
    AlreadySent,
    NotFound,
}

pub async fn cancel(
    pool: &PgPool,
    comms_request_id: Uuid,
    producer_id: Uuid,
) -> Result<CancelOutcome, sqlx::Error> {
    let matched: Option<Uuid> = sqlx::query_scalar(
        "UPDATE outbox SET cancelled_at = COALESCE(cancelled_at, now()) \
         WHERE comms_request_id = $1 AND producer_id = $2 \
         RETURNING comms_request_id",
    )
    .bind(comms_request_id)
    .bind(producer_id)
    .fetch_optional(pool)
    .await?;

    if matched.is_some() {
        return Ok(CancelOutcome::Cancelled);
    }

    let final_status: Option<Option<String>> = sqlx::query_scalar(
        "SELECT final_status FROM comms_request WHERE id = $1 AND producer_id = $2",
    )
    .bind(comms_request_id)
    .bind(producer_id)
    .fetch_optional(pool)
    .await?;

    Ok(match final_status.flatten().as_deref() {
        Some("cancelled") => CancelOutcome::Cancelled,
        Some(_) => CancelOutcome::AlreadySent,
        None => CancelOutcome::NotFound,
    })
}
```

(decisions 1–3 above; `final_status.flatten()` folds "no row at all" and the
contractually-impossible "row exists but `final_status` is still NULL and yet the row is gone
from `outbox`" into the same `NotFound`, since only the former is reachable in practice.)

#### Task 4 — `DELETE /comms/{id}` handler + route

`src/ingest/handler.rs`: add

```rust
pub async fn cancel_comms(
    State(_app): State<AppState>,
    producer: ProducerContext,
    Path(comms_request_id): Path<Uuid>,
) -> Result<StatusCode, IngestError> {
    match super::repo::cancel(&producer.tenant.pool, comms_request_id, producer.producer_id)
        .await?
    {
        CancelOutcome::Cancelled => Ok(StatusCode::NO_CONTENT),
        CancelOutcome::AlreadySent => Err(IngestError::AlreadySent),
        CancelOutcome::NotFound => Err(IngestError::CommsRequestNotFound),
    }
}
```

(`axum::extract::Path` needs importing; `CancelOutcome` re-exported or referenced via
`super::repo::CancelOutcome`, matching `InsertOutcome`'s existing pattern.)

`src/bin/ingest.rs`: add `axum::routing::delete` to the `use axum::routing::post;` import line,
and register the route:

```rust
let app: Router = Router::new()
    .route("/comms", post(create_comms))
    .route("/comms/{id}", delete(cancel_comms))
    .with_state(app_state);
```

(axum 0.8 uses `{id}`, matching DESIGN.md §10's own route-table notation for this exact path.)

#### Task 5 — Dispatcher: re-check `cancelled_at` before the provider call

`src/dispatcher/repo.rs`: add

```rust
/// Re-reads `outbox.cancelled_at` fresh from the DB (DESIGN.md §6.2, T-041)
/// — `ClaimedOutbox::cancelled_at` is a snapshot from claim time and
/// cannot see a cancel that landed afterward; this is the check that
/// closes the race between claim and send.
pub async fn is_cancelled(
    pool: &PgPool,
    comms_request_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cancelled_at IS NOT NULL FROM outbox WHERE comms_request_id = $1",
    )
    .bind(comms_request_id)
    .fetch_one(pool)
    .await
}
```

`src/dispatcher/worker.rs::try_process`: immediately before
`match ctx.sender.send(&destination, &body).await {`, add:

```rust
// Cancellation race check (DESIGN.md §6.2, T-041) — re-read cancelled_at
// immediately before the provider call, deliberately last: unlike the
// Expiry/Suppression/Verification/Consent gates above (which exist to skip
// wasted work on a message already known dead), this check exists to close
// the race window itself, so an earlier check would not be correct (decision 4).
if repo::is_cancelled(&ctx.pool, row.comms_request_id).await? {
    repo::write_terminal(
        &ctx.pool,
        row.created_at,
        row.comms_request_id,
        row.customer_id,
        "cancelled",
        None,
        None,
        "cancelled",
    )
    .await?;
    return Ok(());
}
```

Do not touch `src/dispatcher/drain.rs` — it reuses `try_process` via `process_one` (see that
module's existing doc comment), so both the ordinary claim loop and the release-drain path get
the same check for free.

#### Task 6 — Tests

`tests/ingest.rs` (`setup`/`register_test_producer`/`mtls_client` conventions; register the new
route on `TestServer`'s `Router` alongside the existing `POST /comms` one):
- `cancel_comms_sets_cancelled_at_and_returns_204`: `POST /comms`, then `DELETE /comms/{id}`
  with the same producer's client; assert `204`; assert `outbox.cancelled_at IS NOT NULL` for
  that row.
- `cancel_comms_is_idempotent`: `DELETE` the same id twice; both calls assert `204`.
- `cancel_comms_unknown_id_returns_404`: `DELETE /comms/{random UUID}`; assert `404`.
- `cancel_comms_wrong_producer_returns_404`: `POST` with producer A's client
  (`register_test_producer(&fixture, &vault, "producer-a")`); `DELETE` the resulting id with
  producer B's client (`register_test_producer(&fixture, &vault, "producer-b")`); assert `404`.
- `cancel_comms_after_dispatch_returns_409`: `POST` a message, then directly call
  `messgr::dispatcher::repo::write_terminal(..., "sent", ..., "sent")` against the outbox row
  written by that `POST` (same producer/tenant pool, matching this file's existing
  direct-repo-call convention for setting up dispatcher-side state) to simulate the dispatcher
  having already sent it; then `DELETE /comms/{id}`; assert `409`.

`tests/dispatcher.rs` (`write_ready_outbox_row`/`enforce_blocks_unverified_address_...`
conventions):
- `cancelled_row_is_terminal_written_and_not_sent`: write a ready outbox row via
  `write_ready_outbox_row`, directly `UPDATE outbox SET cancelled_at = now() WHERE
  comms_request_id = $1` (simulating a cancel that landed before the dispatcher claims it —
  the same code path also covers a cancel landing after claim, since the check re-reads fresh
  every time), claim it, call `try_process`, assert the written
  `comms_event`/`comms_request.final_status` is `"cancelled"` and the `Sender` was never
  invoked (mirrors T-040's `expired_row_is_terminal_written_and_not_sent`).

### Acceptance test

From the `messgr` root:

```
just build
just test      # includes the 6 new tests above; full suite must stay green
just lint       # cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings
just docs-check # snowball check — validates docs/user-manual/*.adoc
```

Manual smoke check (mirrors `docs/user-manual/ingest.adoc`'s existing curl examples): with a
tenant/producer provisioned and `messgr-ingest` running locally, `POST /comms`, then
`DELETE /comms/{id}` with the same client certificate and confirm `204` plus
`SELECT cancelled_at FROM outbox WHERE comms_request_id = ...` is non-NULL; repeat the `DELETE`
and confirm `204` again; `DELETE` a random UUID and confirm `404`.

### Docs update (mandatory when user-facing)

- `docs/user-manual/ingest.adoc`: document `DELETE /comms/{id}` — producer-scoped cancellation,
  `204`/`404`/`409` semantics, and the note that it only *requests* cancellation: the dispatcher
  makes the final call at send time (§6.2), so a `204` here does not itself guarantee the
  message never goes out.
- `docs/user-manual/dispatcher.adoc`: add a paragraph introducing the cancellation re-check
  (T-041, DESIGN.md §6.2) — placed after the existing Expiry/Suppression/Verification/Consent
  gate paragraphs, explicitly noting it runs last, immediately before the provider call, not
  alongside the other gates (decision 4).

### Finish (mandatory)

1. Acceptance test green; `just build`/`just test`/`just lint`/`just docs-check` clean.
2. Docs updated per above.
3. Write a summary: files touched, decisions made (the five above), anything deferred (`GET
   /comms/{id}` and the rest of §10's query API are out of scope — this ticket only adds the
   index that future endpoint will also need).
4. Suggest a Conventional Commit message, e.g.:

   ```
   feat(ingest,dispatcher): cancellation endpoint with dispatch-time race check (T-041)

   <body — what and why>
   ```

5. Tidy WIP commits into a small number of atomic ones (root-path child).
6. Commit locally; do not push or open an MR without user approval. Before pushing, verify
   `origin/main...HEAD` carries no `tickets/` path (in-tree layout). Present the commit message,
   then hand back.

## Review

<!-- empty until IN REVIEW -->

## History

- 2026-09-15 — created (TO DO). source: chat: filed from a build-order-vs-shipped-tickets gap
  analysis — `outbox.cancelled_at` exists but has no write path and is never checked before
  send, contradicting §6.2's "cancellation is mandatory" requirement.
- 2026-09-18 — TO DO → READY: plan complete
- 2026-09-18 — READY → IN DEVELOPMENT: picked up
- 2026-09-18 — IN DEVELOPMENT → IN REVIEW: acceptance green
