//! `messgr-ingest` (T-011): `POST /comms`, terminating mTLS itself and
//! resolving the verified client certificate's subject to a
//! `(tenant_id, producer_id)` pair via `producer::resolve::resolve_producer`
//! (T-006). See `src/mtls.rs` and `src/ingest/` for the request path.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};

use messgr::config::Config;
use messgr::db;
use messgr::ingest::AppState;
use messgr::ingest::handler::create_comms;
use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::mtls::{self, ClientCertAcceptor};
use messgr::tenant::registry::TenantRegistry;

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    let config = Config::from_env();

    let listen_addr: SocketAddr = std::env::var("INGEST_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8443".to_string())
        .parse()
        .expect("INGEST_LISTEN_ADDR must be a valid socket address");
    let cert_file = env_var("INGEST_TLS_CERT_FILE");
    let key_file = env_var("INGEST_TLS_KEY_FILE");
    let client_ca_file = env_var("INGEST_TLS_CLIENT_CA_FILE");

    let control_pool = db::connect(
        &config.control_database_url,
        config.database_max_connections,
    )
    .await
    .expect("failed to connect to the control database");

    // Shared admin-token client for every tenant's Transit mount (T-011
    // decision 5) — `connect_as_tenant`'s per-tenant AppRole login is scoped
    // to a single-tenant-per-process dispatcher deployment and doesn't fit
    // this multi-tenant ingest process.
    let keystore: Arc<dyn KeyStore> =
        Arc::new(VaultKeyStore::connect(config.profile).expect(
            "failed to connect to Vault (has VAULT_ADDR/VAULT_TOKEN been set?)",
        ));

    let app_state = AppState {
        control_pool,
        control_database_url: config.control_database_url.clone(),
        keystore,
        registry: Arc::new(TenantRegistry::new()),
        tenant_pool_max_connections: config.database_max_connections,
        profile: config.profile,
    };

    let app: Router = Router::new()
        .route("/comms", post(create_comms))
        .with_state(app_state);

    let tls_config = mtls::load_server_config(&cert_file, &key_file, &client_ca_file)
        .expect("failed to load TLS material (INGEST_TLS_CERT_FILE/INGEST_TLS_KEY_FILE/INGEST_TLS_CLIENT_CA_FILE)");
    let rustls_config = RustlsConfig::from_config(Arc::new(tls_config));
    let acceptor = ClientCertAcceptor::new(RustlsAcceptor::new(rustls_config));

    tracing::info!(%listen_addr, "messgr-ingest listening");
    axum_server::bind(listen_addr)
        .acceptor(acceptor)
        .serve(app.into_make_service())
        .await
        .expect("server error");
}
