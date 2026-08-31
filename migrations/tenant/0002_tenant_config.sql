-- Tenant-scoped typed configuration (DESIGN.md §4.10, T-007): one row per
-- tenant, singleton-enforced. No tenant_id column, matching
-- 0001_producer.sql (§2.1) — the tenant already is the database.
-- display_name and the oidc_* columns from DESIGN.md's full §4.10 table are
-- deliberately not created yet: no ticket reads them (T-035 adds the oidc_*
-- columns when real OIDC lands).
CREATE TABLE tenant_config (
    singleton             boolean  NOT NULL DEFAULT true,
    retention_years       int      NOT NULL,
    default_timezone      text     NOT NULL,
    default_locale        text     NOT NULL,
    schedule_horizon_days int      NOT NULL DEFAULT 90,
    quota_day_boundary_tz text     NOT NULL,
    verification_mode     text     NOT NULL DEFAULT 'observe',  -- enforce | observe (§5)
    staleness_max_age     interval NOT NULL,                    -- projection freshness bound (§4.8)
    CONSTRAINT tenant_config_singleton CHECK (singleton),
    PRIMARY KEY (singleton)
);
