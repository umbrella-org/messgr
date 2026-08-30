-- First tenant-database migration (DESIGN.md §4.9). Registers the upstream
-- systems (fraud, statements, onboarding, ...) allowed to submit messages for
-- this tenant. No `tenant_id` column: this table lives inside the tenant's
-- own database, so the tenant already is the database (§2.1).
CREATE TABLE producer (
    id           uuid PRIMARY KEY,
    name         text UNIQUE NOT NULL,
    cert_subject text UNIQUE NOT NULL,   -- mTLS CN/SAN this producer authenticates with
    owner_team   text NOT NULL,
    contact      text NOT NULL,          -- who to page when its quota alerts fire
    enabled      bool NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL
);
