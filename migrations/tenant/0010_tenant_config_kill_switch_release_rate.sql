-- Kill-switch release ramp rate (DESIGN.md §5.2, T-016): how many held
-- outbox rows the release-drain task admits per second after a switch is
-- released, instead of dispatching a whole backlog at once. A fresh
-- migration rather than an edit to 0002_tenant_config.sql -- this is a new
-- column, not a defect correction (T-022 already has its own, unrelated
-- edits queued against that file).
ALTER TABLE tenant_config ADD COLUMN kill_switch_release_rate int NOT NULL DEFAULT 500;
