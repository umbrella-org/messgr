# messgr

Centralized communications orchestration and audit ledger for customer messaging across
SMS, email, and WhatsApp. See [`DESIGN.md`](DESIGN.md) for the full design; feature work is
tracked as tickets under [`tickets/`](tickets/BOARD.md).

## Local development

```
cp .env.example .env
docker compose up -d          # Postgres (database `control`) and a dev-mode Vault
just vault-dev-init           # one-time: enable Transit + create the messgr-dek key
just control-migrate          # apply control-database migrations
just provision acme eu tenant_acme operator@example.com  #  runs messgr-control provision
```

`messgr-control provision` creates the tenant's database, applies its (currently empty)
migrations, registers it in the control database, and provisions the tenant's own Vault
Transit mount, key, ACL policy, and AppRole (DESIGN.md §7.6, §11.4). Its output looks like:

```
7e5e6b1e-...                                     # tenant_id
vault_role_id=3c9f2b4a-...
vault_wrapped_secret_id=eyJhbGciOi...             # only on a fresh provision — see below
```

The RoleID is persisted (`tenant.vault_role_id`) and safe to log or query — it behaves like a
username, not a credential. The SecretID line, when present, is printed **exactly once**: it is
a Vault response-wrapped token (10-minute TTL, single-use `vault unwrap`) meant for out-of-band
delivery to that tenant's dispatcher deployment (§7.6: "SecretID delivered response-wrapped at
deploy time"). It is never written to Postgres anywhere — capture it immediately or re-run
provisioning is not a way to get it back (an idempotent re-provision of an already-active tenant
deliberately does not mint a new one; see the credential-rotation note below).

Re-running `provision` for the same `slug` (with the same `region`/`database_name`) is safe:
the database/migration/mount/key/policy/role steps are all idempotent no-ops on repeat, and no
second SecretID is minted. Rotating/reissuing a SecretID for an already-provisioned tenant is
not yet a supported operation — it is planned as an operational runbook, not a CLI flag.

### Producers

```
cargo run --bin messgr-control -- producer register \
    --tenant-slug acme --name fraud-alerts \
    --cert-subject "CN=fraud-alerts.internal" \
    --owner-team fraud --contact fraud-oncall@example.com --actor operator@example.com
cargo run --bin messgr-control -- producer list --tenant-slug acme
cargo run --bin messgr-control -- producer disable --tenant-slug acme --name fraud-alerts \
    --actor operator@example.com
```

`producer register` registers an upstream system (fraud, statements, onboarding, ...) against a
tenant (DESIGN.md §4.9): it writes both the `producer` row in the tenant's own database and the
`producer_cert` mapping in the control database, in that order (a producer only in the tenant
database is inert; a `producer_cert` pointing at no producer is a worse, later failure). No send
path consumes this identity yet — `messgr::producer::resolve` (below) is the first reader.

Re-running `register` with identical inputs for the same `--name` is a safe no-op. Re-running it
with different inputs for the same `--name`, or with a `--cert-subject` already bound to another
producer (in this tenant or a different one), is rejected with a non-zero exit.

`producer disable` sets **two** copies to disabled, control database first: `producer_cert.enabled`
(control) — the column mTLS resolution actually reads, since resolution never opens a tenant
database — and then `producer.enabled` (tenant) — the source of truth `producer list` displays.
Neither row is ever deleted, so a disabled producer's certificate fails resolution with a
distinct, diagnosable "disabled" error instead of looking unregistered. Reversible by registering
the same `--name`/`--cert-subject` again (which never re-enables `producer_cert.enabled` — only
`disable` and the initial `register` ever touch that column). Disabling an already-disabled
producer is a safe no-op.

Every `register`/`disable` attempt, including a rejected one, writes a `platform_audit` row
(`producer.register` / `producer.disable`).

### Tenant configuration

```
cargo run --bin messgr-control -- tenant-config set --tenant-slug acme \
    --retention-years 7 --default-timezone Europe/London --default-locale en-GB \
    --quota-day-boundary-tz Europe/London --staleness-max-age-seconds 7200 \
    --actor operator@example.com
cargo run --bin messgr-control -- tenant-config show --tenant-slug acme
```

`tenant_config` (DESIGN.md §4.10) is a **singleton per tenant** — one row, in the tenant's own
database, enforced by a `singleton boolean PRIMARY KEY` rather than by convention. A freshly
provisioned tenant has **no** row until `tenant-config set` is run at least once: several of
these fields (the staleness bound, the quota day-boundary timezone) have no platform-wide
default DESIGN.md has settled on, so there is nothing sensible to seed automatically.

`set` is a plain upsert — safe to re-run. Its outcome is `created` (no prior row), `updated`
(a prior row existed with different values), or `idempotent` (identical to what's already
there); every call, including a rejected one against an unknown `--tenant-slug`, writes a
`tenant_config.set` `platform_audit` row. `--schedule-horizon-days` and `--verification-mode`
are optional, defaulting to DESIGN.md's own `90`/`observe`. This ticket (T-007) scopes the
table to the six fields above — `display_name` and the `oidc_*` columns from DESIGN.md's full
§4.10 table arrive later, when a ticket actually reads them (OIDC: T-035).

### Customer projection

`messgr` is never the system of record for customer data — `customer`, `customer_external_id`,
`customer_address`, and `customer_alias` (DESIGN.md §4.6, `T-015`) are a narrow, read-only
projection, held only to resolve *who to send to right now*: an id, a locale, a timezone, and a
handful of contact points, nothing else. There is no CLI surface for these tables; every write
to them happens inside `messgr-ingest`'s resolution path.

`messgr-ingest` resolves every send to a `customer_id` + `address_id` one of three ways
(DESIGN.md §4.7):

- **`customer_id` supplied** — used directly, after alias expansion (`customer_alias` redirects a
  retired id from a master-system merge to its current one).
- **`external_id` + `external_id_system` supplied** — resolved via `customer_external_id`.
- **Neither supplied** — resolved via `destination`'s keyed HMAC against currently-active
  `customer_address` rows.

**Resolution never rejects a send.** Whichever of the three finds nothing mints a *provisional*
customer (and, for address-only resolution, a provisional address) instead — cheap, thin rows
that get a DEK like any other customer and are reconciled into a real identity later by the event
feed, through the same `customer_alias` merge machinery. A blocked OTP is a worse outcome than a
missing timeline entry, so nothing here ever returns anything but a resolved id (`409` is the one
exception — see above — and it's a genuine identity conflict, not an absence).

Two pieces of the design are explicitly not built yet: the **event feed** that keeps the
projection current from the master system (create/update/merge events; build-order step 10), and
**staleness gating** (DESIGN.md §4.8 — deferring a stale transactional/marketing send rather than
risking delivery to an address the customer no longer holds). Until the feed exists, every
non-provisional row in these tables was put there by a previous resolution, not by upstream
master data.

### Partition lifecycle

```
just tablespace-init   # once per Postgres instance -- creates the messgr_cold tablespace
cargo run --bin messgr-control -- partition-lifecycle run --tenant-slug acme
```

`comms_request` and `comms_event` (DESIGN.md §4.1, T-009) are partitioned by month. This
command (T-014) keeps them self-managing, in one run: ensures the current and next month's
partitions exist, moves partitions older than 18 months (fixed platform-wide, §7.5) to the
`messgr_cold` tablespace (created once per cluster by `just tablespace-init`, not by this
command — tablespaces are shared across every database in the instance), and detaches + drops
partitions past the tenant's `tenant_config.retention_years` boundary. **If the tenant has no
`tenant_config` row, the drop step is skipped entirely** — printed as `retention: skipped
(tenant_config not set)` — rather than assuming a default retention on a bank's ledger data.
Safe to re-run; meant to be invoked on a schedule (cron/systemd timer), not run continuously —
this binary does not daemonize.

### Provider configuration

```
cargo run --bin messgr-control -- provider-config set --tenant-slug acme \
    --channel sms --priority 1 --provider generic-http \
    --credential-path secret/data/acme/sms --rate-limit-per-sec 10 \
    --actor operator@example.com
cargo run --bin messgr-control -- provider-config list --tenant-slug acme --channel sms
```

`provider_config` (DESIGN.md §4.10, §12.1) is an **ordered list per channel**, in the tenant's
own database, keyed by `(channel, priority)` — even at length 1, so later multi-provider
failover is a config change rather than a schema migration. `set` is a plain upsert on one
`(channel, priority)` row at a time — safe to re-run, with the same `created`/`updated`/
`idempotent` outcome and `platform_audit` trail as `tenant-config set`. `list` prints a
channel's rows in failover order (lowest `priority` first).

No real vendor is wired up yet — `provider` is a free-text label, and `HttpSender` (the first
`Sender` implementation) speaks a small JSON-over-HTTP contract of this codebase's own design,
proven against a local mock server rather than a committed vendor's API (DESIGN.md's own
"provider selection" question, Still Open #5, stays open). `credential_path` (a Vault path) and
`rate_limit_per_sec` are stored but not yet read by anything — Vault KV-secret retrieval and
rate-limit enforcement are later tickets' work.

### Customer DEK pre-provisioning

```
cargo run --bin messgr-control -- customer-dek pre-provision --tenant-slug acme \
    --customer-id 11111111-1111-1111-1111-111111111111 \
    --customer-id 22222222-2222-2222-2222-222222222222 \
    --actor operator@example.com
```

`customer-dek pre-provision` (DESIGN.md §7.6) ensures each given customer id has a `customer_dek`
row, minting a fresh Vault Transit datakey for whichever don't — so a sealed or unreachable Vault
doesn't block a new customer's first message. It takes **explicit customer ids, not a live
customer base**: the customer projection (DESIGN.md §4.6, "Customer projection" above) has no
event feed yet (build-order step 10), so there is no live upstream source for this command to
query on its own. Wiring an automatic trigger from the real customer base is deferred to
whichever future ticket adds that feed.

Safe to re-run: identical ids report `already_existed` instead of minting a second DEK. The same
lifecycle also has a lazy path (`customer_dek::lifecycle::get_or_create_dek`) that future
ingest/dispatcher code calls on a customer's first message — no CLI surface for that path, since
it has no operator action to trigger.

Every payload/destination this eventually encrypts uses a **bounded, zeroizing, TTL cache**
(`key_cache::KeyCache`) of unwrapped DEKs, so steady-state sending makes no Vault calls once a
customer's DEK is warm — Vault is only on the path for a cache miss or pre-provisioning.

### Templates

```
cargo run --bin messgr-control -- template approve --tenant-slug acme \
    --template-id balance-alert --version 1 --channel sms --locale en-GB \
    --body-file body.txt --actor operator@example.com
cargo run --bin messgr-control -- template show --tenant-slug acme \
    --template-id balance-alert --version 1 --locale en-GB
cargo run --bin messgr-control -- template list --tenant-slug acme --template-id balance-alert
cargo run --bin messgr-control -- template render --tenant-slug acme \
    --template-id balance-alert --version 1 --locale en-GB \
    --var name=Jordan --var "balance=£120.00"
```

`template` (DESIGN.md §4.4) rows are **immutable once approved** — `approve` is the only way a
row is ever written, and it always stamps `approved_by`/`approved_at`; there is no draft state
and no `update`. A content change is always a new `--version`: re-`approve`-ing an existing
`(template_id, version, locale)` is rejected, not overwritten.

Bodies use literal `{{key}}` placeholders (whitespace inside the braces is trimmed, so
`{{ key }}` also matches), substituted by `template render` or, once it exists, `T-011`'s ingest
path. A key the body references but the caller doesn't supply is a **hard render error** — a
bank must not send customer-facing content with an unsubstituted placeholder — never sent as
literal `{{key}}` text or blanked out.

Every `approve` attempt, including a rejected one, writes a `template.approve` `platform_audit`
row. This is the bare operator-driven surface, matching `producer`/`tenant-config` — the audited,
role-gated admin approval workflow (§11.1, §11.3) is `T-042`'s scope, not this one's.

### mTLS resolution and dev PKI

`messgr::producer::resolve::resolve_producer(control_pool, cert_subject)` (DESIGN.md §4.9, §11.1,
T-006) is the shared layer that turns a client certificate's subject into `(tenant_id,
producer_id)`: one control-database query, distinguishing an unknown cert from a
known-but-disabled one. `messgr-ingest` (`T-011`, below) is the binary that terminates the mTLS
handshake and calls it.

To exercise it locally without a real CA:

```
just dev-pki-bootstrap                                  # one-time: mount pki, root CA, producer-dev role
just dev-pki-issue-cert fraud-alerts.internal /tmp/cert  # writes cert.pem, key.pem, ca.pem
openssl x509 -noout -subject -in /tmp/cert/cert.pem      # CN=fraud-alerts.internal
```

`dev-pki bootstrap`/`dev-pki issue-cert` are backed by Vault's own PKI secrets engine (the same
dev-mode Vault `docker compose up -d` already starts) and refuse to run outside
`MESSGR_PROFILE=dev` — minting a trusted producer client certificate ad hoc must never be
reachable against a real Vault.

### messgr-ingest: `POST /comms`

`messgr-ingest` (`T-011`) is the first end-to-end send path: it terminates mTLS itself, resolves
the client certificate to a producer via `resolve_producer` above, resolves the customer and
address to send to (`T-015`, next section), and writes the ledger + outbox row in one
transaction, encrypting the destination and rendered template body under the customer's DEK
(DESIGN.md §4.1–§4.3, §7, §11). The request body identifies the customer one of three ways —
exactly one of `customer_id`, `external_id` + `external_id_system` together, or neither (resolve
by `destination` alone) — any other combination is rejected (`422`). No gate chain exists yet
(consent, quotas, kill switches, suppression — §5, §5.1, §5.2 — are unbuilt; `T-016` is kill
switches specifically, not the gate chain as a whole), and `class = "auth"` is rejected outright
(`422`) — OTP has its own path
(`sms-sender`/`otp-api`, `T-047`), never this one.

Requires four env vars, no defaults (`.env.example`): `INGEST_LISTEN_ADDR` (default
`0.0.0.0:8443` if unset), `INGEST_TLS_CERT_FILE`, `INGEST_TLS_KEY_FILE`,
`INGEST_TLS_CLIENT_CA_FILE`. Locally, mint a server identity with a DNS SAN (unlike a producer's
own client certificate, a server's certificate *is* hostname-checked by whatever connects to
it):

```
just dev-pki-bootstrap
just dev-pki-issue-server-cert messgr-ingest.internal /tmp/ingest-cert   # writes cert.pem, key.pem, ca.pem

INGEST_TLS_CERT_FILE=/tmp/ingest-cert/cert.pem \
INGEST_TLS_KEY_FILE=/tmp/ingest-cert/key.pem \
INGEST_TLS_CLIENT_CA_FILE=/tmp/ingest-cert/ca.pem \
just ingest-run
```

Then, with a tenant provisioned, configured, and a producer registered (sections above) and a
client certificate issued for it via `just dev-pki-issue-cert` (no `--server` — a producer's
certificate is never hostname-checked):

```
curl -sk --cert /tmp/producer-cert/cert.pem --key /tmp/producer-cert/key.pem \
  --cacert /tmp/ingest-cert/ca.pem --resolve messgr-ingest.internal:8443:127.0.0.1 \
  -H "Idempotency-Key: demo-1" -H "Content-Type: application/json" \
  -d '{"customer_id":"<uuid>","destination":"+15550100","channel":"sms","class":"transactional","template_id":"balance-alert","template_version":1,"variables":{"name":"Jordan","balance":"100.00"}}' \
  https://messgr-ingest.internal:8443/comms
```

Or identify the customer by an upstream system's own id instead of a `messgr` `customer_id`:

```
curl -sk --cert /tmp/producer-cert/cert.pem --key /tmp/producer-cert/key.pem \
  --cacert /tmp/ingest-cert/ca.pem --resolve messgr-ingest.internal:8443:127.0.0.1 \
  -H "Idempotency-Key: demo-2" -H "Content-Type: application/json" \
  -d '{"external_id":"core-banking-12345","external_id_system":"core_banking","destination":"+15550100","channel":"sms","class":"transactional","template_id":"balance-alert","template_version":1,"variables":{"name":"Jordan","balance":"100.00"}}' \
  https://messgr-ingest.internal:8443/comms
```

Returns `201` with a `comms_request_id` on the first call; repeating it with the same
`Idempotency-Key` returns `200` with the same id rather than sending twice. Two other error cases
are specific to resolution: an invalid combination of `customer_id`/`external_id`/
`external_id_system` is `422`, and a `destination` already active under a *different* customer
than the one resolved is `409` (DESIGN.md §4.6's correction — see "Customer projection" above).
Once `messgr-dispatcher` (`T-013`, below) is running against this tenant and channel, the row is
picked up and `final_status` moves to `sent` or `failed`.

`docker compose up -d` also starts a dev-mode Vault (`VAULT_ADDR=http://localhost:8200`,
`VAULT_TOKEN=messgr-dev-root-token`, both in `.env.example`) backing the `KeyStore` trait
(`src/keystore.rs`) that per-customer data-encryption keys go through (DESIGN.md §7.6).
`just vault-dev-init` is idempotent and only needs to run once per fresh Vault container — it
enables the Transit secrets engine and creates the `messgr-dek` key that `tests/keystore.rs`
exercises, and now also enables the `approle` auth method that every tenant's own AppRole (one
role per tenant, inside this single shared backend) is created under. A dev-mode Vault is for
local development and CI only: `VaultKeyStore::connect` refuses to start against a non-TLS
`VAULT_ADDR` outside `MESSGR_PROFILE=dev`.

### messgr-dispatcher

`messgr-dispatcher` (`T-013`) is the first real send path: a per-channel claim loop (`SKIP
LOCKED` leases, `LISTEN`/`NOTIFY` wakeup via a trigger on `outbox` inserts with a 1s poll
fallback) that calls the `Sender` trait (`T-012`) and writes exactly one `comms_event` row plus
one `comms_request.final_status` update per outbox row, deleting it on completion (DESIGN.md
§4.1, §4.2, §4.4, §9). One process per tenant, one attempt per message — no leader election, no
retry/backoff, and no rate-limit enforcement yet; those are later build-order steps.

Requires `DISPATCHER_TENANT_SLUG` (which tenant this process serves), `VAULT_ROLE_ID` /
`VAULT_WRAPPED_SECRET_ID` (this tenant's own AppRole — printed by `just provision` on a fresh
provisioning run), and, per channel in `DISPATCHER_CHANNELS` (default `sms`),
`DISPATCHER_<CHANNEL>_BASE_URL` / `DISPATCHER_<CHANNEL>_API_KEY` — a dev stand-in `Sender`
credential, since `provider_config` has no `base_url` column and its `credential_path` has no
Vault KV reader yet (T-012 decisions 3–4). With a tenant provisioned (its `vault_wrapped_secret_id`
copied from that output), configured, and a message already sitting in its `outbox` (the
`messgr-ingest` walkthrough above):

```
DISPATCHER_TENANT_SLUG=acme DISPATCHER_CHANNELS=sms \
DISPATCHER_SMS_BASE_URL=http://localhost:9091 DISPATCHER_SMS_API_KEY=dev-key \
VAULT_ROLE_ID=<from just provision> VAULT_WRAPPED_SECRET_ID=<from just provision> \
just dispatcher-run
```

Within about a second (the poll fallback; a `NOTIFY` on insert makes it near-instant),
`comms_request.final_status` moves to `sent` (or `failed`, on a non-2xx/unreachable provider), a
matching `comms_event` row appears, and the `outbox` row is gone.

Run `just --list` for the rest of the available recipes (build, test, lint, db-shell, ...).

## Status

Design-driven, from-scratch rebuild in progress against `DESIGN.md`. Ticket `T-001`
establishes the control database, tenant registry, and this provisioning CLI — the
foundation everything else (producer identity, the ledger, dispatchers) builds on.

Three binaries exist so far: messgr-control (provisioning/admin CLI, run with cargo run --bin
messgr-control -- <subcommand>), messgr-ingest (T-011's POST /comms service, run with cargo run
--bin messgr-ingest or `just ingest-run`), and messgr-dispatcher (T-013's per-tenant claim loop,
run with cargo run --bin messgr-dispatcher or `just dispatcher-run`).
