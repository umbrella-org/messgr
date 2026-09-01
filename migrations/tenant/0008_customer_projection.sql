-- Customer projection schema (DESIGN.md §4.6, T-015): customer,
-- customer_external_id, customer_address, customer_alias, verbatim from
-- the design's own CREATE TABLE statements, including the
-- (kind, value_hmac) unique index correction found during this ticket's
-- refinement. No tenant_id column on any of the four tables, matching
-- every other tenant-database table (§2.1 -- the tenant already is the
-- database).

CREATE TABLE customer (
    id                uuid PRIMARY KEY,
    locale            text NOT NULL,          -- selects template locale (§4.4)
    timezone          text NOT NULL,          -- quiet hours + local-time scheduling (§6)
    provisional       bool NOT NULL DEFAULT false,
    source_system     text,
    source_updated_at timestamptz,            -- staleness bound; NULL for provisional
    created_at        timestamptz NOT NULL
);

CREATE TABLE customer_external_id (
    customer_id uuid NOT NULL REFERENCES customer(id),
    system      text NOT NULL,                -- core_banking | crm | digital | cards
    external_id text NOT NULL,
    PRIMARY KEY (system, external_id)
);
CREATE INDEX ON customer_external_id (customer_id);

CREATE TABLE customer_address (
    id                uuid PRIMARY KEY,
    customer_id       uuid NOT NULL REFERENCES customer(id),
    kind              text NOT NULL,          -- email | msisdn | whatsapp | push | postal
    value_ciphertext  bytea NOT NULL,         -- under customer DEK (§7)
    value_hmac        bytea NOT NULL,         -- keyed, pepper in Vault; same as destination_hmac
    rank              smallint NOT NULL,      -- 1 = primary, 2 = secondary, ...
    label             text,
    verified_at       timestamptz,
    active_from       timestamptz NOT NULL,
    active_to         timestamptz,            -- NULL = current
    source_updated_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX ON customer_address (customer_id, kind, rank) WHERE active_to IS NULL;
CREATE UNIQUE INDEX ON customer_address (kind, value_hmac) WHERE active_to IS NULL;
CREATE INDEX ON customer_address (value_hmac, active_from);
CREATE INDEX ON customer_address (customer_id) WHERE active_to IS NULL;

CREATE TABLE customer_alias (               -- master-system merges; ledger stays immutable
    old_customer_id uuid PRIMARY KEY,
    customer_id     uuid NOT NULL REFERENCES customer(id),
    merged_at       timestamptz NOT NULL
);
