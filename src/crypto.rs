//! Encryption of stored access tokens with XChaCha20-Poly1305.
//!
//! A stored value is the 24-byte random nonce followed by the ciphertext and
//! its tag. The random nonce is large enough that two encryptions never reuse
//! one in practice.

use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use thiserror::Error;

use crate::config::SECRET_KEY_LEN;

const NONCE_LEN: usize = 24;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("token encryption failed")]
    Encrypt,
    #[error("stored token cannot be decrypted; KEHRIBAR_SECRET_KEY may have changed")]
    Decrypt,
}

#[derive(Clone)]
pub struct Cipher {
    aead: XChaCha20Poly1305,
}

impl Cipher {
    pub fn new(key: &[u8; SECRET_KEY_LEN]) -> Self {
        Self {
            aead: XChaCha20Poly1305::new(&(*key).into()),
        }
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>, CryptoError> {
        let nonce = XNonce::generate();
        let ciphertext = self
            .aead
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| CryptoError::Encrypt)?;
        let mut stored = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        stored.extend_from_slice(nonce.as_slice());
        stored.extend_from_slice(&ciphertext);
        Ok(stored)
    }

    pub fn decrypt(&self, stored: &[u8]) -> Result<String, CryptoError> {
        if stored.len() < NONCE_LEN {
            return Err(CryptoError::Decrypt);
        }
        let (nonce, ciphertext) = stored.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).map_err(|_| CryptoError::Decrypt)?;
        let plaintext = self
            .aead
            .decrypt(&nonce, ciphertext)
            .map_err(|_| CryptoError::Decrypt)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(byte: u8) -> Cipher {
        Cipher::new(&[byte; SECRET_KEY_LEN])
    }

    #[test]
    fn round_trip_returns_the_original_token() {
        let c = cipher(7);
        let stored = c.encrypt("ghp_example-token").unwrap();
        assert_eq!(c.decrypt(&stored).unwrap(), "ghp_example-token");
    }

    #[test]
    fn stored_value_does_not_contain_the_plaintext() {
        let stored = cipher(7).encrypt("plain-secret").unwrap();
        assert!(!stored.windows(12).any(|w| w == b"plain-secret"));
    }

    #[test]
    fn same_token_encrypts_to_different_values() {
        let c = cipher(7);
        assert_ne!(c.encrypt("x").unwrap(), c.encrypt("x").unwrap());
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let stored = cipher(7).encrypt("token").unwrap();
        assert_eq!(cipher(8).decrypt(&stored), Err(CryptoError::Decrypt));
    }

    #[test]
    fn tampered_or_truncated_value_fails_to_decrypt() {
        let c = cipher(7);
        let mut stored = c.encrypt("token").unwrap();
        let last = stored.len() - 1;
        stored[last] ^= 1;
        assert_eq!(c.decrypt(&stored), Err(CryptoError::Decrypt));
        assert_eq!(c.decrypt(&stored[..10]), Err(CryptoError::Decrypt));
    }
}
