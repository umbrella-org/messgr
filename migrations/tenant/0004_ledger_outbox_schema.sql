-- Ledger + outbox schema (DESIGN.md §4.1-4.4, T-009): comms_request,
-- outbox, comms_event, idempotency, and their indexes, verbatim from the
-- design's own CREATE TABLE statements. No tenant_id column on
-- outbox/comms_event/idempotency and no REFERENCES clauses beyond what
-- the design itself declares -- see T-009 decision 1. Consent (T-020),
-- suppression (T-038), and template (T-010) -- also in §4.4 -- are
-- separate tickets.

CREATE TABLE comms_request (
    tenant_id              uuid        NOT NULL,   -- tripwire + consolidation path (§2.1)
    id                     uuid        NOT NULL,
    created_at             timestamptz NOT NULL,
    customer_id            uuid        NOT NULL,   -- always set; provisional shell if unresolvable (§4.6)
    channel                text        NOT NULL,   -- sms | email | whatsapp
    class                  text        NOT NULL,   -- auth | transactional | marketing
    template_id            text        NOT NULL,
    template_version       int         NOT NULL,   -- pinned; templates are immutable per version
    campaign_id            text,                   -- null for transactional
    destination_hmac       bytea       NOT NULL,   -- keyed HMAC, pepper in Vault; indexed lookup
    destination_ciphertext bytea       NOT NULL,   -- under customer DEK; the address as actually used
    payload_ciphertext     bytea,                  -- NULL for auth class; see §7
    producer_id            uuid        NOT NULL,   -- registered caller (§4.9)
    scheduled_for          timestamptz,            -- NULL = send immediately (§6.2)
    expires_at             timestamptz,            -- drop rather than send late (§6.2)
    final_status           text,                   -- NULL while in flight; see §4.1
    finalized_at           timestamptz,
    PRIMARY KEY (created_at, id)
) PARTITION BY RANGE (created_at);

CREATE INDEX ON comms_request (customer_id, created_at DESC);
CREATE INDEX ON comms_request (final_status, created_at DESC);
CREATE INDEX ON comms_request (campaign_id, created_at) WHERE campaign_id IS NOT NULL;
CREATE INDEX ON comms_request (destination_hmac, created_at DESC);

CREATE TABLE outbox (
    comms_request_id  uuid        PRIMARY KEY,
    created_at        timestamptz NOT NULL,     -- FK component into ledger partition
    channel           text        NOT NULL,
    class             text        NOT NULL,
    priority          smallint    NOT NULL,     -- 1 transactional, 2 marketing; stored, not computed
    customer_id       uuid        NOT NULL,
    address_id        uuid        NOT NULL,     -- resolved contact point (§4.6); consent keys on it
    producer_id       uuid        NOT NULL,     -- quota + kill-switch scoping (§5.1, §5.2)
    campaign_id       text,
    next_attempt_at   timestamptz NOT NULL,     -- future-dated for scheduled sends (§6.2)
    expires_at        timestamptz,
    cancelled_at      timestamptz,              -- set by cancellation; re-checked before send
    attempts          smallint    NOT NULL DEFAULT 0,
    leased_until      timestamptz
);

CREATE INDEX outbox_claim ON outbox (channel, priority, next_attempt_at)
    WHERE leased_until IS NULL;
CREATE INDEX ON outbox (producer_id, next_attempt_at);
CREATE INDEX ON outbox (campaign_id, next_attempt_at) WHERE campaign_id IS NOT NULL;

CREATE TABLE idempotency (
    producer_id       uuid        NOT NULL,
    key               text        NOT NULL,
    comms_request_id  uuid        NOT NULL,
    expires_at        timestamptz NOT NULL,
    PRIMARY KEY (producer_id, key)
);

CREATE TABLE comms_event (
    comms_request_id  uuid        NOT NULL,
    customer_id       uuid        NOT NULL,  -- denormalized so erasure can find these rows
    occurred_at       timestamptz NOT NULL,
    event_type        text        NOT NULL,
      -- queued | sent | delivered | failed | bounced | read | complaint
      -- | expired | cancelled | suppressed_consent | suppressed_list | unverified_address
    provider_ref      text        NOT NULL DEFAULT '',  -- '' when the provider gave none (dispatch-internal events); dedup needs a non-NULL value
    provider_status   text,                  -- normalized code, safe to keep in clear
    provider_payload_ciphertext bytea,       -- raw provider JSON, under customer DEK -- see §4.4
    UNIQUE (occurred_at, comms_request_id, event_type, provider_ref)
) PARTITION BY RANGE (occurred_at);

CREATE INDEX ON comms_event (customer_id, occurred_at);

CREATE TABLE orphan_event (
    id                          uuid        PRIMARY KEY,
    received_at                 timestamptz NOT NULL,
    provider                    text        NOT NULL,
    provider_ref                text        NOT NULL,
    occurred_at                 timestamptz NOT NULL,
    event_type                  text        NOT NULL,
    provider_status             text,
    provider_payload_raw        jsonb,
    reconcile_attempts          smallint    NOT NULL DEFAULT 0
);
CREATE INDEX ON orphan_event (provider_ref);

-- Bootstrap partitions (T-009 decision 2): T-014's create-ahead job
-- doesn't exist yet. Creates the current and next calendar month for
-- both partitioned tables, computed when this migration actually runs
-- (i.e. at tenant provisioning time). T-014 takes over from here.
DO $$
DECLARE
    i           int;
    month_start date;
    month_end   date;
    suffix      text;
BEGIN
    FOR i IN 0..1 LOOP
        month_start := (date_trunc('month', now()) + (i || ' months')::interval)::date;
        month_end   := (month_start + interval '1 month')::date;
        suffix      := to_char(month_start, 'YYYY_MM');

        EXECUTE format(
            'CREATE TABLE %I PARTITION OF comms_request FOR VALUES FROM (%L) TO (%L)',
            'comms_request_' || suffix, month_start, month_end
        );
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF comms_event FOR VALUES FROM (%L) TO (%L)',
            'comms_event_' || suffix, month_start, month_end
        );
    END LOOP;
END $$;
