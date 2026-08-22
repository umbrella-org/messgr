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
    //
    // Review finding T-003/F1: an earlier version of this test asserted only
    // `result.is_err()`, which is satisfied by *any* Vault-side error —
    // including "transit-other's fixture is missing or broken", which would
    // make this test pass for the wrong reason (the same T-001/F13 failure
    // shape). Confirmed by mutation: deleting the `transit-other` mount
    // outright left this test green. Fixed two ways: (1) prove
    // `transit-other` genuinely has its own working key by round-tripping a
    // DEK through it before the cross-mount attempt, so a broken fixture
    // fails loudly and separately from the property under test; (2) assert
    // on the actual error content (`KeyStoreError`'s `Debug`, which surfaces
    // `vaultrs::error::ClientError::APIError`'s `errors` field) rather than
    // merely its existence, requiring it name a cipher/authentication
    // failure specifically.
    let store = store();

    let dek = store
        .create_dek("transit")
        .await
        .expect("create_dek failed");

    let other_dek = store.create_dek("transit-other").await.expect(
        "transit-other's own create_dek failed — the cross-mount fixture is broken, \
         not the property this test exists to check",
    );
    store
        .unwrap_dek("transit-other", &other_dek.wrapped)
        .await
        .expect("transit-other must be able to decrypt its own ciphertext");

    let error = store
        .unwrap_dek("transit-other", &dek.wrapped)
        .await
        .expect_err("decrypting under the wrong mount's key must fail, not succeed");
    let message = format!("{error:?}");
    assert!(
        message.contains("cipher") || message.contains("authentication"),
        "must fail specifically because Transit rejected the ciphertext under the wrong key, \
         not for an unrelated reason (e.g. a broken/missing transit-other fixture): {message:?}"
    );
}
