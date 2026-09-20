-- Holding area for a webhook receipt between messgr-webhook's write (DMZ, unencrypted --
-- see DESIGN.md §10 and T-047's Description) and the next webhook-promote run, which either
-- encrypts it into comms_event under the matched customer's DEK or, if the provider_ref isn't
-- known yet, hands it to orphan_event (the existing, narrower plaintext exception, §4.4) for
-- T-030's reconciler. A named erasure exemption like orphan_event: no customer_id is known yet,
-- so there is no DEK to encrypt under and nothing for erasure to key on.
CREATE TABLE webhook_receipt_staging (
    id                   uuid        PRIMARY KEY,
    received_at          timestamptz NOT NULL,
    provider             text        NOT NULL,
    provider_ref         text        NOT NULL,
    occurred_at          timestamptz NOT NULL,
    event_type           text        NOT NULL,
    provider_status      text,
    provider_payload_raw jsonb       NOT NULL,
    UNIQUE (provider, provider_ref, event_type, occurred_at)
);
CREATE INDEX ON webhook_receipt_staging (received_at);
