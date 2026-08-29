-- Adds the public Vault AppRole RoleID issued per tenant (DESIGN.md §7.6, §11.4).
-- RoleID is not a secret (Vault's own docs describe it as behaving like a
-- username) and is safe to persist and query. The corresponding SecretID is
-- deliberately NOT a column here: it is generated response-wrapped at
-- provisioning time and printed once to the operator's terminal for
-- out-of-band delivery to the tenant's dispatcher deployment (§7.6:
-- "SecretID delivered response-wrapped at deploy time"). Persisting it in
-- Postgres would defeat the point of wrapping it.
ALTER TABLE tenant ADD COLUMN vault_role_id text UNIQUE;
