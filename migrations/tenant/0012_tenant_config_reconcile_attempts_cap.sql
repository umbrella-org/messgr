-- Orphan-reconcile attempts cap (DESIGN.md §4.4/§10 correction note, T-033): how many
-- reconcile passes a pending orphan_event row survives before it's aged out and deleted.
-- Was a hardcoded RECONCILE_ATTEMPTS_CAP constant in src/orphan_reconcile/reconcile.rs;
-- moved here so an operator can tune it, per the design's own "config, not hardcoded"
-- correction. A fresh migration, not an edit to 0002_tenant_config.sql, matching 0010's
-- own precedent for a later-added tunable.
ALTER TABLE tenant_config ADD COLUMN reconcile_attempts_cap smallint NOT NULL DEFAULT 5;
