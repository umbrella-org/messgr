# Throughput and campaign traffic (§8)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 8. Throughput and campaign traffic

The average rate (5M/day ≈ 58/sec) is not the design target and should not be used for sizing. Marketing is bursty: a 2M-recipient campaign submitted as a batch is ~1600/sec sustained if drained over 20 minutes.

Two consequences:

**Bulk ingestion path.** `POST /comms/bulk` accepts a newline-delimited JSON stream and lands it via `COPY` into a staging table, then a single set-based INSERT into ledger and outbox. Submitting 2M individual POSTs is not supported and would not survive contact with the network.

**Constraint this path must satisfy, mechanism not yet chosen.** `COPY` writes `payload_ciphertext` and `destination_ciphertext` directly — both must already be encrypted under the right per-customer DEK by the time a row reaches the staging table, per hard invariant 7 (every payload written under a customer-scoped key, no exceptions for volume). A 2M-recipient campaign can span up to 2M distinct customers, so the batch may need up to 2M distinct DEK unwraps before or during staging — an order of magnitude past what §7.6's steady-state cache (sized and pre-provisioned for ordinary per-message traffic) was reasoned about. Whatever the bulk path does — pre-provision DEKs ahead of the batch, warm the cache from the campaign's recipient list before the `COPY` starts, or something else — it must be decided with this number in view, not discovered when the first real campaign is slow or a Vault mount starts throttling. Mechanism is step 18's problem; this paragraph exists so it isn't invisible until then.

**Preemption, and nothing more.** An earlier version of this section gave marketing "a configured fraction of each channel's rate budget". That is a second mechanism doing the same job as `ORDER BY priority` in the claim query (§4.2) — transactional rows are already claimed before marketing rows, which *is* preemption. The fraction added a tunable that nobody would tune and whose realistic failure mode is marketing starvation, the opposite of the problem it was written for.

Cut. The claim ordering is the whole mechanism: a campaign drains with whatever capacity transactional traffic leaves, and a fraud alert never waits behind one. If marketing starvation becomes real, the fix is a floor on marketing throughput — the inverse knob, added with evidence.

Note also that "admission" now means exactly one thing in this document: the ingest-side rate limit of §5.1. It is not reused for dispatcher-side scheduling.

---

