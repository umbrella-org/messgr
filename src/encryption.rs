//! AES-256-GCM encryption under a per-customer DEK (DESIGN.md §7.1, T-011
//! decision 6). Wire format: a fresh random 12-byte nonce prepended to the
//! ciphertext+tag, i.e. `blob = nonce(12) || ciphertext_with_tag`. `aad`
//! binds a ciphertext to the row it belongs to (`comms_request_id`'s raw
//! bytes) so one row's blob cannot be silently swapped onto another's.

use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};

const NONCE_LEN: usize = 12;

#[derive(Debug)]
pub enum EncryptionError {
    /// The DEK is not 32 bytes (AES-256's key length).
    InvalidKeyLength,
    /// The blob is shorter than the nonce, so it cannot be well-formed.
    BlobTooShort,
    /// AEAD encryption or decryption failed (wrong key, wrong `aad`,
    /// tampered ciphertext — `aes_gcm` deliberately does not distinguish
    /// these, to avoid leaking which one it was).
    Aead,
}

impl std::fmt::Display for EncryptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKeyLength => write!(f, "DEK is not 32 bytes"),
            Self::BlobTooShort => write!(f, "ciphertext blob shorter than the nonce"),
            Self::Aead => write!(f, "AEAD operation failed"),
        }
    }
}

impl std::error::Error for EncryptionError {}

/// Encrypts `plaintext` under `dek` (32 bytes), binding it to `aad`.
/// Returns `nonce(12) || ciphertext_with_tag`.
pub fn encrypt(
    dek: &[u8],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    if dek.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength);
    }
    let key: &Key<Aes256Gcm> = dek.try_into().expect("checked above: dek is 32 bytes");
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::generate();

    let ciphertext = cipher
        .encrypt(
            &nonce,
            aes_gcm::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| EncryptionError::Aead)?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Decrypts a blob produced by `encrypt`, verifying it was bound to the same
/// `aad`. Fails if `blob` is malformed, the key is wrong, `aad` doesn't
/// match, or the ciphertext was tampered with.
pub fn decrypt(
    dek: &[u8],
    aad: &[u8],
    blob: &[u8],
) -> Result<Vec<u8>, EncryptionError> {
    if dek.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength);
    }
    let key: &Key<Aes256Gcm> = dek.try_into().expect("checked above: dek is 32 bytes");
    if blob.len() < NONCE_LEN {
        return Err(EncryptionError::BlobTooShort);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce: &Nonce<_> = nonce_bytes
        .try_into()
        .expect("checked above: NONCE_LEN bytes");
    let cipher = Aes256Gcm::new(key);

    cipher
        .decrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| EncryptionError::Aead)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dek() -> Vec<u8> {
        vec![0x42; 32]
    }

    #[test]
    fn round_trips() {
        let blob = encrypt(&dek(), b"row-1", b"hello, world").unwrap();
        let plaintext = decrypt(&dek(), b"row-1", &blob).unwrap();
        assert_eq!(plaintext, b"hello, world");
    }

    #[test]
    fn wrong_key_fails() {
        let blob = encrypt(&dek(), b"row-1", b"hello, world").unwrap();
        let other_key = vec![0x99; 32];
        assert!(matches!(
            decrypt(&other_key, b"row-1", &blob),
            Err(EncryptionError::Aead)
        ));
    }

    #[test]
    fn mismatched_aad_fails() {
        let blob = encrypt(&dek(), b"row-1", b"hello, world").unwrap();
        assert!(matches!(
            decrypt(&dek(), b"row-2", &blob),
            Err(EncryptionError::Aead)
        ));
    }

    #[test]
    fn truncated_blob_is_a_clean_error() {
        let short = vec![0u8; NONCE_LEN - 1];
        assert!(matches!(
            decrypt(&dek(), b"row-1", &short),
            Err(EncryptionError::BlobTooShort)
        ));
    }

    #[test]
    fn invalid_key_length_is_a_clean_error() {
        let bad_key = vec![0u8; 16];
        assert!(matches!(
            encrypt(&bad_key, b"row-1", b"hi"),
            Err(EncryptionError::InvalidKeyLength)
        ));
    }
}
