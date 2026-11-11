//! Envelope encryption for managed test-wallet keys.
//!
//! Each wallet's 32-byte ed25519 seed is encrypted with AES-256-GCM under a
//! per-wallet data key (DEK); the DEK is wrapped by a key-encryption key (KEK).
//! Locally the KEK comes from an env secret; in GCP it should be a Cloud KMS
//! symmetric key (wrap/unwrap via KMS encrypt/decrypt) — see docs/KEY_MANAGEMENT.md.
//! The wallet id is bound as AAD so ciphertexts cannot be swapped between rows.
//!
//! Honest limitation: to sign, the seed is decrypted into process memory.
//! This is NOT hardware isolation. Zeroization reduces, not eliminates, exposure.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeystoreError {
    #[error("KEK must be 32 bytes")]
    BadKek,
    #[error("decryption failed (wrong key, tampered data or wrong wallet binding)")]
    Decrypt,
    #[error("malformed sealed key")]
    Malformed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedKey {
    pub version: u8,
    pub kek_id: String,
    /// nonce(12) || ciphertext(DEK 32 + tag 16)
    pub wrapped_dek: Vec<u8>,
    /// nonce(12) || ciphertext(seed 32 + tag 16)
    pub sealed_seed: Vec<u8>,
}

pub struct Kek {
    id: String,
    key: Zeroizing<[u8; 32]>,
}

impl Kek {
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, KeystoreError> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| KeystoreError::BadKek)?;
        Ok(Self {
            id: id.into(),
            key: Zeroizing::new(arr),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
}

fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("32-byte key");
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("aes-gcm encrypt");
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, KeystoreError> {
    if sealed.len() < 12 + 16 {
        return Err(KeystoreError::Malformed);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| KeystoreError::BadKek)?;
    let (nonce, ct) = sealed.split_at(12);
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| KeystoreError::Decrypt)
}

pub fn seal_seed(kek: &Kek, wallet_id: &str, seed: &[u8; 32]) -> SealedKey {
    let mut dek = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut dek);
    let aad = format!("wallet:{wallet_id}");
    let sealed_seed = seal(&dek, seed, aad.as_bytes());
    let wrapped_dek = seal(&kek.key, &dek, aad.as_bytes());
    dek.zeroize();
    SealedKey {
        version: 1,
        kek_id: kek.id.clone(),
        wrapped_dek,
        sealed_seed,
    }
}

pub fn open_seed(
    kek: &Kek,
    wallet_id: &str,
    sealed: &SealedKey,
) -> Result<Zeroizing<[u8; 32]>, KeystoreError> {
    if sealed.version != 1 || sealed.kek_id != kek.id {
        return Err(KeystoreError::Malformed);
    }
    let aad = format!("wallet:{wallet_id}");
    let dek = open(&kek.key, &sealed.wrapped_dek, aad.as_bytes())?;
    let dek: [u8; 32] = dek
        .as_slice()
        .try_into()
        .map_err(|_| KeystoreError::Malformed)?;
    let dek = Zeroizing::new(dek);
    let seed = open(&dek, &sealed.sealed_seed, aad.as_bytes())?;
    let arr: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| KeystoreError::Malformed)?;
    Ok(Zeroizing::new(arr))
}

pub fn random_seed() -> Zeroizing<[u8; 32]> {
    let mut s = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(s.as_mut());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_binding() {
        let kek = Kek::new("local-1", &[9u8; 32]).unwrap();
        let seed = [42u8; 32];
        let sealed = seal_seed(&kek, "w1", &seed);
        assert_eq!(*open_seed(&kek, "w1", &sealed).unwrap(), seed);
        // swapped wallet binding fails
        assert_eq!(
            open_seed(&kek, "w2", &sealed).unwrap_err(),
            KeystoreError::Decrypt
        );
        // wrong kek fails
        let other = Kek::new("local-1", &[8u8; 32]).unwrap();
        assert_eq!(
            open_seed(&other, "w1", &sealed).unwrap_err(),
            KeystoreError::Decrypt
        );
        // tamper fails
        let mut t = sealed.clone();
        let n = t.sealed_seed.len();
        t.sealed_seed[n - 1] ^= 1;
        assert_eq!(
            open_seed(&kek, "w1", &t).unwrap_err(),
            KeystoreError::Decrypt
        );
        // ciphertext never contains the seed bytes
        assert!(!sealed.sealed_seed.windows(32).any(|w| w == seed));
        assert!(Kek::new("x", &[0u8; 16]).is_err());
    }
}
