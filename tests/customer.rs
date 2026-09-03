//! `customer` resolution integration suite (DESIGN.md §4.6, §4.7, T-015),
//! following `tests/customer_dek.rs`'s conventions: real provisioning
//! against the local stack, no mocks.

use std::num::NonZeroUsize;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use messgr::customer::resolve::{ResolutionInput, ResolveError, resolve};
use messgr::db;
use messgr::key_cache::KeyCache;
use messgr::keystore::VaultKeyStore;
use messgr::profile::Profile;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::provision::provision_tenant;

const TEST_PEPPER: &[u8] = b"test-pepper-does-not-need-to-be-vault-backed";
const DEFAULT_LOCALE: &str = "en-US";
const DEFAULT_TIMEZONE: &str = "UTC";

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

fn small_cache() -> KeyCache {
    KeyCache::new(NonZeroUsize::new(16).unwrap(), Duration::from_secs(60))
}

async fn drop_test_tenant(control_pool: &PgPool, database_name: &str, slug: &str) {
    let terminate = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}'"
    );
    let _ = sqlx::query(&terminate).execute(control_pool).await;
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database_name}\""))
        .execute(control_pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM tenant_schema_version WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM platform_audit WHERE tenant_id = (SELECT id FROM tenant WHERE slug = $1)",
    )
    .bind(slug)
    .execute(control_pool)
    .await;
    let _ = sqlx::query("DELETE FROM tenant WHERE slug = $1")
        .bind(slug)
        .execute(control_pool)
        .await;
}

struct Fixture {
    control_pool: PgPool,
    tenant_pool: PgPool,
    vault: VaultKeyStore,
    mount: String,
    slug: String,
    database_name: String,
}

async fn setup() -> Fixture {
    let control_url = control_database_url();
    let control_pool = db::connect(&control_url, 5)
        .await
        .expect("connecting to control database failed");
    let vault = vault_keystore();

    let slug = unique_name("test_tenant_customer");
    let database_name = unique_name("test_db_customer");

    let provision_outcome = provision_tenant(
        &control_pool,
        &control_url,
        &slug,
        "eu",
        &database_name,
        "test-actor",
        vault.client(),
    )
    .await
    .expect("provisioning test tenant failed");

    let tenant_pool = connect_tenant_pool(
        &control_pool,
        &control_url,
        provision_outcome.tenant_id,
        &database_name,
        5,
    )
    .await
    .expect("connecting tenant pool failed")
    .pool;
    let mount = format!("transit/{slug}");

    Fixture {
        control_pool,
        tenant_pool,
        vault,
        mount,
        slug,
        database_name,
    }
}

impl Fixture {
    async fn teardown(self) {
        self.tenant_pool.close().await;
        drop_test_tenant(&self.control_pool, &self.database_name, &self.slug).await;
    }
}

async fn address_rank(pool: &PgPool, address_id: Uuid) -> i16 {
    sqlx::query_scalar("SELECT rank FROM customer_address WHERE id = $1")
        .bind(address_id)
        .fetch_one(pool)
        .await
        .expect("address row must exist")
}

async fn customer_count(pool: &PgPool, customer_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM customer WHERE id = $1")
        .bind(customer_id)
        .fetch_one(pool)
        .await
        .expect("counting customer rows failed")
}

async fn total_customer_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM customer")
        .fetch_one(pool)
        .await
        .expect("counting customer rows failed")
}

#[tokio::test]
async fn explicit_customer_id_ranks_successive_addresses() {
    let fixture = setup().await;
    let cache = small_cache();
    let customer_id = Uuid::new_v4();

    let first = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15550100",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("first resolve failed");
    assert_eq!(
        address_rank(&fixture.tenant_pool, first.address_id).await,
        1
    );

    let second = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15550101",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("second resolve failed");
    assert_eq!(second.customer_id, customer_id);
    assert_eq!(
        address_rank(&fixture.tenant_pool, second.address_id).await,
        2
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn explicit_unknown_customer_id_mints_provisional_shell_once() {
    let fixture = setup().await;
    let cache = small_cache();
    let customer_id = Uuid::new_v4();

    let first = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15550200",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("first resolve failed");
    assert_eq!(
        first.customer_id, customer_id,
        "decision 6: mints under the exact caller-supplied id"
    );
    assert!(
        first.locale.is_none(),
        "a provisional customer has no non-default locale"
    );

    let second = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15550200",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("second resolve failed");
    assert_eq!(second.customer_id, customer_id);
    assert_eq!(
        second.address_id, first.address_id,
        "address must be reused, not re-created"
    );
    assert_eq!(customer_count(&fixture.tenant_pool, customer_id).await, 1);

    fixture.teardown().await;
}

#[tokio::test]
async fn unknown_external_id_mints_provisional_shell_once() {
    let fixture = setup().await;
    let cache = small_cache();
    let system = "core_banking";
    let external_id = unique_name("ext");

    let first = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::External {
            system: system.to_string(),
            external_id: external_id.clone(),
        },
        "+15550300",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("first resolve failed");

    let row: (Uuid,) = sqlx::query_as(
        "SELECT customer_id FROM customer_external_id WHERE system = $1 AND external_id = $2",
    )
    .bind(system)
    .bind(&external_id)
    .fetch_one(&fixture.tenant_pool)
    .await
    .expect("customer_external_id row must exist");
    assert_eq!(row.0, first.customer_id);

    let second = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::External {
            system: system.to_string(),
            external_id: external_id.clone(),
        },
        "+15550300",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("second resolve failed");
    assert_eq!(second.customer_id, first.customer_id);
    assert_eq!(
        customer_count(&fixture.tenant_pool, first.customer_id).await,
        1
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn address_only_unknown_destination_mints_provisional_shell_once() {
    let fixture = setup().await;
    let cache = small_cache();

    let first = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::AddressOnly,
        "+15550400",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("first resolve failed");

    let second = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::AddressOnly,
        "+15550400",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("second resolve failed");

    assert_eq!(second.customer_id, first.customer_id);
    assert_eq!(second.address_id, first.address_id);
    assert_eq!(
        customer_count(&fixture.tenant_pool, first.customer_id).await,
        1
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn alias_expansion_resolves_to_the_current_id() {
    let fixture = setup().await;
    let cache = small_cache();

    let current_id = Uuid::new_v4();
    let retired_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO customer (id, locale, timezone, provisional, source_system, source_updated_at, created_at) \
         VALUES ($1, $2, $3, false, 'core_banking', $4, $4)",
    )
    .bind(current_id)
    .bind(DEFAULT_LOCALE)
    .bind(DEFAULT_TIMEZONE)
    .bind(Utc::now())
    .execute(&fixture.tenant_pool)
    .await
    .expect("inserting current customer failed");

    sqlx::query(
        "INSERT INTO customer_alias (old_customer_id, customer_id, merged_at) VALUES ($1, $2, $3)",
    )
    .bind(retired_id)
    .bind(current_id)
    .bind(Utc::now())
    .execute(&fixture.tenant_pool)
    .await
    .expect("inserting customer_alias failed");

    let resolved = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(retired_id),
        "+15550500",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("resolve failed");

    assert_eq!(
        resolved.customer_id, current_id,
        "must resolve to the current id, not the retired one"
    );
    assert_eq!(
        customer_count(&fixture.tenant_pool, retired_id).await,
        0,
        "the retired id must never gain its own customer row"
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn concurrent_address_only_resolution_mints_exactly_one_provisional_customer() {
    let fixture = setup().await;
    let cache = small_cache();
    let before = total_customer_count(&fixture.tenant_pool).await;

    let call_one = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::AddressOnly,
        "+15550600",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let call_two = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::AddressOnly,
        "+15550600",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let (result_one, result_two) = tokio::join!(call_one, call_two);
    let result_one = result_one.expect("first concurrent resolve failed");
    let result_two = result_two.expect("second concurrent resolve failed");

    assert_eq!(result_one.customer_id, result_two.customer_id);
    assert_eq!(result_one.address_id, result_two.address_id);

    let after = total_customer_count(&fixture.tenant_pool).await;
    assert_eq!(
        after,
        before + 1,
        "exactly one provisional customer must be minted"
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn concurrent_external_id_resolution_mints_exactly_one_provisional_customer() {
    let fixture = setup().await;
    let cache = small_cache();
    let system = "core_banking";
    let external_id = unique_name("ext-race");
    let before = total_customer_count(&fixture.tenant_pool).await;

    let call_one = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::External {
            system: system.to_string(),
            external_id: external_id.clone(),
        },
        "+15550700",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let call_two = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::External {
            system: system.to_string(),
            external_id: external_id.clone(),
        },
        "+15550700",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let (result_one, result_two) = tokio::join!(call_one, call_two);
    let result_one = result_one.expect("first concurrent resolve failed");
    let result_two = result_two.expect("second concurrent resolve failed");

    assert_eq!(result_one.customer_id, result_two.customer_id);

    let after = total_customer_count(&fixture.tenant_pool).await;
    assert_eq!(
        after,
        before + 1,
        "exactly one provisional customer must be minted"
    );

    fixture.teardown().await;
}

#[tokio::test]
async fn concurrent_explicit_customer_id_resolution_does_not_error() {
    // Regression for review finding F1: two concurrent Explicit(id) mints
    // for the same never-before-seen id used to race a plain INSERT with no
    // ON CONFLICT, so the loser hit a raw unique-violation surfaced as
    // ResolveError::Database instead of a resolved customer.
    let fixture = setup().await;
    let cache = small_cache();
    let customer_id = Uuid::new_v4();

    let call_one = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15551200",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let call_two = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_id),
        "+15551200",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    );
    let (result_one, result_two) = tokio::join!(call_one, call_two);
    let result_one = result_one.expect("first concurrent resolve failed");
    let result_two = result_two.expect("second concurrent resolve failed");

    assert_eq!(result_one.customer_id, customer_id);
    assert_eq!(result_two.customer_id, customer_id);
    assert_eq!(customer_count(&fixture.tenant_pool, customer_id).await, 1);

    fixture.teardown().await;
}

#[tokio::test]
async fn conflicting_customer_ids_over_the_same_destination_is_rejected() {
    let fixture = setup().await;
    let cache = small_cache();
    let customer_a = Uuid::new_v4();
    let customer_b = Uuid::new_v4();

    let resolved_a = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_a),
        "+15550800",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await
    .expect("resolving customer A failed");

    let result_b = resolve(
        &fixture.tenant_pool,
        &fixture.vault,
        &cache,
        &fixture.mount,
        TEST_PEPPER,
        ResolutionInput::Explicit(customer_b),
        "+15550800",
        "sms",
        DEFAULT_LOCALE,
        DEFAULT_TIMEZONE,
    )
    .await;

    match result_b {
        Err(ResolveError::AddressConflict {
            existing_customer_id,
        }) => assert_eq!(existing_customer_id, resolved_a.customer_id),
        other => panic!("expected AddressConflict, got {other:?}"),
    }

    fixture.teardown().await;
}
