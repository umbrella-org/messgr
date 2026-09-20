//! `messgr-query-api` (T-048): the one read-only, role-authenticated
//! surface over the ledger (DESIGN.md §11). A REST API plus a
//! server-rendered Askama+htmx UI in the same binary, reading from a
//! streaming replica so an unbounded compliance search cannot starve
//! ingestion. See `src/query_api/`, `src/comms_query/`, and `src/auth/`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, Subcommand};

use messgr::auth::mock::MockProvider;
use messgr::auth::provider::AuthProvider;
use messgr::config::Config;
use messgr::db;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::mtls;
use messgr::query_api::{self, AppState};
use messgr::webhook::TenantPoolCache;

#[derive(Parser)]
#[command(name = "messgr-query-api")]
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

/// Only legal value today (T-048 decision 1 — `OidcProvider` is a future
/// ticket).
const AUTH_PROVIDER_MOCK: &str = "mock";

#[tokio::main]
async fn main() {
    if matches!(Cli::parse().command, Some(Command::Version)) {
        println!("messgr-query-api {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let listen_addr: SocketAddr = std::env::var("QUERY_API_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8544".to_string())
        .parse()
        .expect("QUERY_API_LISTEN_ADDR must be a valid socket address");
    let health_listen_addr: SocketAddr = std::env::var("QUERY_API_HEALTH_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8083".to_string())
        .parse()
        .expect("QUERY_API_HEALTH_LISTEN_ADDR must be a valid socket address");
    let cert_file = env_var("QUERY_API_TLS_CERT_FILE");
    let key_file = env_var("QUERY_API_TLS_KEY_FILE");
    // Falls back to CONTROL_DATABASE_URL's value when unset -- dev/compose.yml
    // stands up one Postgres instance with no replica (T-048 decision 5).
    let query_api_database_url = std::env::var("QUERY_API_DATABASE_URL")
        .unwrap_or_else(|_| config.control_database_url.clone());

    let auth_provider_kind = std::env::var("AUTH_PROVIDER").unwrap_or_else(|_| {
        panic!(
            "AUTH_PROVIDER must be set (only legal value today: {AUTH_PROVIDER_MOCK:?})"
        )
    });
    if auth_provider_kind != AUTH_PROVIDER_MOCK {
        panic!(
            "AUTH_PROVIDER={auth_provider_kind:?} is not a recognized provider \
             (only legal value today: {AUTH_PROVIDER_MOCK:?} -- OidcProvider is a future ticket)"
        );
    }
    let mock_auth_actor = env_var("MOCK_AUTH_ACTOR");
    let mock_auth_role = env_var("MOCK_AUTH_ROLE");

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the control database");

    let auth: Arc<dyn AuthProvider> = Arc::new(MockProvider::new(
        config.profile,
        mock_auth_actor,
        mock_auth_role,
    ));

    let keystore: Arc<dyn KeyStore> =
        Arc::new(VaultKeyStore::connect(config.profile).expect(
            "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
        ));

    let app_state = AppState {
        control_pool,
        query_api_database_url,
        auth,
        pool_cache: Arc::new(TenantPoolCache::new()),
        tenant_pool_max_connections: config.database_max_connections,
        keystore,
    };

    let app = query_api::router(app_state);

    let tls_config = mtls::load_plain_server_config(&cert_file, &key_file).expect(
        "failed to load TLS material (QUERY_API_TLS_CERT_FILE/QUERY_API_TLS_KEY_FILE)",
    );
    let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));

    let mut handles = Vec::new();

    handles.push(tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(health_listen_addr)
            .await
            .expect("failed to bind QUERY_API_HEALTH_LISTEN_ADDR");
        tracing::info!(%health_listen_addr, "messgr-query-api health listener up");
        axum::serve(listener, messgr::health::router())
            .await
            .expect("health server error");
    }));

    handles.push(tokio::spawn(async move {
        tracing::info!(%listen_addr, "messgr-query-api listening");
        axum_server::bind_rustls(listen_addr, rustls_config)
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
        let cli = Cli::try_parse_from(["messgr-query-api", "version"])
            .expect("parsing the version subcommand must succeed");

        assert!(matches!(cli.command, Some(Command::Version)));
    }
}
