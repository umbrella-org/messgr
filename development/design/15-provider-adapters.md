# Provider adapters (§15)

[← DESIGN.md](../../DESIGN.md) · [development/README.md](../README.md)

## 15. Provider adapters

How messgr talks to every system that carries a message the last mile — porth for SMS, a vendor
for email and WhatsApp, and any channel added later — and what every such adapter must have in
common.

**Status: a proposal, opened 2026-09-29.** Only SMS is decided: it goes through porth (§2.5,
decision 34). The email and WhatsApp parts outline adapters for **Twilio SendGrid** (v3 Mail Send
API) and the **Meta WhatsApp Cloud API** used directly; both vendors are assumptions made for the
outline, and Still Open #4 stays open for email and WhatsApp until they are confirmed. Nothing in
this section is built until its decisions (§15.7) are taken and it is filed as tickets.

Vendor error codes, header names and payload shapes in this section come from the vendors'
public APIs as recalled while outlining, not from a fresh read of their documentation. Re-verify
each when the tickets are refined.

### 15.1 The adapter contract

Every adapter, for every channel and vendor, meets the same five-part contract. The contract is
what keeps a new channel — Telegram, say (§15.8) — from growing its own dispatch path, error
rules or receipt pipeline.

**1. Outbound: one trait, content by shape.** Decision 31 gave every channel the one `Sender`
trait, and that part stands. What changes is that `send(destination, body)` is too narrow for a
real vendor (§15.3, §15.4), so the trait takes a message whose content is an enum of **shapes**,
not of vendors:

```rust
pub enum Content {
    Text { body: String },                                     // SMS (porth), Telegram, ...
    Email { subject: String, body: String },
    Template { name: String, language: String, params: Vec<String> }, // WhatsApp
}

pub struct Outbound<'a> {
    pub destination: &'a str,
    pub content: &'a Content,
    pub messgr_id: Uuid,      // comms_request.id, sent to the provider for correlation
}

pub enum SenderError {
    Http(reqwest::Error),     // transport failure: always retryable
    Provider { status: u16, body: String, retryable: bool, retry_after: Option<Duration> },
}

#[async_trait]
pub trait Sender: Send + Sync {
    async fn send(&self, msg: &Outbound<'_>) -> Result<SendOutcome, SenderError>;
}
```

A new vendor for an existing shape adds an adapter and no variant. A new variant is justified only
by a content shape no existing variant can carry.

**2. Errors: the adapter classifies.** Retryability is decided inside each adapter, not derived
from the HTTP status by the dispatcher (today's `SenderError::is_retryable`, T-021). Vendors
signal rate limiting differently — SendGrid with `429`, Meta with a `400` whose body carries
`130429`, Telegram with `429` and a `retry_after` — and only the adapter can read its vendor's
signal. When the vendor says how long to wait, the adapter passes it on as `retry_after` and the
dispatcher's backoff (§2.4 step 6) waits at least that long.

**3. Correlation: `messgr_id` out, `messgr_id` back.** Each adapter sends `comms_request.id` in
whatever field its provider echoes on receipts — porth's idempotency key, SendGrid's
`custom_args`, Meta's `biz_opaque_callback_data`. A receipt then finds its message without a
lookup by `provider_ref`. Where a provider echoes nothing back, `provider_ref` is the fallback.

**4. Inbound: one route, one parser per provider.** `messgr-webhook` already routes by provider:
`POST /webhook/{webhook_token}/{provider}` (T-047). What is generic today is the rest: one global
`WebhookVerifier` (HMAC-SHA256, `src/webhook_verify/mod.rs`) and one `GenericReceipt` body
(`src/webhook/handler.rs`). Each provider instead supplies a receipt source that verifies its own
signature and parses its own body into receipts in messgr's existing status vocabulary —
`sent`, `delivered`, `read`, `failed`, `bounced`, `complaint`, `expired`
(`src/orphan_reconcile/reconcile.rs`), plus an opt-out once decision X1 settles what it becomes:

```rust
pub trait ReceiptSource: Send + Sync {
    fn verify(&self, key: &[u8], headers: &HeaderMap, raw_body: &[u8]) -> bool;
    fn parse(&self, raw_body: &[u8]) -> Result<Vec<Receipt>, ParseError>;   // many per request
}
```

An event the parser does not map is **ignored with `200`**, never rejected: a vendor that sees
errors retries, and may disable the webhook. Staging, promotion, dedup and orphan handling
(T-047) stay as they are for every provider.

**5. Configuration: one row shape.** Every adapter is configured by one `provider_config` row:
`channel`, `priority`, `provider`, `base_url`, `sender_id`, `credential_path`,
`rate_limit_per_sec`, plus a Vault path for the webhook key. No adapter reads an environment
variable of its own.

**Per-adapter facts.** Some differences are real and are not smoothed over by the contract. Each
adapter records them in the table in §15.5, and a ticket adding an adapter fills its column
before it can be READY:

- whether the provider sends receipts at all, and which of its statuses are final;
- whether it accepts an idempotency key;
- what copy of the content it keeps, and for how long (§7.2 exemption);
- which of its signals is an opt-out (decision X1);
- which content shape it takes;
- what `rate_limit_per_sec` means for it (§15.5, finding 2).

**Enforcement.** Two things keep the contract honest as adapters are added:

- **A conformance test** every adapter runs, parameterised by a wiremock fixture set for that
  vendor: a successful send yields `provider_ref`; a rate-limit response is retryable and carries
  `retry_after` where the vendor gives one; a permanent rejection is terminal; a transport error
  is retryable; each receipt fixture parses to the expected status; a bad signature is rejected
  with `401`; an unknown event is ignored; a replayed receipt yields one event. It is written
  with the first real adapter and reused by every later one.
- **A review rule** in `development/review-addendum.md`, added with the conformance test: an
  adapter ticket passes review only if its adapter runs the conformance test and its column in
  §15.5 is filled.

### 15.2 SMS: porth, and building without waiting for it

SMS is decided (§2.5): the tenant's porth, over porth's REST API. The adapter is thin — porth
does routing, pacing to the operator, failover and retries — and it is the one blocked by
another project. T-062 (send) needs porth's five new submit fields (POR-025, POR-027, POR-028,
POR-026) and T-063 (receipts) needs porth's callback body (POR-026).

**Plan: build messgr against a pinned contract and a fake, and gate going live, not building.**

1. **Pin the wire contract in porth's design first.** porth design 1.30 §4.1 names the five
   fields' meaning but leaves their names to each porth ticket's refinement. A fake built before
   the names are fixed guesses them, and a guess is a contract drift waiting for integration day.
   So the contract is written once, in porth design §4.1, by porth, before either side builds.
   Proposed shape, using the field names porth's §5 message model already has:

   ```json
   POST /api/v1/sms/send
   {
     "from_number": "BANK",
     "to_number": "+15551234567",
     "message": "Your code is 123456",
     "priority": "high",                        // "high" | "normal", default "normal"
     "valid_until": "2026-09-29T10:05:00Z",     // RFC 3339; absent = no limit
     "idempotency_key": "<comms_request.id>",
     "keep_text": false,                        // default true
     "callback_url": "https://<internal>/webhook/<token>/porth"
   }
   → { "message_id": "...", "status": "queued" }
     (a repeated idempotency_key returns the first message's id and current status)

   POST <callback_url>
   { "message_id": "...", "idempotency_key": "<comms_request.id>",
     "status": "delivered" | "failed" | "expired", "occurred_at": "..." }
   ```

   One requirement belongs in the contract and is easy to miss: **porth rejects a submit field it
   does not know with `400`.** Otherwise messgr, pointed at a porth that predates POR-027, has
   `keep_text: false` silently ignored, and an OTP is kept in porth in clear (§7.4) with nothing
   failing.

2. **messgr encodes the contract as a wiremock fake**, shared by T-062's adapter tests and
   T-063's receipt tests: the submit request each case must produce, porth's responses (success,
   repeat key, `400`, `5xx`, unreachable), and callback bodies for each final status. T-063 needs
   no fake porth at all to be tested: its tests post the pinned callback bodies to the internal
   listener.
3. **porth tests against the same examples.** POR-025–028's acceptance tests use the JSON pinned
   in step 1, so both sides are tested against one contract, not two readings of it.
4. **A contract check against a real porth** once porth's container image (POR-019) carries the
   fields: submit, repeat, callback, on a local porth. It is the go-live check, not a merge gate.
5. **Tickets change accordingly.** T-062 and T-063 are built, reviewed and merged against the
   fake; the porth tickets stop being their build prerequisites and become a **per-tenant go-live
   gate**: a tenant's `provider_config` row is pointed at its porth only once that porth passes
   step 4. POR-027 stays a hard gate for OTP (§7.4).

A standalone fake porth for running messgr by hand (a small `cargo run --example fake-porth` that
accepts a submit and calls back `delivered`) is **not** part of the plan. The wiremock fake covers
the tests; add the standalone one only when someone needs to click through a local send, and
until then a local porth container does that job once step 4 is reachable.

### 15.3 Email: SendGrid

Decision 31's mock contract (`POST {base}/messages`, bearer token, `{to, body}`,
`src/sender/http.rs`) carries no subject or from-address, and SendGrid does not accept it.

**Send — `SendGridSender`.**

- `POST {base_url}/v3/mail/send`, bearer API key.
- Body: `personalizations[0].to` = destination, `personalizations[0].custom_args.messgr_id` =
  correlation id, `from` = the tenant's `sender_id`, `subject`, `content` =
  `[{type: "text/plain", value: body}]`, `tracking_settings` with click and open tracking
  disabled (decision E1).
- Success is `202 Accepted` with an empty body; `provider_ref` is the `X-Message-Id` response
  header.
- Retryable: transport errors, `429`, `5xx`. Everything else terminal.

**Receipts — SendGrid Event Webhook.**

- `POST` of a JSON array of events.
- Signed with **ECDSA P-256**, not HMAC: headers `X-Twilio-Email-Event-Webhook-Signature` and
  `X-Twilio-Email-Event-Webhook-Timestamp`, signature over timestamp + raw body, verified with the
  tenant's public key. Today's HMAC verifier cannot check it.
- Correlate on `custom_args.messgr_id`, not `sg_message_id` (which is `X-Message-Id` plus a
  suffix).
- Mapping: `delivered` → `delivered`; `bounce`, `dropped` → `bounced` / `failed` (a bounce also
  feeds suppression, T-047); `spamreport` → `complaint`; `processed`, `deferred` → ignored, not
  final; `unsubscribe`, `group_unsubscribe` → decision X1.

### 15.4 WhatsApp: Meta Cloud API

Business-initiated WhatsApp messages must be **templates Meta has approved in advance**, sent as
a template name, language and parameters. Free text is accepted only within 24 hours of the
customer last writing to the business; messgr is outbound-only, so in practice every send is a
template, and Meta, not messgr, renders the final text.

**Send — `MetaWaSender`.**

- `POST {base_url}/{phone_number_id}/messages`, where `base_url` is
  `https://graph.facebook.com/vNN.0` with the Graph API version pinned, and `phone_number_id` is
  the tenant's `sender_id`. Bearer system-user access token.
- Body: `messaging_product: "whatsapp"`, `to` = destination without the leading `+`,
  `type: "template"`, `template: {name, language: {code}, components: [{type: "body",
  parameters: [{type: "text", text}...]}]}`, and `biz_opaque_callback_data` = `messgr_id`.
- Success is `200` with `messages[0].id` (a `wamid.…`) as `provider_ref`.
- Retryable: transport errors, `5xx`, and error codes `130429` (throughput), `131056` (pair rate
  limit), `131000` / `131016` (Meta-side failure). Terminal otherwise, including `131047`
  (outside the 24-hour window), `131026` (undeliverable) and `131050` (recipient stopped
  marketing messages — decision X1).

**Receipts — Meta webhook.**

- A one-time `GET` verification handshake: answer `hub.challenge` when `hub.verify_token`
  matches.
- `POST` signed with `X-Hub-Signature-256` = HMAC-SHA256 of the raw body under the app secret.
- Statuses at `entry[].changes[].value.statuses[]`, correlated on `biz_opaque_callback_data`:
  `delivered` → `delivered`; `read` → `read`; `failed` → `failed`; `sent` → `sent`.
- The same webhook carries **inbound customer messages** (`value.messages[]`) — decision W2.

### 15.5 Comparison

| | porth (SMS) | SendGrid (email) | Meta Cloud API (WhatsApp) |
|---|---|---|---|
| What it is | Our own gateway, one per tenant, internal network | Third-party SaaS, tenant's own account | Third-party SaaS, tenant's own app and number |
| Content shape | `Text`, rendered by messgr | `Email`, rendered by messgr | `Template`; **Meta renders the text** |
| Auth | None (porth's REST API is unauthenticated) | API key, bearer | System-user token, bearer |
| "Accepted" means | Queued in porth; an operator outage never reaches the dispatcher | Queued at the vendor; outages show as `5xx`/`429` | Queued at the vendor; rate limits show as `400` + code |
| Retry signal | HTTP status | HTTP status, `429` | **Error code in the body** |
| Idempotency key | Yes (POR-028) | **None** | **None** |
| Priority | Yes, for OTP (POR-025) | Not needed | Not needed |
| Validity | Yes, from `expires_at` (POR-027) | None | None per message known; check |
| Pacing and failover | porth's (`throughput` per SMSC, prefix routing) | Vendor's | Vendor's; messaging-limit tiers per number |
| `rate_limit_per_sec` means | Handoff cap only | Handoff cap **and** vendor rate limit | Handoff cap **and** vendor throughput |
| Receipts | Callback (POR-026), internal, **unsigned** | Event Webhook, public, ECDSA | Webhook, public, HMAC, `GET` handshake |
| Correlation | `idempotency_key` | `custom_args.messgr_id` | `biz_opaque_callback_data` |
| Copy kept outside messgr | Plaintext, OTP blanked (POR-027) | Retention to check | Up to 30 days, per Meta's docs |
| Opt-out signals | None | `unsubscribe` events | Error `131050`, "STOP" replies |
| Can we change it? | Yes, porth tickets | No | No |
| Blocked on | Nothing to build (§15.2); POR tickets to go live | Decisions V1, E1–E3, X1 | Decisions V1, W1–W3, X1 |

What the comparison shows:

1. **The gaps land in different places.** porth is ours, so a gap there becomes a porth ticket
   and the messgr adapter stays close to the mock. SendGrid and Meta are fixed, so a gap there
   becomes a messgr-side decision, usually to accept it and write it down.
2. **The handoff cap means different things per channel.** For SMS, `rate_limit_per_sec` only
   keeps the backlog in messgr's outbox, where kill switches, cancellation and `expires_at` reach
   it; porth paces to the operator (§5, §2.5). For email and WhatsApp there is no gateway in
   between, so the same number also has to stay under the vendor's rate limit on the tenant's
   account. §4.10's comment on the column ("the handoff cap, not pacing to the operator") is true
   for SMS only; the column's meaning has to be stated per channel. The mechanism does not
   change — one in-process limit per channel — only what it is set against.
3. **Duplicates are solved for SMS only.** Delivery to the provider is at-least-once (§12): a
   send that times out, or a crash, after the provider accepted it is retried. porth's
   idempotency key (POR-028) turns that retry into a no-op. Neither SendGrid nor Meta accepts
   one, so on email and WhatsApp the customer gets the message twice. §12 already accepts "a
   small duplicate rate" where a provider has no key; what it does not say is which channels
   those are, and the answer is every channel but SMS.
4. **Receipt verification splits three ways**: unsigned on an internal listener (porth, T-063),
   ECDSA and HMAC on the public one. The single global verifier becomes one per provider
   (§15.1, part 4).
5. **WhatsApp is the only channel where messgr does not render the final text**, so the ledger
   shows what the customer read only if messgr keeps a copy of each approved template (W1).

### 15.6 Design changes this implies

| Section | Change |
|---|---|
| Decision 31 (§14) | Revised: the one `Sender` trait stands, but it takes `Outbound` (§15.1) and each real vendor has its own adapter; the mock `HttpSender` stays as a test double only. |
| §2.4 step 6, T-021's retry rule | Retryability moves from the HTTP status into each adapter, with `retry_after`. |
| §12 delivery semantics | Name the channels: duplicates prevented for SMS (POR-028), accepted for email and WhatsApp. |
| §4.10 `provider_config` | Add `base_url` (per-tenant porth; a fixed value for SendGrid and Meta, overridden in tests) and `sender_id` (porth's SMS sender ID, the email from-address, the WhatsApp `phone_number_id` — closing the SMS gap §4.10 records). `rate_limit_per_sec`'s meaning stated per channel (§15.5, finding 2). The webhook keys (SendGrid public key, Meta app secret and verify token) need a Vault path of their own. |
| Templates | Email templates gain a subject. A WhatsApp template in messgr is a pointer to a Meta-approved template (name, language, parameter count); `payload_ciphertext` holds the parameters (decision W1). |
| §10 `messgr-webhook` | One `ReceiptSource` per provider replaces the global verifier and `GenericReceipt`; Meta's `GET` handshake; porth on the internal listener (T-063). |
| §7.2 erasure exemptions | Each third-party copy is an erasure bound outside the schema, like porth's: Meta's (up to 30 days per its docs), SendGrid's (to check), and any later channel's (§15.8). |
| `development/review-addendum.md` | The adapter review rule (§15.1, enforcement), added with the first real adapter. |
| porth design §4.1 | The pinned wire contract, including rejecting unknown fields (§15.2). |

### 15.7 Decisions to take

| # | Decision | Recommendation so far |
|---|---|---|
| P1 | Pin porth's REST contract (§15.2) in porth design §4.1 now, and move the porth tickets from T-062/T-063's build prerequisites to a per-tenant go-live gate? | **Yes.** It unblocks both tickets and makes drift a porth design change rather than an integration-day surprise. |
| E1 | Email click and open tracking on or off? | **Off.** Click tracking rewrites links in bank emails onto the vendor's domain, which trains customers to trust what phishing looks like; the open pixel is personal data and unreliable; receipts need only delivered and bounced. |
| E2 | Plain-text email only (decision 31), or HTML? | **Plain text first.** HTML is additive later (a second `text/html` part) but brings sanitising, branding templates and a rendering review; add it when a producer needs branded mail. |
| E3 | Marketing email needs an unsubscribe mechanism (the large mailbox providers require a one-click `List-Unsubscribe` header from bulk senders). Does messgr add it, and who receives the unsubscribe? | Open. Tied to X1. |
| W1 | How is a Meta-approved template represented, and does messgr keep a copy of each approved version's text so the ledger shows what the customer read? | Keep a versioned copy alongside the pointer. Open. |
| W2 | Inbound WhatsApp messages on the receipt webhook: drop, or record? | **Drop the content** — it is new PII with no consumer. Opt-outs are X1. |
| W3 | Must messgr's `class` match the Meta template category (marketing, utility, authentication)? Meta can re-categorise a template, which changes its price and its opt-out treatment. | Open. |
| X1 | **Opt-outs that arrive from a provider** — SendGrid `unsubscribe`, Meta `131050`, a WhatsApp "STOP", Telegram's "bot was blocked" (§15.8) — become a consent change (marketing only, keyed on `address_id`, §5), a suppression entry (keyed on `destination_hmac`, blocks every class), or nothing (the provider enforces its own list)? Which system owns consent writes, and whether SMS and WhatsApp destinations share a `destination_hmac`, must be checked in §4 and §5 first. | Open — **check before recommending.** Suppression looks wrong for a marketing opt-out, because it would also block fraud alerts. |
| V1 | Confirm the vendors: SendGrid (or another HTTP email API, or the bank's own SMTP relay, §13) and Meta Cloud API direct (or a business solution provider). An SMTP relay changes the email half entirely: messgr would send to the relay the way SMS goes to porth. | Open (Still Open #4). |

### 15.8 Adding a channel: Telegram as the worked example

The test of §15.1 is a channel nobody has designed for. Telegram's Bot API, walked through the
contract, shows what the contract absorbs and what it cannot:

- **Content shape: `Text`, no new variant.** `POST https://api.telegram.org/bot<token>/sendMessage`
  with `{chat_id, text}`. The token sits in the URL path, so the adapter must keep request URLs
  out of logs and traces.
- **Errors fit part 2.** `429` with `parameters.retry_after` in the body is retryable with that
  delay; `403` "bot was blocked by the user" is terminal and an opt-out signal (X1); `400` "chat
  not found" is terminal.
- **Correlation falls back to `provider_ref`** (the returned `message_id`); nothing is echoed.
- **Receipts: none.** The Bot API reports no delivery status, so `sent` is final, and the orphan
  reconciler must not wait for a receipt that never comes. This is a per-adapter fact, not a
  contract exception.
- **Idempotency: none** — the same accepted-duplicate rule as email and WhatsApp.
- **Retention is the hard part.** Messages stay on Telegram's servers until deleted, and a bot can
  delete its own message only for a limited time after sending. That is an erasure bound (§7.2)
  that cannot be fixed at 30 days, and it is a compliance decision before it is an adapter.
- **The destination is the real cost.** A bot can message only a user who has started a chat with
  it, and the address is a `chat_id`, not a phone number. `customer_address.kind` takes a new
  value without a migration (§4.6), but capturing the `chat_id` needs an inbound flow (the
  customer taps a link, the bot receives `/start` with a token, messgr binds the `chat_id` to the
  customer) that messgr does not have. Verification (§5) is that binding.

So the adapter itself is the smallest Telegram task. The contract makes the adapter uniform; it
cannot make a channel's address capture, consent or retention uniform, and §15.1's per-adapter
facts are where those surface before anything is built.

### 15.9 When this becomes tickets

- **Now, after P1:** amend T-062 and T-063 to build against the pinned contract and the fake
  (§15.2), and move the porth tickets to a go-live gate in each.
- **After V1 and X1:** two tickets, **email adapter + SendGrid receipt source** and **WhatsApp
  adapter + Meta receipt source**. The `Outbound` trait change, the conformance test and the
  review rule go in whichever of T-062 or these lands first; `base_url` and `sender_id` most
  likely come with T-062, which needs them.
- **Telegram and any later channel:** not before a producer needs the channel, and then the
  retention and address-capture questions of §15.8 come first.
