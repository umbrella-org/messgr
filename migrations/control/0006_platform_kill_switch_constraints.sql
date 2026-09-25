-- Platform kill switch constraints (DESIGN.md §5.2 "Two tiers in cloud",
-- §4.11, T-058). 0001 shipped `platform_kill_switch` with no constraints at
-- all, since nothing read or wrote it until T-058. Now that engage/release
-- exist, the table needs the same guarantees `kill_switch` has had since
-- T-016: a closed `scope` vocabulary, a `tenant_id` that agrees with it, and
-- at most one live switch per (scope, tenant).

ALTER TABLE platform_kill_switch
    ADD CONSTRAINT platform_kill_switch_scope_check
        CHECK (scope IN ('platform', 'tenant')),
    -- A region-wide switch names no tenant; a tenant switch always names one.
    ADD CONSTRAINT platform_kill_switch_scope_tenant_check
        CHECK ((scope = 'platform') = (tenant_id IS NULL)),
    -- Tenant rows are never deleted (offboarding only changes `status`,
    -- T-059), so this FK never blocks an offboard.
    ADD CONSTRAINT platform_kill_switch_tenant_fk
        FOREIGN KEY (tenant_id) REFERENCES tenant (id);

-- Same COALESCE trick as `kill_switch`'s own index (migrations/tenant/
-- 0009_kill_switch.sql): Postgres treats every NULL as distinct, so a bare
-- (scope, tenant_id) index would never catch two live region-wide switches.
-- Named explicitly, unlike 0009's, because `platform_kill_switch::configure`
-- targets it with ON CONFLICT.
CREATE UNIQUE INDEX platform_kill_switch_live_idx
    ON platform_kill_switch (scope, COALESCE(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid))
    WHERE released_at IS NULL;
