//! Integration coverage for `messgr::keystore` against a real dev-mode
//! Vault (see `just vault-dev-init` / the CI bootstrap step in
//! `.github/workflows/ci.yml` — both must have already run against this
//! process's `VAULT_ADDR` before this suite runs).

use messgr::keystore::{KeyStore, VaultKeyStore};
use messgr::profile::Profile;

fn store() -> VaultKeyStore {
    dotenvy::dotenv().ok();
    VaultKeyStore::connect(Profile::Dev).expect("connecting to dev-mode Vault failed")
}

#[tokio::test]
async fn create_dek_and_unwrap_dek_round_trip() {
    let store = store();

    let dek = store
        .create_dek("transit")
        .await
        .expect("create_dek failed");
    assert_eq!(
        dek.plaintext.len(),
        32,
        "Transit's default data key is 256 bits"
    );
    assert!(
        dek.wrapped.starts_with("vault:v1:"),
        "wrapped ciphertext must carry Vault's own versioned prefix, got {:?}",
        dek.wrapped
    );

    let unwrapped = store
        .unwrap_dek("transit", &dek.wrapped)
        .await
        .expect("unwrap_dek failed");
    assert_eq!(
        *unwrapped, *dek.plaintext,
        "unwrap_dek must return exactly the plaintext create_dek produced"
    );
}

#[tokio::test]
async fn create_dek_returns_a_distinct_key_each_call() {
    let store = store();

    let first = store
        .create_dek("transit")
        .await
        .expect("first create_dek failed");
    let second = store
        .create_dek("transit")
        .await
        .expect("second create_dek failed");

    assert_ne!(
        *first.plaintext, *second.plaintext,
        "two calls to create_dek must not return the same key"
    );
    assert_ne!(
        first.wrapped, second.wrapped,
        "two calls to create_dek must not return the same wrapped ciphertext"
    );
}

#[tokio::test]
async fn unwrap_dek_rejects_a_ciphertext_from_a_different_mount() {
    // Vault Transit binds a ciphertext to the key that wrapped it; asking a
    // *different* key (a second mount, same fixed "messgr-dek" name inside
    // it — see `just vault-dev-init`) to decrypt it must fail, not silently
    // return garbage.
    let store = store();

    let dek = store
        .create_dek("transit")
        .await
        .expect("create_dek failed");

    let result = store.unwrap_dek("transit-other", &dek.wrapped).await;
    assert!(
        result.is_err(),
        "decrypting under the wrong mount's key must fail, not succeed"
    );
}
