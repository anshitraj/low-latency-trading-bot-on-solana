//! Non-order application data: users, wallets, settings, sessions, Telegram
//! callback tokens, update dedupe, tracked wallets, outbox, audit.
//! `MemAppStore` (mock mode/tests) and `PgStore` (crates/api/src/pg.rs).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use trader_domain::{Bps, Lamports, Network, SenderKind, WalletPolicy};
use trader_wallets::{SealedKey, SignerKind};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AppStoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub telegram_id: i64,
    pub username: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Private-beta allowlist flag (operator-controlled).
    pub beta_allowed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wallet {
    pub id: Uuid,
    pub user_id: Uuid,
    pub network: Network,
    pub pubkey: String,
    pub signer_kind: SignerKind,
    pub label: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserSettings {
    pub buy_presets: Vec<Lamports>,
    pub sell_presets_bps: Vec<u16>,
    pub slippage: Bps,
    pub compute_unit_price_micro_lamports: u64,
    pub max_priority_fee: Lamports,
    pub tip: Lamports,
    pub confirm_trades: bool,
    pub senders: Vec<SenderKind>,
}

impl UserSettings {
    pub fn defaults_for(mode_senders: Vec<SenderKind>) -> Self {
        Self {
            buy_presets: [
                50_000_000u64,
                100_000_000,
                250_000_000,
                500_000_000,
                1_000_000_000,
            ]
            .into_iter()
            .map(Lamports)
            .collect(),
            sell_presets_bps: vec![2_500, 5_000, 7_500, 10_000],
            slippage: Bps::new(500).unwrap(),
            // ~100,000 lamports at a 100k-CU limit: the GMGN/Photon median
            // priority fee on 2026-11-12 (BENCHMARKS 2j), ~US$0.008 per trade.
            compute_unit_price_micro_lamports: 1_000_000,
            max_priority_fee: Lamports(150_000),
            tip: Lamports(0),
            confirm_trades: true,
            senders: mode_senders,
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.buy_presets.is_empty() || self.buy_presets.len() > 6 {
            return Err("1..=6 buy presets required".into());
        }
        if self.buy_presets.iter().any(|l| l.0 == 0) {
            return Err("buy presets must be > 0".into());
        }
        if self.sell_presets_bps.is_empty()
            || self.sell_presets_bps.len() > 6
            || self.sell_presets_bps.iter().any(|b| *b == 0 || *b > 10_000)
        {
            return Err("sell presets must be 1..=10000 bps (1..=6 of them)".into());
        }
        if self.senders.is_empty() {
            return Err("at least one sender".into());
        }
        Ok(())
    }
}

/// What a Telegram inline button does. Stored server-side; callback_data is
/// only an opaque random token, so it cannot be forged or edited.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "a", rename_all = "snake_case")]
pub enum CallbackAction {
    Buy { mint: String, lamports: u64 },
    Sell { mint: String, bps: u16 },
    Approve { order_id: Uuid },
    Cancel { order_id: Uuid },
    Refresh { mint: String },
    CreateWallet,
    MockAirdrop,
}

impl CallbackAction {
    /// Spending actions are single-use; navigation may be reused.
    pub fn single_use(&self) -> bool {
        matches!(
            self,
            CallbackAction::Buy { .. }
                | CallbackAction::Sell { .. }
                | CallbackAction::Approve { .. }
                | CallbackAction::Cancel { .. }
                | CallbackAction::CreateWallet
                | CallbackAction::MockAirdrop
        )
    }
}

#[derive(Clone, Debug)]
pub struct CallbackToken {
    pub token: String,
    pub user_id: Uuid,
    pub telegram_id: i64,
    pub chat_id: i64,
    pub action: CallbackAction,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CallbackCheck {
    Ok(CallbackAction),
    /// Already consumed: return the original action so the caller can map it
    /// to the same idempotency key (replay-safe) without re-executing.
    Replayed(CallbackAction),
    Expired,
    WrongUser,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxMessage {
    pub id: Uuid,
    pub chat_id: i64,
    pub text: String,
    pub buttons: serde_json::Value,
    pub dedupe_key: String,
    pub attempts: u32,
    pub next_attempt_at: DateTime<Utc>,
    /// Edit this message instead of sending a new one, if set.
    pub edit_message_id: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedWallet {
    pub user_id: Uuid,
    pub address: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait AppStore: Send + Sync {
    async fn upsert_telegram_user(
        &self,
        telegram_id: i64,
        username: Option<String>,
        beta_allowed: bool,
    ) -> Result<User, AppStoreError>;
    async fn get_user(&self, id: Uuid) -> Result<User, AppStoreError>;
    async fn user_by_telegram(&self, telegram_id: i64) -> Result<Option<User>, AppStoreError>;

    async fn create_wallet(
        &self,
        w: &Wallet,
        sealed: Option<&SealedKey>,
    ) -> Result<(), AppStoreError>;
    async fn list_wallets(&self, user_id: Uuid) -> Result<Vec<Wallet>, AppStoreError>;
    async fn get_wallet(&self, id: Uuid) -> Result<Wallet, AppStoreError>;
    async fn sealed_key(&self, wallet_id: Uuid) -> Result<Option<SealedKey>, AppStoreError>;

    async fn get_policy(&self, wallet_id: Uuid) -> Result<Option<WalletPolicy>, AppStoreError>;
    async fn set_policy(&self, wallet_id: Uuid, p: &WalletPolicy) -> Result<(), AppStoreError>;

    async fn get_settings(&self, user_id: Uuid) -> Result<Option<UserSettings>, AppStoreError>;
    async fn set_settings(&self, user_id: Uuid, s: &UserSettings) -> Result<(), AppStoreError>;

    async fn create_session(
        &self,
        token_hash: &str,
        user_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<(), AppStoreError>;
    async fn session_user(&self, token_hash: &str) -> Result<Option<Uuid>, AppStoreError>;
    /// Remember a Mini App initData hash so the same signed payload cannot mint
    /// a second session (replay mitigation). Returns false if already seen.
    async fn remember_init_data(
        &self,
        hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, AppStoreError>;

    async fn create_callback(&self, t: &CallbackToken) -> Result<(), AppStoreError>;
    async fn check_callback(
        &self,
        token: &str,
        telegram_id: i64,
        chat_id: i64,
    ) -> Result<CallbackCheck, AppStoreError>;

    /// Returns true the first time an update_id is seen.
    async fn mark_update(&self, update_id: i64) -> Result<bool, AppStoreError>;

    async fn add_tracked(&self, t: &TrackedWallet) -> Result<(), AppStoreError>;
    async fn remove_tracked(&self, user_id: Uuid, address: &str) -> Result<bool, AppStoreError>;
    async fn list_tracked(&self, user_id: Uuid) -> Result<Vec<TrackedWallet>, AppStoreError>;

    async fn enqueue(&self, m: &OutboxMessage) -> Result<bool, AppStoreError>;
    async fn due_messages(&self, limit: usize) -> Result<Vec<OutboxMessage>, AppStoreError>;
    async fn mark_sent(
        &self,
        id: Uuid,
        telegram_message_id: Option<i64>,
    ) -> Result<(), AppStoreError>;
    async fn mark_retry(&self, id: Uuid, at: DateTime<Utc>, err: &str)
        -> Result<(), AppStoreError>;

    async fn set_order_message(
        &self,
        order_id: Uuid,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), AppStoreError>;
    async fn order_message(&self, order_id: Uuid) -> Result<Option<(i64, i64)>, AppStoreError>;

    async fn audit(
        &self,
        user_id: Option<Uuid>,
        kind: &str,
        detail: serde_json::Value,
    ) -> Result<(), AppStoreError>;
}

#[derive(Default)]
struct MemInner {
    users: HashMap<Uuid, User>,
    by_tg: HashMap<i64, Uuid>,
    wallets: HashMap<Uuid, (Wallet, Option<SealedKey>)>,
    policies: HashMap<Uuid, WalletPolicy>,
    settings: HashMap<Uuid, UserSettings>,
    sessions: HashMap<String, (Uuid, DateTime<Utc>)>,
    init_hashes: HashMap<String, DateTime<Utc>>,
    callbacks: HashMap<String, CallbackToken>,
    updates: std::collections::HashSet<i64>,
    tracked: Vec<TrackedWallet>,
    outbox: Vec<(OutboxMessage, bool)>,
    outbox_keys: std::collections::HashSet<String>,
    order_msgs: HashMap<Uuid, (i64, i64)>,
    audit: Vec<(Option<Uuid>, String, serde_json::Value, DateTime<Utc>)>,
}

#[derive(Clone, Default)]
pub struct MemAppStore {
    inner: Arc<Mutex<MemInner>>,
}

impl MemAppStore {
    pub fn audit_len(&self) -> usize {
        self.inner.lock().audit.len()
    }
    pub fn sent_texts(&self) -> Vec<(i64, String)> {
        self.inner
            .lock()
            .outbox
            .iter()
            .filter(|(_, s)| *s)
            .map(|(m, _)| (m.chat_id, m.text.clone()))
            .collect()
    }
    pub fn all_outbox(&self) -> Vec<OutboxMessage> {
        self.inner
            .lock()
            .outbox
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }
}

#[async_trait]
impl AppStore for MemAppStore {
    async fn upsert_telegram_user(
        &self,
        telegram_id: i64,
        username: Option<String>,
        beta_allowed: bool,
    ) -> Result<User, AppStoreError> {
        let mut g = self.inner.lock();
        if let Some(id) = g.by_tg.get(&telegram_id).copied() {
            let u = g.users.get_mut(&id).unwrap();
            u.username = username;
            u.beta_allowed = beta_allowed;
            return Ok(u.clone());
        }
        let u = User {
            id: Uuid::new_v4(),
            telegram_id,
            username,
            created_at: Utc::now(),
            beta_allowed,
        };
        g.by_tg.insert(telegram_id, u.id);
        g.users.insert(u.id, u.clone());
        Ok(u)
    }
    async fn get_user(&self, id: Uuid) -> Result<User, AppStoreError> {
        self.inner
            .lock()
            .users
            .get(&id)
            .cloned()
            .ok_or(AppStoreError::NotFound)
    }
    async fn user_by_telegram(&self, telegram_id: i64) -> Result<Option<User>, AppStoreError> {
        let g = self.inner.lock();
        Ok(g.by_tg
            .get(&telegram_id)
            .and_then(|id| g.users.get(id))
            .cloned())
    }
    async fn create_wallet(
        &self,
        w: &Wallet,
        sealed: Option<&SealedKey>,
    ) -> Result<(), AppStoreError> {
        let mut g = self.inner.lock();
        if g.wallets
            .values()
            .any(|(x, _)| x.pubkey == w.pubkey && x.network == w.network)
        {
            return Err(AppStoreError::Conflict("wallet exists".into()));
        }
        g.wallets.insert(w.id, (w.clone(), sealed.cloned()));
        Ok(())
    }
    async fn list_wallets(&self, user_id: Uuid) -> Result<Vec<Wallet>, AppStoreError> {
        let mut v: Vec<Wallet> = self
            .inner
            .lock()
            .wallets
            .values()
            .filter(|(w, _)| w.user_id == user_id)
            .map(|(w, _)| w.clone())
            .collect();
        v.sort_by_key(|w| w.created_at);
        Ok(v)
    }
    async fn get_wallet(&self, id: Uuid) -> Result<Wallet, AppStoreError> {
        self.inner
            .lock()
            .wallets
            .get(&id)
            .map(|(w, _)| w.clone())
            .ok_or(AppStoreError::NotFound)
    }
    async fn sealed_key(&self, wallet_id: Uuid) -> Result<Option<SealedKey>, AppStoreError> {
        Ok(self
            .inner
            .lock()
            .wallets
            .get(&wallet_id)
            .and_then(|(_, s)| s.clone()))
    }
    async fn get_policy(&self, wallet_id: Uuid) -> Result<Option<WalletPolicy>, AppStoreError> {
        Ok(self.inner.lock().policies.get(&wallet_id).cloned())
    }
    async fn set_policy(&self, wallet_id: Uuid, p: &WalletPolicy) -> Result<(), AppStoreError> {
        self.inner.lock().policies.insert(wallet_id, p.clone());
        Ok(())
    }
    async fn get_settings(&self, user_id: Uuid) -> Result<Option<UserSettings>, AppStoreError> {
        Ok(self.inner.lock().settings.get(&user_id).cloned())
    }
    async fn set_settings(&self, user_id: Uuid, s: &UserSettings) -> Result<(), AppStoreError> {
        self.inner.lock().settings.insert(user_id, s.clone());
        Ok(())
    }
    async fn create_session(
        &self,
        token_hash: &str,
        user_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<(), AppStoreError> {
        self.inner
            .lock()
            .sessions
            .insert(token_hash.to_string(), (user_id, expires_at));
        Ok(())
    }
    async fn session_user(&self, token_hash: &str) -> Result<Option<Uuid>, AppStoreError> {
        Ok(self
            .inner
            .lock()
            .sessions
            .get(token_hash)
            .filter(|(_, e)| *e > Utc::now())
            .map(|(u, _)| *u))
    }
    async fn remember_init_data(
        &self,
        hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, AppStoreError> {
        let mut g = self.inner.lock();
        let now = Utc::now();
        g.init_hashes.retain(|_, e| *e > now);
        if g.init_hashes.contains_key(hash) {
            return Ok(false);
        }
        g.init_hashes.insert(hash.to_string(), expires_at);
        Ok(true)
    }
    async fn create_callback(&self, t: &CallbackToken) -> Result<(), AppStoreError> {
        self.inner
            .lock()
            .callbacks
            .insert(t.token.clone(), t.clone());
        Ok(())
    }
    async fn check_callback(
        &self,
        token: &str,
        telegram_id: i64,
        chat_id: i64,
    ) -> Result<CallbackCheck, AppStoreError> {
        let mut g = self.inner.lock();
        let Some(t) = g.callbacks.get_mut(token) else {
            return Ok(CallbackCheck::Unknown);
        };
        if t.telegram_id != telegram_id || t.chat_id != chat_id {
            return Ok(CallbackCheck::WrongUser);
        }
        if t.expires_at <= Utc::now() {
            return Ok(CallbackCheck::Expired);
        }
        if t.action.single_use() {
            if t.used_at.is_some() {
                return Ok(CallbackCheck::Replayed(t.action.clone()));
            }
            t.used_at = Some(Utc::now());
        }
        Ok(CallbackCheck::Ok(t.action.clone()))
    }
    async fn mark_update(&self, update_id: i64) -> Result<bool, AppStoreError> {
        Ok(self.inner.lock().updates.insert(update_id))
    }
    async fn add_tracked(&self, t: &TrackedWallet) -> Result<(), AppStoreError> {
        let mut g = self.inner.lock();
        if g.tracked
            .iter()
            .any(|x| x.user_id == t.user_id && x.address == t.address)
        {
            return Ok(());
        }
        if g.tracked.iter().filter(|x| x.user_id == t.user_id).count() >= 20 {
            return Err(AppStoreError::Conflict(
                "tracking limit (20) reached".into(),
            ));
        }
        g.tracked.push(t.clone());
        Ok(())
    }
    async fn remove_tracked(&self, user_id: Uuid, address: &str) -> Result<bool, AppStoreError> {
        let mut g = self.inner.lock();
        let before = g.tracked.len();
        g.tracked
            .retain(|x| !(x.user_id == user_id && x.address == address));
        Ok(g.tracked.len() != before)
    }
    async fn list_tracked(&self, user_id: Uuid) -> Result<Vec<TrackedWallet>, AppStoreError> {
        Ok(self
            .inner
            .lock()
            .tracked
            .iter()
            .filter(|x| x.user_id == user_id)
            .cloned()
            .collect())
    }
    async fn enqueue(&self, m: &OutboxMessage) -> Result<bool, AppStoreError> {
        let mut g = self.inner.lock();
        if !g.outbox_keys.insert(m.dedupe_key.clone()) {
            return Ok(false);
        }
        g.outbox.push((m.clone(), false));
        Ok(true)
    }
    async fn due_messages(&self, limit: usize) -> Result<Vec<OutboxMessage>, AppStoreError> {
        let now = Utc::now();
        Ok(self
            .inner
            .lock()
            .outbox
            .iter()
            .filter(|(m, s)| !s && m.next_attempt_at <= now)
            .take(limit)
            .map(|(m, _)| m.clone())
            .collect())
    }
    async fn mark_sent(&self, id: Uuid, _mid: Option<i64>) -> Result<(), AppStoreError> {
        if let Some(e) = self
            .inner
            .lock()
            .outbox
            .iter_mut()
            .find(|(m, _)| m.id == id)
        {
            e.1 = true;
        }
        Ok(())
    }
    async fn mark_retry(
        &self,
        id: Uuid,
        at: DateTime<Utc>,
        _err: &str,
    ) -> Result<(), AppStoreError> {
        if let Some(e) = self
            .inner
            .lock()
            .outbox
            .iter_mut()
            .find(|(m, _)| m.id == id)
        {
            e.0.attempts += 1;
            e.0.next_attempt_at = at;
        }
        Ok(())
    }
    async fn set_order_message(
        &self,
        order_id: Uuid,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), AppStoreError> {
        self.inner
            .lock()
            .order_msgs
            .insert(order_id, (chat_id, message_id));
        Ok(())
    }
    async fn order_message(&self, order_id: Uuid) -> Result<Option<(i64, i64)>, AppStoreError> {
        Ok(self.inner.lock().order_msgs.get(&order_id).copied())
    }
    async fn audit(
        &self,
        user_id: Option<Uuid>,
        kind: &str,
        detail: serde_json::Value,
    ) -> Result<(), AppStoreError> {
        self.inner
            .lock()
            .audit
            .push((user_id, kind.to_string(), detail, Utc::now()));
        Ok(())
    }
}
