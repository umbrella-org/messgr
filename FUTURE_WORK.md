# Future work

Work that has been thought through but is not yet a ticket. Nothing here is built from this file
directly: an item becomes work only as a ticket that meets the READY gate (see `AGENTS.md`,
"Brine"). Each item records the outline reached so far, the design changes it implies, and the
decisions that must be taken before it can be filed.

## Email and WhatsApp adapters

**Status:** outline only, parked 2026-09-29. Assumed vendors: **Twilio SendGrid** for email (v3
Mail Send API) and **Meta WhatsApp Cloud API** directly (no business solution provider). Both
choices are assumptions for the outline, not decisions; Still Open #4 remains open for email and
WhatsApp until they are confirmed.

### Why the current design does not stretch to them

Decision 31 says email and WhatsApp reuse SMS's `Sender` contract and `HttpSender` as-is, with no
per-channel adapter. That holds only against the mock contract `HttpSender` speaks
(`POST {base}/messages`, bearer token, `{to, body}`, `src/sender/http.rs`). No real email or
WhatsApp provider accepts it:

- Email needs a subject and a from-address; `send(destination, body)` carries neither.
- WhatsApp business-initiated messages must be **templates pre-approved by Meta**, sent as a
  template name, language and parameters. Free text is accepted only inside the 24-hour window
  after the customer last wrote to the business, and messgr is outbound-only, so in practice
  every WhatsApp send is a template.
- `SenderError::is_retryable` derives retryability from the HTTP status (4xx terminal, 5xx
  retried). SendGrid signals rate limiting with `429`, and Meta with **`400`** carrying error
  code `130429`. Both must be retried; the current rule treats both as terminal.

SMS is unaffected in substance: it keeps going through porth (§2.5, decision 34).

### Outline

**`Sender` trait widens** (`src/sender/mod.rs`) from `send(destination, body)` to a message that
carries channel-specific content and a correlation id:

```rust
pub enum Content {
    Text { body: String },                                   // SMS → porth
    Email { subject: String, body: String },
    WaTemplate { name: String, language: String, params: Vec<String> },
}

pub struct Outbound<'a> {
    pub destination: &'a str,
    pub content: &'a Content,
    pub messgr_id: Uuid,   // sent to the provider, echoed back on receipts
}

pub enum SenderError {
    Http(reqwest::Error),
    Provider { status: u16, body: String, retryable: bool }, // each adapter classifies
}
```

**Email adapter — `SendGridSender`.**

- `POST {base_url}/v3/mail/send`, bearer API key.
- Body: `personalizations[0].to` = destination, `personalizations[0].custom_args.messgr_id` =
  correlation id, `from` = the tenant's from-address, `subject`, `content` =
  `[{type: "text/plain", value: body}]`, `tracking_settings` with click and open tracking
  disabled (see decision E1).
- Success is `202 Accepted` with an empty body; `provider_ref` is the `X-Message-Id` response
  header.
- Retryable: transport errors, `429`, `5xx`. Everything else terminal.

**Email receipts — SendGrid Event Webhook.**

- `POST` of a JSON array of events.
- Signed with **ECDSA P-256**, not HMAC: headers `X-Twilio-Email-Event-Webhook-Signature` and
  `X-Twilio-Email-Event-Webhook-Timestamp`, signature over timestamp + raw body, verified with the
  tenant's public key. `messgr-webhook`'s existing HMAC check cannot verify it.
- Correlate on `custom_args.messgr_id`, not `sg_message_id` (which is `X-Message-Id` plus a
  suffix).
- Mapping: `delivered` → delivered; `bounce`, `dropped` → failed (bounce also feeds suppression,
  T-047); `spamreport` → complaint; `processed`, `deferred` → not terminal, ignored; `unsubscribe`
  / `group_unsubscribe` → see decision X1.

**WhatsApp adapter — `MetaWaSender`.**

- `POST {base_url}/{phone_number_id}/messages`, where `base_url` is
  `https://graph.facebook.com/vNN.0` with the Graph API version pinned. Bearer system-user
  access token.
- Body: `messaging_product: "whatsapp"`, `to` = destination without the leading `+`,
  `type: "template"`, `template: {name, language: {code}, components: [{type: "body",
  parameters: [{type: "text", text}...]}]}`, and `biz_opaque_callback_data` = `messgr_id`.
- Success is `200` with `messages[0].id` (a `wamid.…`) as `provider_ref`.
- Retryable: transport errors, `5xx`, and error codes `130429` (throughput), `131056` (pair rate
  limit), `131000` / `131016` (Meta-side failure). Terminal otherwise, including `131047`
  (outside the 24-hour window), `131026` (undeliverable) and `131050` (recipient stopped
  marketing messages — see decision X1).

**WhatsApp receipts — Meta webhook.**

- A one-time `GET` verification handshake: answer `hub.challenge` when `hub.verify_token`
  matches.
- `POST` signed with `X-Hub-Signature-256` = HMAC-SHA256 of the raw body under the app secret.
  This fits the existing HMAC check, once the key is per tenant.
- Statuses at `entry[].changes[].value.statuses[]`, correlated on `biz_opaque_callback_data`:
  `delivered` → delivered; `failed` → failed; `sent`, `read` → not terminal, ignored.
- The same webhook also carries **inbound customer messages** (`value.messages[]`) — see
  decision W2.

Error codes, header names and payload shapes above were taken from the vendors' public APIs
while outlining; re-verify each against current vendor documentation when the tickets are
refined.

### Design changes this implies

| Section | Change |
|---|---|
| Decision 31 (§14) | Revised: one adapter per real vendor behind the `Sender` trait; the mock `HttpSender` stays as the test double only. |
| §2.4 step 6, T-021's retry rule | Retryability moves from the HTTP status into each adapter. |
| §4.10 `provider_config` | Add `sender_id` (the email from-address, the WhatsApp `phone_number_id`, and porth's SMS sender ID — one column covers the SMS gap §4.10 already records) and `base_url` (per-tenant porth URL; a fixed value for SendGrid and Meta, overridden in tests). `credential_path` holds the SendGrid API key and the Meta access token; the webhook keys (SendGrid public key, Meta app secret and verify token) need a Vault path of their own. |
| Templates | Email templates gain a subject. A WhatsApp template in messgr becomes a pointer to a Meta-approved template (name, language, parameter count); the ledger's `payload_ciphertext` holds the parameters, not the final text, because Meta renders it. See decision W1. |
| §10 `messgr-webhook` | One signature verifier per provider (ECDSA for SendGrid, HMAC for Meta), Meta's `GET` handshake, and a per-tenant route (e.g. `/webhook/{tenant}/{provider}`), since each tenant brings its own SendGrid account and Meta app (§4.10). |
| §7.2 erasure exemptions | Both vendors keep their own copy of what was sent: Meta states the Cloud API retains message content for up to 30 days; SendGrid's retention of content and event data must be checked. Both are erasure bounds outside the schema, like porth's plaintext copy. |
| Still Open #4 | Neither vendor accepts an idempotency key, so a send that times out after the vendor accepted it is duplicated on retry. Accept and record it; nothing on the vendor side fixes it (porth POR-028 fixes it for SMS only). |

### Decisions to take before filing

| # | Decision | Recommendation so far |
|---|---|---|
| E1 | Email click and open tracking on or off? | **Off.** Click tracking rewrites links in bank emails onto the vendor's domain, which trains customers to trust exactly what phishing looks like; the open pixel is personal data and unreliable; receipts need only delivered/bounced. |
| E2 | Plain-text email only (decision 31), or HTML? | **Plain text first.** HTML is additive later (a second `text/html` part) but brings sanitising, branding templates and a rendering review; add it when a producer needs branded mail. |
| E3 | Marketing email must carry an unsubscribe mechanism (a `List-Unsubscribe` one-click header is required by the large mailbox providers for bulk senders). Does messgr add it, and who receives the unsubscribe? | Open. Tied to X1. |
| W1 | How is a Meta-approved template represented in messgr, and does the ledger keep a copy of each approved template version's text, so the audit record shows what the customer actually read? | Keep a versioned copy of the approved text alongside the pointer; the ledger then shows name + version + parameters, resolvable to text. Open. |
| W2 | Inbound WhatsApp messages (customer replies) arriving on the receipt webhook: drop, or record? | **Drop the content** — storing it is new PII with no consumer. Opt-outs are X1. |
| W3 | Does messgr's `class` (marketing / transactional / auth) have to match the Meta template category (marketing / utility / authentication)? Meta can re-categorise a template, which changes its price and its opt-out treatment. | Open. |
| X1 | **Opt-outs that arrive from a provider** — SendGrid `unsubscribe` events, Meta's `131050` send error, a customer's WhatsApp "STOP" — become what: a consent change (marketing only, keyed on `address_id`, §5), a suppression entry (keyed on `destination_hmac`, blocks every class), or nothing (the provider enforces its own list)? Which system owns consent writes, and whether an SMS and a WhatsApp destination share a `destination_hmac`, must be checked in §4 and §5 first. | Open — **check before recommending.** Suppression looks wrong for a marketing opt-out because it would also block fraud alerts. |
| V1 | Confirm the vendors: SendGrid (or another HTTP email API, or the bank's own SMTP relay, §13 note) and Meta Cloud API direct (or a business solution provider). An SMTP relay changes the email half entirely: messgr would then send to the relay, the same way SMS goes to porth. | Open (Still Open #4). |

### When this becomes tickets

Two tickets, once V1 and X1 are settled: **email adapter + SendGrid receipt webhook**, and
**WhatsApp adapter + Meta receipt webhook**. The `Sender` trait change and the two
`provider_config` columns go in whichever is picked up first, since T-062 (SMS through porth)
needs `sender_id` and `base_url` too and may land them earlier.
