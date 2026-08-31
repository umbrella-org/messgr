-- Per-customer data-encryption keys (DESIGN.md §4.5, §7.1, §7.6). No
-- tenant_id column, matching 0001_producer.sql / 0002_tenant_config.sql
-- (§2.1) -- the tenant already is the database. shredded_at is written
-- only by the crypto-shredding erasure operation (step 15, not yet
-- built); this ticket creates the column, never writes it.
CREATE TABLE customer_dek (
    customer_id  uuid PRIMARY KEY,
    wrapped_dek  text NOT NULL,       -- Vault Transit ciphertext, "vault:v1:..." (§7.6); opaque -- never expose via queries or the API
    created_at   timestamptz NOT NULL,
    shredded_at  timestamptz          -- set when key destroyed; row retained as tombstone (step 15)
);
