//! Dev PKI integration suite (DESIGN.md §4.9, §11.1, T-006), following
//! `tests/tenant_vault.rs`'s conventions: real dev-mode Vault, no mocks.

use messgr::keystore::VaultKeyStore;
use messgr::producer::dev_pki;
use messgr::profile::Profile;
use vaultrs::pki::issuer;

fn vault_keystore() -> VaultKeyStore {
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

#[tokio::test]
async fn bootstrap_is_idempotent_and_mints_exactly_one_root_ca() {
    let vault = vault_keystore();

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("first bootstrap failed");
    let first_issuers = issuer::list(vault.client(), "pki")
        .await
        .expect("listing issuers failed")
        .keys;
    assert_eq!(
        first_issuers.len(),
        1,
        "bootstrap must mint exactly one root CA"
    );

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("second (idempotent) bootstrap failed");
    let second_issuers = issuer::list(vault.client(), "pki")
        .await
        .expect("listing issuers failed")
        .keys;
    assert_eq!(
        second_issuers, first_issuers,
        "re-running bootstrap must not mint a second root CA"
    );
}

#[tokio::test]
async fn issue_cert_returns_a_usable_certificate_and_key() {
    let vault = vault_keystore();

    dev_pki::bootstrap(vault.client(), Profile::Dev)
        .await
        .expect("bootstrap failed");

    let cert =
        dev_pki::issue_cert(vault.client(), Profile::Dev, "fraud-alerts.internal.test")
            .await
            .expect("issuing a certificate failed");

    assert!(
        cert.certificate.starts_with("-----BEGIN CERTIFICATE-----"),
        "certificate must be PEM-encoded, got: {}",
        cert.certificate
    );
    assert!(
        !cert.private_key.is_empty(),
        "issued certificate must come with a private key"
    );
    assert!(
        cert.issuing_ca.starts_with("-----BEGIN CERTIFICATE-----"),
        "issuing CA must be PEM-encoded"
    );
}
