# Delivery receipts (§10)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 10. Delivery receipts

Provider webhooks arrive duplicated, out of order, and occasionally **before** the sending transaction has committed. The design must tolerate all three:

- `comms_event` has a natural-key unique constraint; inserts use `ON CONFLICT DO NOTHING`. Duplicates are free.
- Events are keyed on `provider_ref` and never assume ordering. A `delivered` arriving before a `sent` is recorded as-is; the UI renders by `occurred_at`, not arrival order.
- A receipt for an unknown `provider_ref` goes to a small `orphan_event` table and is reconciled on a short delay rather than discarded.

**Tenant routing must not use the tenant slug.** A callback URL is configured in the provider's console and is effectively public; `/webhook/acme/twilio` lets anyone enumerating paths discover which institutions are customers. Route on an **opaque per-tenant webhook token** instead — `/webhook/7f3a…/twilio` — rotatable independently of the tenant slug, and carrying no information if leaked.

**Network placement:** the webhook receiver is the only internet-facing component in an otherwise internal system. It runs as a separate minimal binary in the DMZ, does provider signature verification and nothing else, and writes through a narrowly-scoped DB path. This is a firewall and network-segmentation conversation to have with infrastructure early — it is usually the longest-lead item in an on-prem deployment.

**Encryption placement — resolved (T-047).** A delivery receipt's raw provider JSON is stored encrypted under the customer's DEK (§4.4), which means `messgr-webhook` — the one DMZ-facing binary, deliberately kept to "signature verification and nothing else" — needs some way to get that encryption done. Holding a Vault AppRole with decrypt/encrypt policy for every tenant mount directly in the DMZ binary would make the platform's most exposed process also its widest keyholder, the opposite of "narrowly-scoped"; a synchronous internal RPC to a separate encryptor service was also considered and rejected, since it would put a new internal dependency on the DMZ binary's hot path for no reduction in *which* process ends up holding every tenant's decrypt capability. The chosen shape instead: `messgr-webhook` writes the verified-but-unencrypted payload into a new, narrowly-scoped staging table, `webhook_receipt_staging`, in the target tenant's own database — its only SQL statement. A new internal-only `messgr-control webhook-promote` subcommand, invoked by cron (mirroring `orphan_reconcile`'s existing shape, never a long-lived listener), resolves the customer, encrypts under their DEK exactly as `orphan_reconcile`'s own promotion path already does, and writes the real `comms_event` row — or, if the `provider_ref` isn't known yet, hands the row to the existing `orphan_event` table (§4.4's own documented plaintext exception) for that reconciler to pick up on its own schedule. The cost: the raw payload sits unencrypted in `webhook_receipt_staging` — inside the tenant's own database, never in the DMZ — for as long as it takes the next `webhook-promote` run to pick it up, bounded by the cron cadence (start at every 1 minute) rather than open-ended.

---

