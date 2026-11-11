//! Wallet custody and signing. See docs/SECURITY.md for the custody model.

pub mod keystore;
pub mod signer;

pub use keystore::{open_seed, random_seed, seal_seed, Kek, KeystoreError, SealedKey};
pub use signer::{keypair_from_seed, ExternalWalletSigner, ManagedLocalSigner, MockSigner, SignError, SignerKind, TxSigner};
