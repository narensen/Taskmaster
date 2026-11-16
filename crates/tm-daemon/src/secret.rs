//! Random tokens and their hashes.

use std::io::Read;

use anyhow::Context;
use sha2::{Digest, Sha256};

pub fn random_bytes(n: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    let mut file = std::fs::File::open("/dev/urandom").context("cannot read /dev/urandom")?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// `n` random bytes as `2n` hex characters.
pub fn random_hex(n: usize) -> anyhow::Result<String> {
    Ok(random_bytes(n)?.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What is stored in place of a token, so the record cannot be used to sign in.
pub fn fingerprint(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_differ_and_a_fingerprint_is_stable() {
        let (a, b) = (random_hex(24).unwrap(), random_hex(24).unwrap());
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
        assert_eq!(fingerprint(&a), fingerprint(&a));
        assert_ne!(fingerprint(&a), a);
    }
}
