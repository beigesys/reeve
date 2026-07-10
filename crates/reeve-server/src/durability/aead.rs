//! AEAD envelope for everything shipped to the durability target
//! (spec/reeve/07-durability.md §9.2/§9.3/§9.6: nothing reaches the
//! target in plaintext). Pure-Rust XChaCha20-Poly1305 under the D15
//! external keyfile; the 24-byte random nonce makes random nonces safe
//! at any volume. Wire shape: `nonce(24) || ciphertext+tag`.

use anyhow::{Context as _, bail};
use chacha20poly1305::aead::{Aead as _, KeyInit as _};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::keyfile::KEY_LEN;

const NONCE_LEN: usize = 24;

/// Encrypt `plaintext` under `key`. Output: nonce || ciphertext.
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|e| anyhow::anyhow!("aead seal: {e}"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a `seal` envelope. Any tamper (bit-flip, truncation, wrong
/// key) fails authentication — corrupted target objects are DETECTED,
/// never silently restored (§9.4 verify-restore relies on this).
pub fn open(key: &[u8; KEY_LEN], blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    if blob.len() < NONCE_LEN {
        bail!("aead envelope too short ({} bytes)", blob.len());
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .ok()
        .context("aead open failed — object corrupt or wrong keyfile")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let key = [7u8; KEY_LEN];
        let sealed = seal(&key, b"snapshot bytes").unwrap();
        assert_eq!(open(&key, &sealed).unwrap(), b"snapshot bytes");

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&key, &tampered).is_err(), "bit-flip must fail auth");

        let other = [8u8; KEY_LEN];
        assert!(open(&other, &sealed).is_err(), "wrong key must fail");
        assert!(open(&key, b"xx").is_err(), "truncation must fail");
    }

    #[test]
    fn nonces_are_fresh_per_seal() {
        let key = [1u8; KEY_LEN];
        let a = seal(&key, b"x").unwrap();
        let b = seal(&key, b"x").unwrap();
        assert_ne!(a, b, "identical plaintext must not produce identical envelopes");
    }
}
