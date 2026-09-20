-- Compliance access-audit log (DESIGN.md §11.1, T-048): every query-api
-- access made under the `compliance` role is recorded here. Not
-- partitioned -- one row per compliance query, nowhere near ledger scale.
-- Named exemption from erasure (tests/erasure_coverage.rs, T-048 decision
-- 8): this is evidence of what a compliance user did, not the customer's
-- own data, so it outlives the record it describes -- same reasoning as
-- suppression/orphan_event's existing exemptions.
CREATE TABLE access_audit (
    id            uuid        PRIMARY KEY,
    occurred_at   timestamptz NOT NULL,
    actor         text        NOT NULL,
    role          text        NOT NULL,
    route         text        NOT NULL,
    customer_id   uuid,                  -- NULL for a query naming no single customer
    query_params  text        NOT NULL DEFAULT ''
);
CREATE INDEX ON access_audit (occurred_at);
CREATE INDEX ON access_audit (customer_id) WHERE customer_id IS NOT NULL;
