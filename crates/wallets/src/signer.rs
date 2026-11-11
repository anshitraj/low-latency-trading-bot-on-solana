//! Signer abstraction. The engine only ever hands a signer the serialized
//! *message* bytes after the guard has validated them.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use trader_domain::Network;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerKind {
    /// Deterministic test key; can never sign for mainnet.
    Mock,
    /// Per-user managed key, envelope-encrypted at rest, decrypted into process
    /// memory to sign. Private-beta only. NOT hardware isolation.
    ManagedLocal,
    /// GCP Cloud KMS EC_SIGN_ED25519 (key never leaves KMS). Gated / not implemented.
    Kms,
    /// User signs in their own wallet; the server never holds the key.
    External,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SignError {
    #[error("signer {kind:?} is not allowed to sign for network {network}")]
    WrongNetwork { kind: SignerKind, network: Network },
    #[error("external wallet signature required")]
    ExternalRequired,
    #[error("signer unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait TxSigner: Send + Sync {
    fn pubkey(&self) -> Pubkey;
    fn kind(&self) -> SignerKind;
    fn can_sign_for(&self, network: Network) -> bool;
    async fn sign_message(&self, network: Network, message: &[u8]) -> Result<Signature, SignError>;
}

fn check_net(kind: SignerKind, allowed: &[Network], network: Network) -> Result<(), SignError> {
    if allowed.contains(&network) {
        Ok(())
    } else {
        Err(SignError::WrongNetwork { kind, network })
    }
}

/// Deterministic keypair from a 32-byte seed. Mock/localnet only.
pub struct MockSigner {
    kp: Keypair,
}

impl MockSigner {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            kp: keypair_from_seed(&seed),
        }
    }
    pub fn keypair(&self) -> &Keypair {
        &self.kp
    }
}

pub fn keypair_from_seed(seed: &[u8; 32]) -> Keypair {
    Keypair::new_from_array(*seed)
}

const MOCK_NETWORKS: &[Network] = &[Network::Mock, Network::Localnet];

#[async_trait]
impl TxSigner for MockSigner {
    fn pubkey(&self) -> Pubkey {
        self.kp.pubkey()
    }
    fn kind(&self) -> SignerKind {
        SignerKind::Mock
    }
    fn can_sign_for(&self, n: Network) -> bool {
        MOCK_NETWORKS.contains(&n)
    }
    async fn sign_message(&self, network: Network, message: &[u8]) -> Result<Signature, SignError> {
        check_net(SignerKind::Mock, MOCK_NETWORKS, network)?;
        Ok(self.kp.sign_message(message))
    }
}

/// A managed per-user key held in process memory after envelope decryption.
/// Allowed networks are fixed at construction from wallet metadata; mainnet
/// requires the wallet record to be explicitly mainnet-scoped.
pub struct ManagedLocalSigner {
    kp: Keypair,
    networks: Vec<Network>,
}

impl ManagedLocalSigner {
    pub fn new(kp: Keypair, network: Network) -> Self {
        Self {
            kp,
            networks: vec![network],
        }
    }
}

#[async_trait]
impl TxSigner for ManagedLocalSigner {
    fn pubkey(&self) -> Pubkey {
        self.kp.pubkey()
    }
    fn kind(&self) -> SignerKind {
        SignerKind::ManagedLocal
    }
    fn can_sign_for(&self, n: Network) -> bool {
        self.networks.contains(&n)
    }
    async fn sign_message(&self, network: Network, message: &[u8]) -> Result<Signature, SignError> {
        check_net(SignerKind::ManagedLocal, &self.networks, network)?;
        Ok(self.kp.sign_message(message))
    }
}

/// Placeholder that makes the external-wallet path explicit: the engine must
/// return the unsigned message to the client instead of signing.
pub struct ExternalWalletSigner {
    pub pubkey: Pubkey,
}

#[async_trait]
impl TxSigner for ExternalWalletSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }
    fn kind(&self) -> SignerKind {
        SignerKind::External
    }
    fn can_sign_for(&self, _: Network) -> bool {
        false
    }
    async fn sign_message(&self, _: Network, _: &[u8]) -> Result<Signature, SignError> {
        Err(SignError::ExternalRequired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_refuses_mainnet_and_devnet() {
        let s = MockSigner::from_seed([1; 32]);
        assert!(s.sign_message(Network::Localnet, b"m").await.is_ok());
        assert_eq!(
            s.sign_message(Network::Mainnet, b"m").await,
            Err(SignError::WrongNetwork {
                kind: SignerKind::Mock,
                network: Network::Mainnet
            })
        );
        assert!(s.sign_message(Network::Devnet, b"m").await.is_err());
    }

    #[tokio::test]
    async fn managed_is_network_scoped() {
        let s = ManagedLocalSigner::new(Keypair::new(), Network::Devnet);
        assert!(s.sign_message(Network::Devnet, b"m").await.is_ok());
        assert!(s.sign_message(Network::Mainnet, b"m").await.is_err());
    }

    #[test]
    fn deterministic_seed() {
        assert_eq!(
            MockSigner::from_seed([7; 32]).pubkey(),
            MockSigner::from_seed([7; 32]).pubkey()
        );
        assert_ne!(
            MockSigner::from_seed([7; 32]).pubkey(),
            MockSigner::from_seed([8; 32]).pubkey()
        );
    }

    #[tokio::test]
    async fn external_never_signs() {
        let s = ExternalWalletSigner {
            pubkey: Pubkey::new_unique(),
        };
        assert_eq!(
            s.sign_message(Network::Localnet, b"m").await,
            Err(SignError::ExternalRequired)
        );
    }
}
