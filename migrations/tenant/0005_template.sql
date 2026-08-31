-- Template store (DESIGN.md §4.4, T-010): immutable versioned template
-- bodies with mandatory approval metadata. No tenant_id column, matching
-- 0002_tenant_config.sql / 0004_ledger_outbox_schema.sql (§2.1) -- the
-- tenant already is the database. No REFERENCES/CHECK beyond what the
-- design's own CREATE TABLE declares. No status/draft column -- every
-- row is already approved (approved_by/approved_at NOT NULL); see T-010
-- decision 1.

CREATE TABLE template (
    template_id text        NOT NULL,
    version     int         NOT NULL,
    channel     text        NOT NULL,
    locale      text        NOT NULL,
    body        text        NOT NULL,
    approved_by text        NOT NULL,
    approved_at timestamptz NOT NULL,
    PRIMARY KEY (template_id, version, locale)
);
