-- Control database schema (DESIGN.md §4.11).
-- One per region, outside every tenant database. Holds no customer data and no
-- message content — tenant lifecycle, cert→tenant resolution, schema-drift
-- tracking, and platform-level (cross-tenant) controls only.

CREATE TABLE tenant (
    id             uuid PRIMARY KEY,
    slug           text UNIQUE NOT NULL,        -- routing key for the tenant UI hostname
    region         text NOT NULL,               -- must match this control DB's region; asserted on boot
    database_name  text UNIQUE NOT NULL,
    vault_mount    text UNIQUE NOT NULL,        -- per-tenant Transit mount (§7.6)
    webhook_token  text UNIQUE NOT NULL,        -- opaque; provider callback path (§10). Never the slug
    status         text NOT NULL,               -- provisioning | active | suspended
                                                -- | offboarding_archive | offboarding_destroy  (§7.7)
    created_at     timestamptz NOT NULL
);

-- mTLS producer certs resolve to a tenant BEFORE any tenant database is opened.
-- Without this, ingest cannot know which database to look the producer up in.
CREATE TABLE producer_cert (
    cert_subject text PRIMARY KEY,              -- CN/SAN presented by the producer
    tenant_id    uuid NOT NULL REFERENCES tenant(id),
    producer_id  uuid NOT NULL                  -- resolved within that tenant's producer table
);

CREATE TABLE tenant_schema_version (             -- migrations run N times; drift must be visible
    tenant_id  uuid PRIMARY KEY REFERENCES tenant(id),
    version    int NOT NULL,
    applied_at timestamptz NOT NULL
);

CREATE TABLE platform_kill_switch (              -- operator-level; overrides tenant switches (§5.2)
    id          uuid PRIMARY KEY,
    scope       text NOT NULL,                   -- platform | tenant
    tenant_id   uuid,
    engaged_by  text NOT NULL,
    engaged_at  timestamptz NOT NULL,
    reason      text NOT NULL,
    released_by text,
    released_at timestamptz
);

CREATE TABLE platform_audit (                    -- provisioning, suspension, break-glass
    id         uuid PRIMARY KEY,
    actor      text NOT NULL,
    action     text NOT NULL,
    tenant_id  uuid,
    detail     jsonb NOT NULL,
    at         timestamptz NOT NULL
);
