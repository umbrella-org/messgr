//! Computes the keyed HMAC used for `destination_hmac`/`value_hmac` lookup
//! columns (DESIGN.md §4.1, §4.6, §7.6). Pure function — the pepper itself
//! comes from `tenant_pepper::ensure_tenant_pepper`; this module never
//! touches Vault or Postgres.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn compute(pepper: &[u8], destination: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(pepper)
        .expect("HMAC accepts a key of any length");
    mac.update(destination.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_for_the_same_pepper_and_destination() {
        assert_eq!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-a", "+15550100")
        );
    }

    #[test]
    fn differs_across_peppers_for_the_same_destination() {
        assert_ne!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-b", "+15550100")
        );
    }

    #[test]
    fn differs_across_destinations_for_the_same_pepper() {
        assert_ne!(
            compute(b"pepper-a", "+15550100"),
            compute(b"pepper-a", "+15550199")
        );
    }
}
