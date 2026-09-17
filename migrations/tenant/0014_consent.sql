-- Consent table (DESIGN.md §5, §4.4, T-037), verbatim from
-- development/design/03-data-model.md's own CREATE TABLE consent snippet.
-- Keyed on (address_id, class), never customer_id or the raw destination
-- (AGENTS.md hard invariant 4): customer_address rows are append-only, so
-- a recycled phone number's new address row starts with no consent record
-- of its own, and absence of consent defaults to opted-out for marketing
-- (§5) -- correct behaviour falls out of the schema, no cleanup job needed.
CREATE TABLE consent (
    address_id  uuid        NOT NULL REFERENCES customer_address(id),
    class       text        NOT NULL,  -- transactional | marketing
    opted_in    bool        NOT NULL,
    source      text        NOT NULL,  -- where the opt-in/out was captured, for evidence
    updated_at  timestamptz NOT NULL,
    PRIMARY KEY (address_id, class)
);
