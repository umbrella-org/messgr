# Delivery receipts (§10)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 10. Delivery receipts

Provider webhooks arrive duplicated, out of order, and occasionally **before** the sending transaction has committed. The design must tolerate all three:

- `comms_event` has a natural-key unique constraint; inserts use `ON CONFLICT DO NOTHING`. Duplicates are free.
- Events are keyed on `provider_ref` and never assume ordering. A `delivered` arriving before a `sent` is recorded as-is; the UI renders by `occurred_at`, not arrival order.
- A receipt for an unknown `provider_ref` goes to a small `orphan_event` table and is reconciled on a short delay rather than discarded.

**Tenant routing must not use the tenant slug.** A callback URL is configured in the provider's console and is effectively public; `/webhook/acme/twilio` lets anyone enumerating paths discover which institutions are customers. Route on an **opaque per-tenant webhook token** instead — `/webhook/7f3a…/twilio` — rotatable independently of the tenant slug, and carrying no information if leaked.

**Network placement:** the webhook receiver is the only internet-facing component in an otherwise internal system. It runs as a separate minimal binary in the DMZ, does provider signature verification and nothing else, and writes through a narrowly-scoped DB path. This is a firewall and network-segmentation conversation to have with infrastructure early — it is usually the longest-lead item in an on-prem deployment.

**Constraint this path must satisfy, mechanism not yet chosen.** A delivery receipt's raw provider JSON is stored encrypted under the customer's DEK (§4.4), which means `messgr-webhook` — the one DMZ-facing binary, deliberately kept to "signature verification and nothing else" — needs some way to get that encryption done. Holding a Vault AppRole with decrypt/encrypt policy for every tenant mount directly in the DMZ binary would make the platform's most exposed process also its widest keyholder, the opposite of "narrowly-scoped". Deferring encryption to an internal process instead means the raw payload crosses the DMZ boundary and sits briefly unencrypted somewhere on the internal side before that process picks it up — survivable if that window is short and the interim store is itself access-controlled, but it is a real design choice with a real exposure window, not a detail. Which of these (or another shape) is correct needs answering before step 12, not assumed by silence.

---

