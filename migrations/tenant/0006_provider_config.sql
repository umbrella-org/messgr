-- Per-channel ordered provider list (DESIGN.md §4.10, §12.1), tenant
-- database. No tenant_id column, matching 0001_producer.sql and
-- 0002_tenant_config.sql (§2.1 — the tenant already is the database);
-- DESIGN.md's own §4.10 snippet is corrected to match in this ticket's
-- Docs step (T-012 decision 2).
CREATE TABLE provider_config (
    channel            text     NOT NULL,
    priority           smallint NOT NULL,
    provider           text     NOT NULL,       -- free-text label; no vendor wired yet (T-012 decision 1)
    credential_path    text     NOT NULL,       -- Vault path; not read yet (T-012 decision 3)
    rate_limit_per_sec int      NOT NULL,       -- not enforced yet (T-012 decision 4)
    PRIMARY KEY (channel, priority)
);
