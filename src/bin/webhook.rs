//! `messgr-webhook` (T-047): the one internet-facing binary in an otherwise
//! internal system (DESIGN.md §10). Signature verification and a single,
//! narrowly-scoped DB write -- nothing else. See `src/webhook/` for the
//! request path and `src/webhook_receipt/` for the internal promotion step
//! this binary never runs itself.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, Subcommand};

use messgr::config::Config;
use messgr::db;
use messgr::keystore::VaultKeyStore;
use messgr::mtls;
use messgr::webhook::handler::receive_webhook;
use messgr::webhook::{AppState, TenantPoolCache};
use messgr::webhook_verify::{HmacSha256Verifier, WebhookVerifier};

#[derive(Parser)]
#[command(name = "messgr-webhook")]
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
        println!("messgr-webhook {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let listen_addr: SocketAddr = std::env::var("WEBHOOK_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8543".to_string())
        .parse()
        .expect("WEBHOOK_LISTEN_ADDR must be a valid socket address");
    let health_listen_addr: SocketAddr = std::env::var("WEBHOOK_HEALTH_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8082".to_string())
        .parse()
        .expect("WEBHOOK_HEALTH_LISTEN_ADDR must be a valid socket address");
    let cert_file = env_var("WEBHOOK_TLS_CERT_FILE");
    let key_file = env_var("WEBHOOK_TLS_KEY_FILE");

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the control database");

    // Admin-token Vault client, same rationale as messgr-ingest's own
    // (T-011 decision 5) -- but only ever used for the KV secret read
    // (`read_webhook_secret`), never Transit: this binary holds no tenant's
    // decrypt/encrypt capability (T-047 decision 1).
    let vault_keystore =
        Arc::new(VaultKeyStore::connect(config.profile).expect(
            "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
        ));

    let verifier: Arc<dyn WebhookVerifier> = Arc::new(HmacSha256Verifier);

    let app_state = AppState {
        control_pool,
        control_database_url: config.control_database_url.clone(),
        vault_keystore,
        verifier,
        pool_cache: Arc::new(TenantPoolCache::new()),
        tenant_pool_max_connections: config.database_max_connections,
    };

    let app: Router = Router::new()
        .route("/webhook/{webhook_token}/{provider}", post(receive_webhook))
        .with_state(app_state);

    let tls_config = mtls::load_plain_server_config(&cert_file, &key_file).expect(
        "failed to load TLS material (WEBHOOK_TLS_CERT_FILE/WEBHOOK_TLS_KEY_FILE)",
    );
    let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));

    let mut handles = Vec::new();

    handles.push(tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(health_listen_addr)
            .await
            .expect("failed to bind WEBHOOK_HEALTH_LISTEN_ADDR");
        tracing::info!(%health_listen_addr, "messgr-webhook health listener up");
        axum::serve(listener, messgr::health::router())
            .await
            .expect("health server error");
    }));

    handles.push(tokio::spawn(async move {
        tracing::info!(%listen_addr, "messgr-webhook listening");
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
        let cli = Cli::try_parse_from(["messgr-webhook", "version"])
            .expect("parsing the version subcommand must succeed");

        assert!(matches!(cli.command, Some(Command::Version)));
    }
}
