//! Engine dependency implementations backed by the AppStore.

use crate::appstore::{AppStore, Wallet};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use trader_domain::{Network, SenderKind, WalletPolicy};
use trader_orders::{PolicySource, SignerProvider};
use trader_wallets::{keypair_from_seed, open_seed, seal_seed, Kek, ManagedLocalSigner, SealedKey, SignerKind, TxSigner};
use uuid::Uuid;

/// Decrypts a wallet's sealed seed once and caches the signer in memory.
/// Honest limitation: the key then lives in process memory (see docs/SECURITY.md).
pub struct AppSigners {
    pub app: Arc<dyn AppStore>,
    pub kek: Option<Arc<Kek>>,
    pub network: Network,
    cache: Mutex<HashMap<Uuid, Arc<dyn TxSigner>>>,
}

impl AppSigners {
    pub fn new(app: Arc<dyn AppStore>, kek: Option<Arc<Kek>>, network: Network) -> Self {
        Self { app, kek, network, cache: Mutex::new(HashMap::new()) }
    }
}

#[async_trait]
impl SignerProvider for AppSigners {
    async fn signer_for(&self, wallet_id: Uuid) -> Result<Arc<dyn TxSigner>, String> {
        if let Some(s) = self.cache.lock().get(&wallet_id) {
            return Ok(s.clone());
        }
        let w = self.app.get_wallet(wallet_id).await.map_err(|e| e.to_string())?;
        if w.network != self.network {
            return Err(format!("wallet network {} != engine network {}", w.network, self.network));
        }
        if w.signer_kind != SignerKind::ManagedLocal {
            return Err("wallet is not a managed signer (external wallets sign client-side)".into());
        }
        let kek = self.kek.as_ref().ok_or("no KEK configured")?;
        let sealed = self.app.sealed_key(wallet_id).await.map_err(|e| e.to_string())?.ok_or("no sealed key")?;
        let seed = open_seed(kek, &wallet_id.to_string(), &sealed).map_err(|e| e.to_string())?;
        let kp = keypair_from_seed(&seed);
        let s: Arc<dyn TxSigner> = Arc::new(ManagedLocalSigner::new(kp, self.network));
        if s.pubkey().to_string() != w.pubkey {
            return Err("decrypted key does not match wallet pubkey".into());
        }
        self.cache.lock().insert(wallet_id, s.clone());
        Ok(s)
    }
}

pub struct AppPolicies {
    pub app: Arc<dyn AppStore>,
    pub defaults: WalletPolicy,
}

#[async_trait]
impl PolicySource for AppPolicies {
    async fn policy(&self, wallet_id: Uuid) -> Result<WalletPolicy, String> {
        Ok(self.app.get_policy(wallet_id).await.map_err(|e| e.to_string())?.unwrap_or_else(|| self.defaults.clone()))
    }
}

pub fn default_policy(network: Network, senders: &[SenderKind]) -> WalletPolicy {
    WalletPolicy {
        allowed_senders: senders.to_vec(),
        // Mainnet stays disabled per wallet until an operator flips it after review.
        mainnet_enabled: false,
        ..if network == Network::Mainnet {
            WalletPolicy {
                max_order: trader_domain::Lamports(50_000_000), // 0.05 SOL
                max_daily_turnover: trader_domain::Lamports(200_000_000),
                ..Default::default()
            }
        } else {
            WalletPolicy::default()
        }
    }
}

/// Create a managed (custodial, private-beta) test wallet.
pub async fn create_managed_wallet(app: &Arc<dyn AppStore>, kek: &Kek, user_id: Uuid, network: Network) -> Result<Wallet, String> {
    let id = Uuid::new_v4();
    let seed = trader_wallets::random_seed();
    let kp = keypair_from_seed(&seed);
    let sealed: SealedKey = seal_seed(kek, &id.to_string(), &seed);
    use solana_signer::Signer;
    let w = Wallet {
        id,
        user_id,
        network,
        pubkey: kp.pubkey().to_string(),
        signer_kind: SignerKind::ManagedLocal,
        label: format!("{} test wallet", network.as_str()),
        created_at: chrono::Utc::now(),
    };
    app.create_wallet(&w, Some(&sealed)).await.map_err(|e| e.to_string())?;
    app.audit(Some(user_id), "wallet_created", serde_json::json!({"wallet_id": id, "network": network.as_str(), "pubkey": w.pubkey})).await.ok();
    Ok(w)
}
