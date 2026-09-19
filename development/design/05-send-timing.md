# Send timing (§6)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 6. Send timing

### 6.1 Quiet hours

Three failure modes the naive implementation hits:

1. **Thundering herd.** Rescheduling every suppressed message to exactly `quiet_end` wakes an entire region's backlog at 07:00:00.000. Reschedule to `quiet_end + random_jitter(0, 30min)`. Plain uniform jitter — an earlier draft weighted it so transactional landed early in the window and marketing late, which is a knob nobody will tune and which duplicates the `ORDER BY priority` preemption already in the claim query (§4.2).
2. **Unknown timezone.** Null or missing customer timezone must fall back to an explicit institution default, configured per deployment — never to server-local time, and never to "send anyway".
3. **DST.** A message scheduled for 02:30 local on a spring-forward night refers to a moment that does not exist. Store everything in UTC, resolve windows with a real tz database (`chrono-tz`), and on a non-existent local time, round forward to the next valid instant.

```
policy: quiet_hours(region|segment) -> (start_local, end_local, tz)
resolve: customer tz -> segment policy -> institution default
```

**Correction: the `segment`/`region` scopes above are currently unreachable.** No customer
segment attribute exists anywhere in the data model — §1 puts audience segmentation out of
scope by design ("upstream systems decide who gets what and why") — and `region` (§2.2) is a
deployment-topology concept already 1:1 with a tenant's own database, not a per-customer field a
policy lookup could key on. A `scope = 'segment'` or `scope = 'region'` row is representable in
`quiet_hours_policy` (§4.10) but nothing in the data model can ever resolve a customer to one.
T-043 ships the table with its full `scope`/`scope_key` shape, so a future ticket can light up
segment/region resolution once an upstream system actually supplies that identity, but its
resolver and its `messgr-control` surface only ever read/write `scope = 'default'`. The
effective resolution order today is `customer tz -> institution default`, not the three-tier
version above.

Auth class skips this evaluation entirely (§3).

### 6.2 Scheduled delivery

The mechanism is nearly free: `outbox.next_attempt_at` already drives when a message becomes claimable, so a scheduled send is one whose `next_attempt_at` starts in the future. Future-dated rows sit in the claim index but sort after `now()`, so they cost nothing to skip. No scheduler process, no cron, no second queue.

The API accepts two forms, because marketing wants the second one and only ever gets offered the first:

| Form | Meaning |
|---|---|
| `scheduled_for` (absolute, UTC) | send at this instant |
| `scheduled_local` (time + optional date) | send at this **customer-local** time, resolved via `customer.timezone` |

Local-time scheduling is what "send at 9am on Tuesday" actually means for a campaign spanning timezones. It resolves through the same tz machinery and non-existent-local-time handling as §6.1 — the DST edge case is identical and must not be solved twice.

**All gates run at dispatch, never at schedule time.** This is what makes scheduling safe rather than dangerous: a message queued three weeks ago is checked against consent, suppression, verification, quota, and kill switches *as they stand at the moment of sending*. A customer who opts out on Monday does not receive Tuesday's pre-scheduled marketing. This falls out of §5 already; it is worth stating because the obvious alternative — validating at submission — would create a large and silent compliance hole.

**Cancellation is mandatory, not optional.** Anything schedulable must be cancellable: the offer was pulled, the account closed, the campaign was wrong. `DELETE /comms/{id}` sets `outbox.cancelled_at`. The race is real — a cancel can arrive while the dispatcher holds the lease — so the dispatcher **re-reads `cancelled_at` immediately before the provider call**, and the API returns `409 Already Sent` when it lost. Reporting a cancellation that did not happen is worse than failing to cancel.

**`expires_at` — drop rather than send late.** If dispatch is stalled (provider outage, kill switch, quota hold) a scheduled message can come due hours after it mattered. "Your flash sale ends at noon" arriving at 3pm is worse than silence. `expires_at` is checked first in the gate chain (§5) and produces a terminal `expired` state that is visible in reporting rather than a silent drop. Optional, but strongly recommended for anything time-bound.

**Maximum horizon, default 90 days.** Unbounded scheduling is a slow-acting footgun: a message scheduled two years out will fire against a template that has since been superseded, for a product that may be withdrawn, to a customer who may have left. Requests beyond the horizon are rejected unless the producer holds an explicit override.

**Outbox growth.** Scheduled messages occupy the outbox for their full delay, which is in tension with §4.2's "small and ephemeral" premise. At realistic ratios (scheduled volume is a small fraction of immediate) this is comfortable, and future-dated rows are inert. Monitor outbox row count as a first-class metric; if far-future scheduling ever becomes a bulk pattern, move rows beyond ~7 days into a separate `scheduled` table promoted nightly. Do not build that until the metric says so.

### 6.3 Precedence when timing rules collide

| Situation | Outcome |
|---|---|
| Scheduled time falls inside quiet hours | Quiet hours wins — deferred to window end + jitter. Scheduling is a producer convenience; quiet hours is a customer protection |
| Scheduled + `expires_at` before quiet-hours window ends | Message expires and is dropped. Correct: it cannot be sent legally *and* on time |
| Scheduled + kill switch active at due time | Held, subject to `expires_at` on release (§5.2) |
| Scheduled + quota exhausted at due time | Marketing defers to next window; transactional sends with an alert (§5.1) |
| Auth class | Never scheduled. Auth is synchronous by definition (§3); the API rejects `scheduled_for` on auth-class requests |

---

