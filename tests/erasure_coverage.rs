//! CI check that every customer-linkable table is either covered by an
//! erasure redaction statement or a named, reasoned exemption (DESIGN.md
//! §7.2, §14, T-024). `COVERED`/`EXEMPT` below are a stand-in for
//! `src/erasure`'s eventual redaction statements — that module does not
//! exist yet (build order step 15) and is not created by this ticket.
//! Follows `tests/ledger_outbox_schema.rs`'s conventions: real provisioning
//! against the local stack, no mocks, no shared test-helpers module.

use sqlx::PgPool;
use uuid::Uuid;

use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;

fn control_database_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("CONTROL_DATABASE_URL")
        .expect("CONTROL_DATABASE_URL must be set for tests")
}

fn vault_keystore() -> VaultKeyStore {
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn drop_test_tenant(control_pool: &PgPool, database_name: &str, slug: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}'"
    );
    if let Err(err) = sqlx::query(&terminate).execute(control_pool).await {
        eprintln!("cleanup: failed to terminate backends on {database_name}: {err}");
    }

    let drop_db = format!("DROP DATABASE IF EXISTS \"{database_name}\"");
    if let Err(err) = sqlx::query(&drop_db).execute(control_pool).await {
        eprintln!("cleanup: failed to drop database {database_name}: {err}");
    }

    if let Err(err) = sqlx::query(
        "DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await
    {
        eprintln!("cleanup: failed to delete tenant_schema_version for {slug}: {err}");
    }

    if let Err(err) = sqlx::query(
        "DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await
    {
        eprintln!("cleanup: failed to delete platform_audit rows for {slug}: {err}");
    }

    if let Err(err) = sqlx::query("DELETE FROM tenant WHERE slug = $1")
        .bind(slug)
        .execute(control_pool)
        .await
    {
        eprintln!("cleanup: failed to delete tenant row for {slug}: {err}");
    }
}

async fn provision_test_tenant(
    control_pool: &PgPool,
    control_url: &str,
    vault: &VaultKeyStore,
    slug: &str,
    database_name: &str,
) -> Uuid {
    let outcome = provision_tenant(
        control_pool,
        control_url,
        slug,
        "eu",
        database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");
    outcome.tenant_id
}

struct TestTenant {
    control_pool: PgPool,
    tenant_pool: PgPool,
    slug: String,
    db_name: String,
}

impl TestTenant {
    async fn provision(prefix: &str) -> Self {
        let control_url = control_database_url();
        let control_pool = db::connect(&control_url, 5)
            .await
            .expect("failed to connect to control database");
        let vault = vault_keystore();

        let slug = unique_name(&format!("test_ec_{prefix}"));
        let db_name = unique_name(&format!("test_db_ec_{prefix}"));
        let tenant_id =
            provision_test_tenant(&control_pool, &control_url, &vault, &slug, &db_name)
                .await;

        let tenant_pool =
            connect_tenant_pool(&control_pool, &control_url, tenant_id, &db_name, 5)
                .await
                .expect("connecting tenant pool failed")
                .pool;

        TestTenant {
            control_pool,
            tenant_pool,
            slug,
            db_name,
        }
    }

    async fn teardown(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.db_name, &self.slug).await;
    }
}

/// Tables whose customer-linkable columns are physically redacted by
/// `src/erasure`'s (future) statements — DESIGN.md §7.2.
const COVERED: &[&str] = &[
    "comms_request",
    "comms_event",
    "customer_address",
    "customer_external_id",
    "consent",
];

/// Tables that match the detection rule but are deliberately not redacted,
/// each with its own stated reason (DESIGN.md §7.2) — never a blanket rule.
const EXEMPT: &[(&str, &str)] = &[
    (
        "suppression",
        "destination-scoped, not customer-scoped — a recycled number must stay blocked \
         regardless of who currently holds it (DESIGN.md §5, §7.2)",
    ),
    (
        "customer_dek",
        "destroying wrapped_dek is Mode 1's own crypto-shredding mechanism; Mode 2 doesn't \
         also need to touch it (§7.1, §7.2)",
    ),
    (
        "customer_alias",
        "holds only opaque customer-id UUIDs and a merge timestamp, no PII-bearing value to \
         redact (§7.2)",
    ),
    (
        "outbox",
        "customer_id here is routing/claim metadata; message content lives in comms_request, \
         not here (§4.2, §7.2)",
    ),
    (
        "orphan_event",
        "unmatched provider payload with no customer_id column yet; reconciliation, which \
         would resolve one, is a separate ticket (§4.4, §7.2)",
    ),
    (
        "customer",
        "locale/timezone are operational preferences and source_system/source_updated_at are \
         sync metadata, none of it personal information; caught only via the customer_id FK \
         other tables declare against it, not by its own columns (§7.2)",
    ),
];

async fn customer_linkable_tables(pool: &PgPool) -> Vec<String> {
    // comms_request/comms_event are declaratively partitioned by month
    // (T-014): information_schema.columns lists each monthly partition
    // (e.g. comms_request_2026_09) as its own table, so resolve a partition
    // to its parent's name via pg_inherits before classifying — COVERED/
    // EXEMPT name the logical table, not a bootstrap-dependent partition.
    //
    // A column-name match alone misses `customer` itself: it is the root
    // entity, so its own primary key is `id`, not `customer_id`, and it has
    // no `*_ciphertext`/`*_hmac`/`*_raw` column (T-024 review finding F1).
    // The second half of this UNION catches any table referenced by a
    // foreign key from a `customer_id` column — which surfaces `customer`
    // via customer_address/customer_external_id/customer_alias's own
    // declared constraints, without hand-naming it.
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT COALESCE(parent.relname, child.relname) AS table_name
        FROM information_schema.columns col
        JOIN pg_namespace ns ON ns.nspname = col.table_schema
        JOIN pg_class child ON child.relname = col.table_name AND child.relnamespace = ns.oid
        LEFT JOIN pg_inherits inh ON inh.inhrelid = child.oid
        LEFT JOIN pg_class parent ON parent.oid = inh.inhparent
        WHERE col.table_schema = 'public'
          AND (
                col.column_name = 'customer_id'
             OR col.column_name LIKE '%\_ciphertext' ESCAPE '\'
             OR col.column_name LIKE '%\_hmac' ESCAPE '\'
             OR col.column_name LIKE '%\_raw' ESCAPE '\'
          )

        UNION

        SELECT DISTINCT co.confrelid::regclass::text AS table_name
        FROM pg_constraint co
        JOIN pg_attribute att
          ON att.attrelid = co.conrelid AND att.attnum = ANY(co.conkey)
        WHERE co.contype = 'f' AND att.attname = 'customer_id'

        -- One hop removed: a table keyed on customer_address.id rather than
        -- customer_id directly (AGENTS.md invariant 4 -- e.g. `consent`,
        -- T-037) is still customer-linkable, just not reachable by either
        -- arm above. Without this, adding such a table to COVERED would trip
        -- the *stale-entry* assertion below instead (the table exists but
        -- this query wouldn't return it) -- this arm is what makes it
        -- visible to the check at all. Catches any future table following
        -- the same keying convention automatically, not just `consent`.
        UNION

        SELECT DISTINCT co.conrelid::regclass::text AS table_name
        FROM pg_constraint co
        WHERE co.contype = 'f' AND co.confrelid = 'customer_address'::regclass
        "#,
    )
    .fetch_all(pool)
    .await
    .expect("querying information_schema.columns for customer-linkable tables failed");

    rows.into_iter().map(|(name,)| name).collect()
}

/// Every real, top-level table in the schema (partitions collapsed to their
/// parent, same as `customer_linkable_tables`) — used to tell "manifest
/// entry names a table that doesn't exist yet" (fine — e.g. `suppression`
/// before its migration landed, T-038, decision 5) apart from "manifest
/// entry names a table that exists but no longer matches the detection
/// rule" (a real staleness bug).
async fn existing_tables(pool: &PgPool) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT COALESCE(parent.relname, child.relname) AS table_name
        FROM pg_class child
        JOIN pg_namespace ns ON ns.oid = child.relnamespace AND ns.nspname = 'public'
        LEFT JOIN pg_inherits inh ON inh.inhrelid = child.oid
        LEFT JOIN pg_class parent ON parent.oid = inh.inhparent
        WHERE child.relkind IN ('r', 'p')
        "#,
    )
    .fetch_all(pool)
    .await
    .expect("querying pg_class for existing tables failed");

    rows.into_iter().map(|(name,)| name).collect()
}

#[tokio::test]
async fn every_customer_linkable_table_is_covered_or_exempt() {
    let tenant = TestTenant::provision("coverage").await;

    let tables = customer_linkable_tables(&tenant.tenant_pool).await;

    let exempt_names: Vec<&str> = EXEMPT.iter().map(|(name, _)| *name).collect();
    let unclassified: Vec<&String> = tables
        .iter()
        .filter(|table| {
            !COVERED.contains(&table.as_str())
                && !exempt_names.contains(&table.as_str())
        })
        .collect();
    assert!(
        unclassified.is_empty(),
        "customer-linkable table(s) {unclassified:?} are neither in COVERED (an erasure \
         redaction statement) nor EXEMPT (a named, reasoned exemption) — add each to one list \
         or the other in tests/erasure_coverage.rs, and to DESIGN.md §7.2 if newly exempt"
    );

    // A manifest entry naming a table that doesn't exist yet is not stale —
    // e.g. `suppression` before its migration landed (T-038) — decision 5.
    // Only a table that DOES exist but wasn't returned by the detection
    // query (renamed, or its customer-linkable column dropped/renamed) is a
    // real staleness bug, same class as the orphan_event gap this ticket's
    // own history records.
    let present = existing_tables(&tenant.tenant_pool).await;
    let stale: Vec<&&str> = COVERED
        .iter()
        .chain(exempt_names.iter())
        .filter(|name| {
            present.contains(&name.to_string()) && !tables.contains(&name.to_string())
        })
        .collect();
    assert!(
        stale.is_empty(),
        "manifest entry/entries {stale:?} in COVERED/EXEMPT name a table that exists but no \
         longer matches the detection rule — the table may have been renamed or its \
         customer-linkable column dropped/renamed; remove or fix the stale entry in \
         tests/erasure_coverage.rs"
    );

    tenant.teardown().await;
}
