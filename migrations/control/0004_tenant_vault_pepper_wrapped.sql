-- Per-tenant HMAC pepper, wrapped by that tenant's own Transit key
-- (DESIGN.md §7.6: "the destination_hmac pepper must be per-tenant,
-- derived from that tenant's own Transit mount"). Minted lazily on first
-- use (tenant_pepper::ensure_tenant_pepper, T-008), not at provisioning
-- time -- mirrors tenant_config's own "no auto-seeding" precedent (T-007
-- decision 4). Opaque ciphertext, same discipline as
-- customer_dek.wrapped_dek -- never expose via queries or the API.
ALTER TABLE tenant ADD COLUMN vault_pepper_wrapped text;
