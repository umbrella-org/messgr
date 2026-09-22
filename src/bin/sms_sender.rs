//! `messgr-sms-sender` (T-052): the on-prem OTP fast path -- synchronous,
//! outside the queue/gate chain/dispatcher entirely (AGENTS.md hard
//! invariant 1; DESIGN.md §3). Terminates mTLS itself and resolves the
//! caller (the bank's own auth service, registered as an ordinary producer)
//! exactly as `messgr-ingest` does (`src/sms_sender/identity.rs`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::mtls::{self, ClientCertAcceptor};
use messgr::sms_sender::AppState;
use messgr::sms_sender::auth_flag::{self, AuthEnabledCache};
use messgr::sms_sender::buffer::run_drain_loop;
use messgr::sms_sender::handler::send_otp;
use messgr::sms_sender::pending::run_drain_loop as run_pending_drain_loop;
use messgr::sms_sender::provider::ProviderConfigCache;
use messgr::tenant::registry::TenantRegistry;

/// `tenant.auth_enabled`'s own poll cadence (decision 8) -- same interval
/// `messgr-ingest`'s kill-switch poll uses for a comparable reason.
const AUTH_FLAG_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Buffer drain cadence (task 6).
const BUFFER_DRAIN_INTERVAL: Duration = Duration::from_secs(10);
/// Pending-crypto drain cadence (F1 rework) -- same interval as the
/// write-retry buffer; no reason for these to differ.
const PENDING_DRAIN_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(name = "messgr-sms-sender")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the binary name and version, then exit.
    Version,
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::main]
async fn main() {
    if matches!(Cli::parse().command, Some(Command::Version)) {
        println!("messgr-sms-sender {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let listen_addr: SocketAddr = std::env::var("SMS_SENDER_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8445".to_string())
        .parse()
        .expect("SMS_SENDER_LISTEN_ADDR must be a valid socket address");
    let health_listen_addr: SocketAddr = std::env::var("SMS_SENDER_HEALTH_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8084".to_string())
        .parse()
        .expect("SMS_SENDER_HEALTH_LISTEN_ADDR must be a valid socket address");
    let cert_file = env_var("SMS_SENDER_TLS_CERT_FILE");
    let key_file = env_var("SMS_SENDER_TLS_KEY_FILE");
    let client_ca_file = env_var("SMS_SENDER_TLS_CLIENT_CA_FILE");
    // Dev stand-in for a real provider, same reasoning as
    // `DISPATCHER_<CHANNEL>_BASE_URL` -- `provider_config` has no
    // `base_url` column (see .env.example).
    let sms_base_url = env_var("SMS_SENDER_BASE_URL");
    let buffer_path = PathBuf::from(
        std::env::var("SMS_SENDER_BUFFER_PATH")
            .unwrap_or_else(|_| "sms-sender-buffer.jsonl".to_string()),
    );
    let pending_path = PathBuf::from(
        std::env::var("SMS_SENDER_PENDING_PATH")
            .unwrap_or_else(|_| "sms-sender-pending.jsonl".to_string()),
    );

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the control database");

    // Shared admin-token client, mirroring messgr-ingest (T-011 decision 5)
    // -- this is a multi-tenant process, unlike messgr-dispatcher's
    // single-tenant AppRole login.
    let keystore: Arc<dyn KeyStore> =
        Arc::new(VaultKeyStore::connect(config.profile).expect(
            "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
        ));

    let registry = Arc::new(TenantRegistry::new());
    registry.start_eviction_sweep(
        messgr::tenant::registry::TENANT_IDLE_TTL,
        messgr::tenant::registry::EVICTION_SWEEP_INTERVAL,
    );

    let auth_flag = Arc::new(AuthEnabledCache::new());
    if let Err(err) = auth_flag.refresh(&control_pool).await {
        tracing::error!(
            %err,
            "messgr-sms-sender: initial auth_enabled refresh failed, starting fail-open"
        );
    }

    let app_state = AppState {
        control_pool: control_pool.clone(),
        control_database_url: config.control_database_url.clone(),
        keystore: keystore.clone(),
        registry: registry.clone(),
        tenant_pool_max_connections: config.database_max_connections,
        profile: config.profile,
        auth_flag: auth_flag.clone(),
        provider_config_cache: Arc::new(ProviderConfigCache::new()),
        sms_base_url,
        buffer_path: buffer_path.clone(),
        pending_path: pending_path.clone(),
    };

    let app: Router = Router::new()
        .route("/otp", post(send_otp))
        .with_state(app_state);

    let tls_config = mtls::load_server_config(&cert_file, &key_file, &client_ca_file)
        .expect(
        "failed to load TLS material (SMS_SENDER_TLS_CERT_FILE/SMS_SENDER_TLS_KEY_FILE/SMS_SENDER_TLS_CLIENT_CA_FILE)",
    );
    let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));
    let acceptor = ClientCertAcceptor::new(RustlsAcceptor::new(rustls_config));

    let mut handles = Vec::new();

    handles.push(tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(health_listen_addr)
            .await
            .expect("failed to bind SMS_SENDER_HEALTH_LISTEN_ADDR");
        tracing::info!(%health_listen_addr, "messgr-sms-sender health listener up");
        axum::serve(listener, messgr::health::router())
            .await
            .expect("health server error");
    }));

    handles.push(tokio::spawn(auth_flag::run_refresh_loop(
        auth_flag,
        control_pool.clone(),
        AUTH_FLAG_POLL_INTERVAL,
    )));

    handles.push(tokio::spawn(run_drain_loop(
        buffer_path.clone(),
        control_pool.clone(),
        config.control_database_url.clone(),
        keystore.clone(),
        registry.clone(),
        config.database_max_connections,
        BUFFER_DRAIN_INTERVAL,
    )));

    handles.push(tokio::spawn(run_pending_drain_loop(
        pending_path,
        buffer_path,
        control_pool,
        config.control_database_url.clone(),
        keystore,
        registry,
        config.database_max_connections,
        PENDING_DRAIN_INTERVAL,
    )));

    handles.push(tokio::spawn(async move {
        tracing::info!(%listen_addr, "messgr-sms-sender listening");
        axum_server::bind(listen_addr)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
            .expect("server error");
    }));

    for handle in handles {
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_as_its_own_subcommand() {
        let cli = Cli::try_parse_from(["messgr-sms-sender", "version"])
            .expect("parsing the version subcommand must succeed");

        assert!(matches!(cli.command, Some(Command::Version)));
    }
}
