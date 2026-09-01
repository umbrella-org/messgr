//! `messgr-dispatcher` (T-013): a per-tenant, per-channel claim loop that
//! turns an `outbox` row into a `comms_event` + `final_status` write. One
//! process per tenant (DESIGN.md §9) -- leader election
//! (`pg_try_advisory_lock`) and retry/backoff are later tickets (build
//! order step 7); this binary runs exactly one instance per tenant and
//! treats a send failure as terminal on the first attempt (T-013 decision
//! 2).

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use messgr::config::Config;
use messgr::db;
use messgr::dispatcher::worker::{DispatcherContext, run_channel_loop};
use messgr::key_cache::KeyCache;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::repo as tenant_repo;

/// `key_cache.rs`'s own doc comment recommends this exact sizing for
/// "T-013's dispatcher".
const DEK_CACHE_CAPACITY: usize = 100_000;
const DEK_CACHE_TTL: Duration = Duration::from_secs(3600);

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let tenant_slug = env_var("DISPATCHER_TENANT_SLUG");
    let channels: Vec<String> = std::env::var("DISPATCHER_CHANNELS")
        .unwrap_or_else(|_| "sms".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the control database");

    let tenant = tenant_repo::find_by_slug(&control_pool, &tenant_slug)
        .await
        .expect("tenant lookup failed")
        .unwrap_or_else(|| panic!("no tenant registered with slug {tenant_slug:?}"));

    let tenant_pool = connect_tenant_pool(
        &config.control_database_url,
        &tenant.database_name,
        config.database_max_connections,
        config.profile,
    )
    .await
    .expect("failed to connect to the tenant database");

    // Per-tenant AppRole login (T-013 decision 7) -- this is the
    // single-tenant-per-process case `connect_as_tenant`'s own doc comment
    // names, unlike messgr-ingest's shared admin-token connection (T-011
    // decision 5).
    let keystore: Arc<dyn KeyStore> = Arc::new(
        VaultKeyStore::connect_as_tenant(config.profile)
            .await
            .expect(
                "failed to log in to Vault as this tenant's AppRole \
                 (VAULT_ROLE_ID/VAULT_WRAPPED_SECRET_ID must be set)",
            ),
    );

    let cache = Arc::new(KeyCache::new(
        NonZeroUsize::new(DEK_CACHE_CAPACITY).expect("DEK_CACHE_CAPACITY is nonzero"),
        DEK_CACHE_TTL,
    ));

    let mut handles = Vec::new();
    for channel in channels {
        let upper = channel.to_uppercase();
        let base_url = env_var(&format!("DISPATCHER_{upper}_BASE_URL"));
        let api_key = env_var(&format!("DISPATCHER_{upper}_API_KEY"));
        let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(base_url, api_key));

        let ctx = Arc::new(DispatcherContext {
            pool: tenant_pool.clone(),
            keystore: keystore.clone(),
            cache: cache.clone(),
            mount: tenant.vault_mount.clone(),
            sender,
        });

        tracing::info!(%channel, "messgr-dispatcher: starting claim loop");
        handles.push(tokio::spawn(run_channel_loop(ctx, channel)));
    }

    for handle in handles {
        let _ = handle.await;
    }
}
