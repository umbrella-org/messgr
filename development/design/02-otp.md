# The OTP question (§3)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 3. The OTP question

**Auth SMS does not go through the queue.**

The obvious design — one pipeline, priority column, OTP marked P0 — puts customer login on the critical path of a system whose other job is bulk marketing. A campaign bug, a bad migration, or dispatcher lock contention would then prevent customers from logging in. A dedicated worker pool addresses head-of-line blocking but not shared fate, and a polling dispatcher imposes a latency floor equal to the poll interval.

Instead:

- Auth services call `sms-sender`, a thin Rust library (or a dedicated single-purpose HTTP service, if the callers are not Rust) that talks to the SMS provider **synchronously**.
- The audit record is written to `comms_request` **asynchronously and best-effort**. If Postgres is unavailable, the send still succeeds and the record is buffered to local disk and backfilled.
- Quiet hours are not evaluated on this path at all — OTP is exempt by policy.

This preserves the single pane of glass (every OTP still appears in messgr's ledger and UI) while removing messgr from the availability path of authentication.

**Cost of this choice:** OTP rate limiting and provider failover are duplicated in `sms-sender` rather than centralized. That is the correct trade — a small amount of duplicated logic in exchange for decoupling tier-0 auth from a tier-1 batch system.

### 3.1 The cloud variant

On-prem, `sms-sender` is a library linked into the bank's own auth service — same process, no network hop, and the availability argument above holds exactly.

In cloud that is not available: the tenant's auth service is on their infrastructure, and a library cannot hold their provider credentials or reach a Vault mount across the boundary. The cloud deployment therefore exposes **`otp-api`, a dedicated minimal endpoint per region**:

- Its own binary and its own process pool, sharing nothing with `ingest-api` or the dispatchers.
- Synchronous: authenticate the tenant by mTLS, look up the provider credential, call the provider, return. No queue, no gate chain, no Postgres write on the request path.
- The audit record is written asynchronously and best-effort, exactly as on-prem — a Postgres outage delays the log, never the OTP.
- Independently deployable and independently scalable, so a messgr release never touches the auth path without an explicit decision to do so.

**Correction: "look up the provider credential" is doing the same job Vault-independence needs for the message path, and it was never given the same mechanism.** §2.4 and the end of §3.1 claim OTP has no dependency on Vault being reachable — but `otp-api`'s provider credential comes from Vault (§13: "all secrets come from Vault"), and unlike a message's DEK, no cache, TTL, or pre-provisioning is specified for it here. As written, "look up the provider credential" reads as a per-request or startup-time Vault call with nothing said about what happens when Vault is sealed. Fixed by stating the same discipline §7.6 already applies to DEKs: `otp-api` fetches its provider credential from Vault at startup and holds it in memory for the life of the process, refreshed on a background timer rather than per request, so a sealed or unreachable Vault degrades nothing on the OTP request path — it only delays picking up a credential *rotation* until Vault recovers.

This is weaker than the on-prem story — a network hop and a shared regional service now sit between the tenant and their SMS provider, where on-prem there was neither. It is worth being explicit with cloud tenants about that difference rather than presenting the two deployments as equivalent. Tenants for whom OTP latency and availability are paramount should be told they can keep auth on-premise while using the cloud service for everything else; the ledger accepts backfilled auth records from either source.

---

