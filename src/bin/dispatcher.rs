//! `messgr-dispatcher` (T-013): a per-tenant, per-channel claim loop that
//! turns an `outbox` row into a `comms_event` + `final_status` write. Two
//! processes may run per tenant for HA (DESIGN.md §9); `pg_try_advisory_lock`
//! on a direct connection (T-039) elects exactly one as active, and the
//! standby idles and retries every few seconds until it can take over.
//!
//! T-021 adds retry with backoff for retryable send failures and a sweep
//! that clears any stale lease left by a crashed prior run, now run once per
//! acquired leadership (T-039) rather than once at raw process start --
//! winning the advisory lock is what guarantees the previous leader's
//! session, and therefore its leases, are truly gone.
//!
//! T-016 adds kill-switch enforcement: a shared `KillSwitchCache`, refreshed
//! by a dedicated `LISTEN kill_switch` connection with a 30-second poll
//! fallback (DESIGN.md §5.2), feeds every channel's claim-exclusion check
//! and drives the release-drain / engage-discard one-shot tasks.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::dispatcher::drain::{discard_engaged_scope, run_release_drain};
use messgr::dispatcher::repo;
use messgr::dispatcher::worker::{DispatcherContext, run_channel_loop};
use messgr::key_cache::KeyCache;
use messgr::keystore::{KeyStore, VaultKeyStore, split_kv_path};
use messgr::kill_switch::cache::{KillSwitchCache, run_refresh_loop};
use messgr::kill_switch::model::on_queued;
use messgr::producer_quota::tracker::QuotaTracker;
use messgr::provider_config::repo as provider_config_repo;
use messgr::sender::Sender;
use messgr::sender::http::HttpSender;
use messgr::tenant::pool::connect_tenant_pool;
use messgr::tenant::repo as tenant_repo;
use messgr::tenant_config::repo as tenant_config_repo;

/// `key_cache.rs`'s own doc comment recommends this exact sizing for
/// "T-013's dispatcher".
const DEK_CACHE_CAPACITY: usize = 100_000;
const DEK_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Matches `migrations/tenant/0010_tenant_config_kill_switch_release_rate.sql`'s
/// own column default -- used when a tenant has no `tenant_config` row at
/// all (T-007 decision 4: no auto-seeding).
const DEFAULT_KILL_SWITCH_RELEASE_RATE: i32 = 500;
/// Matches `migrations/tenant/0002_tenant_config.sql`'s own
/// `verification_mode` column default (T-036) -- used when a tenant has no
/// `tenant_config` row at all (T-007 decision 4: no auto-seeding).
const DEFAULT_VERIFICATION_MODE: &str = "observe";
const KILL_SWITCH_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Used when a tenant has no `tenant_config` row at all (T-007 decision 4:
/// no auto-seeding) -- matches `migrations/tenant/0002_tenant_config.sql`'s
/// own `quota_day_boundary_tz` column, which has no default and is always
/// backed by a real `tenant_config` row once one exists.
const DEFAULT_QUOTA_DAY_BOUNDARY_TZ: &str = "UTC";
const USAGE_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const QUOTA_CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(30);

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[derive(Parser)]
#[command(name = "messgr-dispatcher")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the binary name and version, then exit.
    Version,
}

#[tokio::main]
async fn main() {
    if matches!(Cli::parse().command, Some(Command::Version)) {
        println!("messgr-dispatcher {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let health_listen_addr: std::net::SocketAddr =
        std::env::var("DISPATCHER_HEALTH_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8081".to_string())
            .parse()
            .expect("DISPATCHER_HEALTH_LISTEN_ADDR must be a valid socket address");

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
        &control_pool,
        &config.control_database_url,
        tenant.id,
        &tenant.database_name,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the tenant database")
    .pool;

    // Per-tenant AppRole login (T-013 decision 7) -- this is the
    // single-tenant-per-process case `connect_as_tenant`'s own doc comment
    // names, unlike messgr-ingest's shared admin-token connection (T-011
    // decision 5). Kept as the concrete type until after the per-channel
    // credential resolution below (T-023 decision 5) -- `read_provider_credential`
    // is an inherent method, not on the `KeyStore` trait `keystore` is
    // narrowed to once wrapped.
    let vault_keystore = VaultKeyStore::connect_as_tenant(config.profile)
        .await
        .expect(
            "failed to log in to Vault as this tenant's AppRole \
             (VAULT_ROLE_ID/VAULT_WRAPPED_SECRET_ID must be set)",
        );

    let cache = Arc::new(KeyCache::new(
        NonZeroUsize::new(DEK_CACHE_CAPACITY).expect("DEK_CACHE_CAPACITY is nonzero"),
        DEK_CACHE_TTL,
    ));

    let tenant_config = tenant_config_repo::load(&tenant_pool)
        .await
        .expect("loading tenant_config failed");
    let release_rate = tenant_config
        .as_ref()
        .map(|c| c.kill_switch_release_rate)
        .unwrap_or(DEFAULT_KILL_SWITCH_RELEASE_RATE) as i64;
    let verification_mode = tenant_config
        .as_ref()
        .map(|c| c.verification_mode.clone())
        .unwrap_or_else(|| DEFAULT_VERIFICATION_MODE.to_string());
    let quota_day_boundary_tz = tenant_config
        .as_ref()
        .map(|c| c.quota_day_boundary_tz.clone())
        .unwrap_or_else(|| DEFAULT_QUOTA_DAY_BOUNDARY_TZ.to_string());
    let quota = Arc::new(QuotaTracker::new(&quota_day_boundary_tz));

    let kill_switches = Arc::new(KillSwitchCache::new());
    let draining = Arc::new(RwLock::new(HashMap::new()));

    // Two passes: resolve every channel's credential from Vault via the
    // still-concrete `vault_keystore` first, then wrap it into the shared
    // `Arc<dyn KeyStore>` `DispatcherContext` needs (T-023 decision 5) --
    // `read_provider_credential` is inherent on `VaultKeyStore`, not on the
    // `KeyStore` trait, so it must run before the value is moved into `Arc`.
    let mut channel_senders = Vec::new();
    for channel in &channels {
        let upper = channel.to_uppercase();
        let base_url = env_var(&format!("DISPATCHER_{upper}_BASE_URL"));

        let configs = provider_config_repo::list(&tenant_pool, channel)
            .await
            .expect("loading provider_config failed");
        let top = configs.first().unwrap_or_else(|| {
            panic!("no provider_config row for channel {channel:?} (tenant {tenant_slug:?})")
        });
        let (kv_mount, kv_path) = split_kv_path(&top.credential_path).unwrap_or_else(|| {
            panic!(
                "provider_config.credential_path {:?} for channel {channel:?} is not in \
                 <mount>/data/<path> form",
                top.credential_path
            )
        });
        let api_key = vault_keystore
            .read_provider_credential(kv_mount, kv_path)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "reading Vault credential at {:?} for channel {channel:?} failed: {err}",
                    top.credential_path
                )
            });

        channel_senders.push((channel.clone(), base_url, api_key));
    }

    let keystore: Arc<dyn KeyStore> = Arc::new(vault_keystore);

    let mut contexts = HashMap::new();
    for (channel, base_url, api_key) in channel_senders {
        let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(base_url, api_key));

        contexts.insert(
            channel,
            Arc::new(DispatcherContext {
                pool: tenant_pool.clone(),
                keystore: keystore.clone(),
                cache: cache.clone(),
                mount: tenant.vault_mount.clone(),
                sender,
                verification_mode: verification_mode.clone(),
                kill_switches: kill_switches.clone(),
                draining: draining.clone(),
                quota_day_boundary_tz: quota_day_boundary_tz.clone(),
                quota: quota.clone(),
            }),
        );
    }

    let mut handles = Vec::new();

    handles.push(tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(health_listen_addr)
            .await
            .expect("failed to bind DISPATCHER_HEALTH_LISTEN_ADDR");
        tracing::info!(%health_listen_addr, "messgr-dispatcher health listener up");
        axum::serve(listener, messgr::health::router())
            .await
            .expect("health server error");
    }));

    let mut refresh_listener = sqlx::postgres::PgListener::connect_with(&tenant_pool)
        .await
        .expect("messgr-dispatcher: failed to open the kill-switch LISTEN connection");
    refresh_listener
        .listen("kill_switch")
        .await
        .expect("messgr-dispatcher: failed to LISTEN on kill_switch");

    let refresh_pool = tenant_pool.clone();
    let refresh_cache = kill_switches.clone();
    let refresh_contexts = contexts.clone();
    let refresh_draining = draining.clone();
    handles.push(tokio::spawn(async move {
        run_refresh_loop(
            refresh_cache,
            refresh_pool.clone(),
            Some(refresh_listener),
            KILL_SWITCH_POLL_INTERVAL,
            move |delta| {
                for switch in delta.newly_engaged {
                    if switch.on_queued == on_queued::DISCARD {
                        tracing::info!(
                            kill_switch_id = %switch.id,
                            scope = %switch.scope,
                            "messgr-dispatcher: discarding backlog for newly-engaged switch"
                        );
                        tokio::spawn(discard_engaged_scope(
                            refresh_pool.clone(),
                            switch,
                            release_rate,
                        ));
                    }
                }
                for switch in delta.released {
                    if switch.on_queued == on_queued::HOLD {
                        tracing::info!(
                            kill_switch_id = %switch.id,
                            scope = %switch.scope,
                            "messgr-dispatcher: draining backlog for released switch"
                        );
                        // Inserted here, synchronously, in the same tick
                        // `KillSwitchCache::refresh` removed this switch from
                        // the *engaged* set — not inside the spawned task
                        // below. A `tokio::spawn` only starts running at its
                        // own next poll, so doing this insert there would
                        // leave a window where the switch is excluded by
                        // neither map and the normal claim loop could claim
                        // its whole backlog at once (T-016 decision 4's own
                        // plan-amendment note).
                        refresh_draining
                            .write()
                            .expect("draining lock poisoned")
                            .insert(switch.id, switch.clone());

                        let draining = refresh_draining.clone();
                        let contexts = refresh_contexts.clone();
                        tokio::spawn(run_release_drain(contexts, draining, switch, release_rate));
                    }
                }
            },
            None,
        )
        .await;
    }));

    // Producer quota config/override cache refresh (DESIGN.md §5.1, T-042) --
    // read-only and safe to run before leadership (decision 12), like the
    // kill-switch refresh loop above. No `LISTEN` channel: config changes
    // are rare and 30s staleness is acceptable here, unlike kill switches.
    let quota_refresh = quota.clone();
    let quota_refresh_pool = tenant_pool.clone();
    handles.push(tokio::spawn(async move {
        loop {
            if let Err(err) = quota_refresh
                .refresh_config(&quota_refresh_pool, Utc::now())
                .await
            {
                tracing::error!(%err, "messgr-dispatcher: producer quota config refresh failed");
            }
            tokio::time::sleep(QUOTA_CONFIG_POLL_INTERVAL).await;
        }
    }));

    // T-039: block here, after everything that runs regardless of
    // leadership (health listener, kill-switch refresh loop, Vault login,
    // credential/config loads) is already up, and only before the sweep and
    // claim loops that a standby must not run. `_leadership` stays bound for
    // the rest of `main` -- dropping it early would release the lock out
    // from under an otherwise-healthy leader.
    let leader_options =
        db::with_database_name(&config.control_database_url, &tenant.database_name)
            .expect("deriving the leader-lock connection options failed");
    let _leadership = messgr::dispatcher::leader::acquire(leader_options).await;
    tracing::info!("messgr-dispatcher: acquired tenant leadership");

    let cleared_leases = repo::clear_stale_leases(&tenant_pool)
        .await
        .expect("clearing stale outbox leases failed");
    tracing::info!(
        cleared_leases,
        "messgr-dispatcher: cleared stale outbox leases after acquiring leadership"
    );

    // Producer quota usage counters (DESIGN.md §5.1, T-042) -- rebuilt from
    // producer_usage only after leadership is acquired (decision 12): a
    // standby's tracker never has real counts, and a racing flush from one
    // would corrupt the real leader's producer_usage rows.
    quota
        .rebuild_from_db(&tenant_pool, Utc::now())
        .await
        .expect("rebuilding producer quota counters from producer_usage failed");

    let quota_flush = quota.clone();
    let quota_flush_pool = tenant_pool.clone();
    handles.push(tokio::spawn(async move {
        loop {
            tokio::time::sleep(USAGE_FLUSH_INTERVAL).await;
            if let Err(err) = quota_flush.flush(&quota_flush_pool).await {
                tracing::error!(%err, "messgr-dispatcher: producer quota usage flush failed");
            }
        }
    }));

    for (channel, ctx) in contexts {
        tracing::info!(%channel, "messgr-dispatcher: starting claim loop");
        handles.push(tokio::spawn(run_channel_loop(ctx, channel)));
    }

    for handle in handles {
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_as_its_own_subcommand() {
        let cli = Cli::try_parse_from(["messgr-dispatcher", "version"])
            .expect("parsing the version subcommand must succeed");

        assert!(matches!(cli.command, Some(Command::Version)));
    }
}
