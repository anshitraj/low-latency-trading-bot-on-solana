//! The order pipeline (master prompt §4):
//! intent -> idempotency -> policy -> quote -> (auth) -> sign once -> route
//! -> observe -> reconcile -> notify.

use crate::model::*;
use crate::observer::{ObserverHandle, Track};
use crate::store::{OrderStore, StoreError};
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use trader_domain::fees::{base_fee, rent_exempt_minimum, TOKEN_2022_ATA_LEN, TOKEN_ACCOUNT_LEN};
use trader_domain::money::portion_bps;
use trader_domain::{
    AmountSpec, ComputeBudget, CostEstimate, Lamports, LatencyTrace, Network, OrderState, PolicyViolation, Quote,
    QuoteSource, Side, Stage, TradeIntent, WalletPolicy,
};
use trader_execution::{
    build_and_sign, BlockhashService, BuildParams, GuardConfig, ProtocolAdapter, ProtocolError, ProtocolQuote,
    QuoteRequest, Reservation, TipInstruction, TxGuard, WalletLocks,
};
use trader_market_data::token::TOKEN_2022_PROGRAM;
use trader_router::{verdict, FanOutVerdict, Router, SendContext};
use trader_wallets::TxSigner;
use uuid::Uuid;
use solana_hash as solana_hash_compat;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub network: Network,
    /// Global operator switch; mainnet orders are refused unless true.
    pub mainnet_enabled: bool,
    pub max_state_age: Duration,
    pub max_blockhash_age: Duration,
    pub quote_ttl: Duration,
    pub workers: usize,
    pub queue_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            network: Network::Localnet,
            mainnet_enabled: false,
            max_state_age: Duration::from_millis(2_000),
            max_blockhash_age: Duration::from_secs(30),
            quote_ttl: Duration::from_secs(30),
            workers: 4,
            queue_capacity: 1024,
        }
    }
}

#[async_trait]
pub trait SignerProvider: Send + Sync {
    async fn signer_for(&self, wallet_id: Uuid) -> Result<Arc<dyn TxSigner>, String>;
}

#[async_trait]
pub trait BalanceSource: Send + Sync {
    async fn balances(&self, wallet: &Pubkey, mint: &Pubkey, extra: &[Pubkey]) -> Result<WalletBalances, String>;
}

#[async_trait]
pub trait PolicySource: Send + Sync {
    async fn policy(&self, wallet_id: Uuid) -> Result<WalletPolicy, String>;
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("policy: {0}")]
    Policy(#[from] PolicyViolation),
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("not supported: no adapter can trade this mint")]
    NoMarket,
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("forbidden")]
    Forbidden,
    #[error("engine busy, try again")]
    Busy,
    #[error("dependency unavailable: {0}")]
    Unavailable(String),
}

/// Notification emitted on every state change (consumed by the outbox).
#[derive(Clone, Debug, serde::Serialize)]
pub struct OrderUpdate {
    pub order_id: Uuid,
    pub user_id: Uuid,
    pub state: OrderState,
    pub signature: Option<String>,
    pub detail: String,
    pub fill: Option<FillRecord>,
}

#[derive(Clone)]
pub struct Prepared {
    pub adapter: usize,
    pub pq: ProtocolQuote,
    pub balances: WalletBalances,
    pub at: Instant,
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub store: Arc<dyn OrderStore>,
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    market_index: RwLock<HashMap<Pubkey, usize>>,
    pub router: Router,
    pub blockhash: BlockhashService,
    pub locks: WalletLocks,
    guard: TxGuard,
    signers: Arc<dyn SignerProvider>,
    balances: Arc<dyn BalanceSource>,
    policies: Arc<dyn PolicySource>,
    observer: ObserverHandle,
    pub updates: broadcast::Sender<OrderUpdate>,
    prepared: Mutex<HashMap<Uuid, Prepared>>,
    traces: Mutex<HashMap<Uuid, LatencyTrace>>,
    queue: Mutex<Option<mpsc::Sender<Uuid>>>,
    /// (wallet, blockhash) -> (next CU offset, first use).
    recent_msgs: Mutex<HashMap<(Pubkey, solana_hash_compat::Hash), (u32, Instant)>>,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: EngineConfig,
        store: Arc<dyn OrderStore>,
        adapters: Vec<Arc<dyn ProtocolAdapter>>,
        router: Router,
        blockhash: BlockhashService,
        locks: WalletLocks,
        signers: Arc<dyn SignerProvider>,
        balances: Arc<dyn BalanceSource>,
        policies: Arc<dyn PolicySource>,
        observer: ObserverHandle,
        updates: broadcast::Sender<OrderUpdate>,
    ) -> Arc<Self> {
        let mut guard = TxGuard::default();
        for a in &adapters {
            for (id, name, rule) in a.program_rules() {
                match rule {
                    Some(r) => guard.allow_program_with_rule(id, name, r),
                    None => guard.allow_program(id, name),
                }
            }
        }
        Arc::new(Self {
            cfg,
            store,
            adapters,
            market_index: RwLock::new(HashMap::new()),
            router,
            blockhash,
            locks,
            guard,
            signers,
            balances,
            policies,
            observer,
            updates,
            prepared: Mutex::new(HashMap::new()),
            traces: Mutex::new(HashMap::new()),
            queue: Mutex::new(None),
            recent_msgs: Mutex::new(HashMap::new()),
        })
    }

    pub fn guard(&self) -> &TxGuard {
        &self.guard
    }

    /// Start N workers consuming the bounded order queue.
    pub fn spawn_workers(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel::<Uuid>(self.cfg.queue_capacity);
        *self.queue.lock() = Some(tx);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..self.cfg.workers.max(1) {
            let (me, rx) = (self.clone(), rx.clone());
            tokio::spawn(async move {
                loop {
                    let next = { rx.lock().await.recv().await };
                    match next {
                        Some(id) => {
                            if let Err(e) = me.process(id).await {
                                tracing::warn!(order_id = %id, error = %e, "order processing error");
                            }
                        }
                        None => break,
                    }
                }
            });
        }
    }

    pub fn take_trace(&self, id: Uuid) -> Option<LatencyTrace> {
        self.traces.lock().remove(&id)
    }

    fn mark(&self, id: Uuid, s: Stage) {
        if let Some(t) = self.traces.lock().get_mut(&id) {
            t.mark(s);
        }
    }

    fn emit(&self, o: &Order, state: OrderState, detail: impl Into<String>) {
        let _ = self.updates.send(OrderUpdate {
            order_id: o.id,
            user_id: o.user_id,
            state,
            signature: o.signed.as_ref().map(|s| s.signature.clone()),
            detail: detail.into(),
            fill: None,
        });
    }

    async fn adapter_for(&self, mint: &Pubkey) -> Result<usize, EngineError> {
        if let Some(i) = self.market_index.read().get(mint) {
            return Ok(*i);
        }
        for (i, a) in self.adapters.iter().enumerate() {
            match a.resolve_market(mint).await {
                Ok(Some(_)) => {
                    self.market_index.write().insert(*mint, i);
                    return Ok(i);
                }
                Ok(None) => continue,
                Err(ProtocolError::Unsupported { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(EngineError::NoMarket)
    }

    async fn quote_with_refresh(&self, idx: usize, req: &QuoteRequest) -> Result<ProtocolQuote, EngineError> {
        let a = &self.adapters[idx];
        match a.quote_exact_in(req) {
            Ok(q) => Ok(q),
            Err(ProtocolError::Stale(_) | ProtocolError::StateUnavailable(_)) => {
                a.refresh(&req.mint).await?;
                Ok(a.quote_exact_in(req)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Compute an executable quote for an intent with full cost breakdown.
    pub async fn prepare(&self, intent: &TradeIntent, wallet: &Pubkey, policy: &WalletPolicy) -> Result<(Quote, Prepared), EngineError> {
        let idx = self.adapter_for(&intent.mint).await?;
        let candidates = self.adapters[idx].user_rent_candidates(wallet, &intent.mint, intent.side);
        let extra: Vec<Pubkey> = candidates.iter().map(|c| c.0).collect();
        let balances = self.balances.balances(wallet, &intent.mint, &extra).await.map_err(EngineError::Unavailable)?;
        let protocol_rent: u64 = candidates
            .iter()
            .zip(balances.extra_exists.iter().chain(std::iter::repeat(&false)))
            .filter(|(_, exists)| !**exists)
            .map(|(c, _)| c.1 .0)
            .sum();
        let amount_in = match &intent.amount {
            AmountSpec::ExactSolIn { lamports } => lamports.0,
            AmountSpec::SellPercent { bps } => {
                let have = balances.token_raw.unwrap_or(0).saturating_sub(self.locks.reserved_tokens(wallet, &intent.mint));
                portion_bps(have, *bps)
            }
            AmountSpec::ExactTokenIn { raw } => {
                let have = balances.token_raw.unwrap_or(0).saturating_sub(self.locks.reserved_tokens(wallet, &intent.mint));
                if *raw > have {
                    return Err(EngineError::Invalid(format!("sell amount {raw} exceeds spendable balance {have}")));
                }
                *raw
            }
        };
        if amount_in == 0 {
            return Err(EngineError::Invalid("nothing to sell (zero spendable balance)".into()));
        }
        let req = QuoteRequest {
            mint: intent.mint,
            side: intent.side,
            amount_in,
            slippage_bps: intent.slippage.get(),
            max_state_age: self.cfg.max_state_age,
        };
        let mut pq = self.quote_with_refresh(idx, &req).await?;
        pq.rent_lamports = Lamports(pq.rent_lamports.0.saturating_add(protocol_rent));
        let quote = self.to_quote(intent, &pq, &balances, policy)?;
        Ok((quote, Prepared { adapter: idx, pq, balances, at: Instant::now() }))
    }

    fn compute_budget(&self, intent: &TradeIntent, pq: &ProtocolQuote, policy: &WalletPolicy) -> ComputeBudget {
        self.compute_budget_with_limit(intent, pq.compute_units, policy)
    }

    fn compute_budget_with_limit(&self, intent: &TradeIntent, unit_limit: u32, policy: &WalletPolicy) -> ComputeBudget {
        let cap = intent.fees.max_priority_fee.min(policy.max_priority_fee);
        let price = ComputeBudget::capped_price(unit_limit, intent.fees.compute_unit_price_micro_lamports, cap);
        ComputeBudget { unit_limit, unit_price_micro_lamports: price }
    }

    /// Two *distinct* orders with identical parameters signed against the same
    /// blockhash would serialize to identical bytes and share one signature —
    /// the cluster would execute only one of them. Under the wallet lock we
    /// give each (wallet, blockhash) a distinct compute-unit limit (+1 CU per
    /// collision), which makes the message unique at zero extra bytes.
    fn unique_unit_limit(&self, wallet: &Pubkey, blockhash: &solana_hash_compat::Hash, base: u32) -> u32 {
        let mut g = self.recent_msgs.lock();
        if g.len() > 4096 {
            g.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(120));
        }
        let e = g.entry((*wallet, *blockhash)).or_insert((0, Instant::now()));
        let limit = base.saturating_add(e.0);
        e.0 += 1;
        limit
    }

    fn to_quote(&self, intent: &TradeIntent, pq: &ProtocolQuote, b: &WalletBalances, policy: &WalletPolicy) -> Result<Quote, EngineError> {
        let cb = self.compute_budget(intent, pq, policy);
        let tip_cap = policy.max_tip.0;
        let plan = self
            .router
            .plan(&intent.senders, intent.fees.tip.0, tip_cap)
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let tip = plan.tip.as_ref().map(|t| t.lamports).unwrap_or(0);
        let ata_rent = if intent.side == Side::Buy && !b.token_account_exists {
            if pq.base_token_program == TOKEN_2022_PROGRAM {
                rent_exempt_minimum(TOKEN_2022_ATA_LEN)
            } else {
                rent_exempt_minimum(TOKEN_ACCOUNT_LEN)
            }
        } else {
            Lamports(0)
        };
        let costs = CostEstimate {
            base_fee: base_fee(1),
            priority_fee: cb.priority_fee(),
            tip: Lamports(tip),
            rent_deposit: ata_rent.checked_add(pq.rent_lamports).map_err(|e| EngineError::Invalid(e.to_string()))?,
            protocol_fee: pq.protocol_fee_lamports.unwrap_or_default(),
        };
        let overhead = costs.total_overhead().map_err(|e| EngineError::Invalid(e.to_string()))?;
        let max_total_debit = match intent.side {
            Side::Buy => Lamports(pq.max_in).checked_add(overhead),
            Side::Sell => Ok(overhead),
        }
        .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let (in_dec, out_dec) = match intent.side {
            Side::Buy => (9, pq.base_decimals),
            Side::Sell => (pq.base_decimals, 9),
        };
        let now = Utc::now();
        Ok(Quote {
            id: Uuid::new_v4(),
            network: intent.network,
            market: pq.market.clone(),
            side: intent.side,
            in_amount: pq.amount_in,
            in_decimals: in_dec,
            expected_out: pq.expected_out,
            out_decimals: out_dec,
            min_out: pq.min_out,
            max_in: pq.max_in,
            price_impact_bps: pq.price_impact_bps,
            costs,
            max_total_debit,
            context_slot: pq.context_slot,
            state_age_ms: pq.state_age_ms,
            created_at: now,
            expires_at: now + chrono::Duration::from_std(self.cfg.quote_ttl).unwrap(),
            source: if self.cfg.network == Network::Mock { QuoteSource::Mock } else { QuoteSource::OnChain },
            warnings: pq.warnings.clone(),
        })
    }

    async fn policy_and_usage(&self, intent: &TradeIntent) -> Result<WalletPolicy, EngineError> {
        if intent.network != self.cfg.network {
            return Err(EngineError::Invalid(format!("order network {} != engine network {}", intent.network, self.cfg.network)));
        }
        if intent.network.is_mainnet() && !self.cfg.mainnet_enabled {
            return Err(PolicyViolation::MainnetDisabled.into());
        }
        intent.validate_shape().map_err(EngineError::Invalid)?;
        let policy = self.policies.policy(intent.wallet_id).await.map_err(EngineError::Unavailable)?;
        let usage = self.store.usage_today(intent.wallet_id).await?;
        policy.check_intent(intent, &usage)?;
        Ok(policy)
    }

    /// Resolve which adapter trades `mint` and describe the market from cache.
    pub async fn describe_market(&self, mint: &Pubkey) -> Result<(&'static str, Option<serde_json::Value>), EngineError> {
        let idx = self.adapter_for(mint).await?;
        let a = &self.adapters[idx];
        if a.describe(mint).is_none() {
            let _ = a.refresh(mint).await;
        }
        Ok((a.name(), a.describe(mint)))
    }

    /// `POST /v1/orders/preview`: full quote + costs, no persistence, no signing.
    pub async fn preview(&self, intent: &TradeIntent, wallet: &Pubkey) -> Result<Quote, EngineError> {
        let policy = self.policy_and_usage(intent).await?;
        let (quote, prep) = self.prepare(intent, wallet, &policy).await?;
        let avail = Lamports(prep.balances.sol.0.saturating_sub(self.locks.reserved_lamports(wallet)));
        policy.check_quote(intent, &quote, avail, Utc::now())?;
        Ok(quote)
    }

    /// `POST /v1/orders`: durable + idempotent creation, then enqueue.
    pub async fn submit(&self, intent: TradeIntent, wallet: Pubkey, trace: Option<LatencyTrace>) -> Result<Order, EngineError> {
        let permit = {
            let q = self.queue.lock().clone();
            match q {
                Some(q) => Some(q.try_reserve_owned().map_err(|_| EngineError::Busy)?),
                None => None,
            }
        };
        let out = self.store.create_order(&intent, wallet).await?;
        match out {
            crate::model::CreateOutcome::Existing(o) => Ok(o),
            crate::model::CreateOutcome::Created(o) => {
                let mut t = trace.unwrap_or_default();
                t.mark(Stage::IntentAccepted);
                self.traces.lock().insert(o.id, t);
                match permit {
                    Some(p) => {
                        p.send(o.id);
                    }
                    None => {
                        // No worker pool (tests): process inline.
                        let _ = self.process(o.id).await;
                    }
                }
                Ok(o)
            }
        }
    }

    /// Advance a CREATED order to QUOTED and then either AWAITING_AUTH or execution.
    pub async fn process(&self, id: Uuid) -> Result<(), EngineError> {
        let o = self.store.get_order(id).await?;
        if o.state != OrderState::Created {
            return Ok(()); // another worker / duplicate delivery
        }
        let policy = match self.policy_and_usage(&o.intent).await {
            Ok(p) => p,
            Err(e) => return self.reject(&o, OrderState::Created, &e.to_string()).await,
        };
        let (quote, prep) = match self.prepare(&o.intent, &o.wallet, &policy).await {
            Ok(x) => x,
            Err(e) => return self.reject(&o, OrderState::Created, &e.to_string()).await,
        };
        self.mark(id, Stage::QuoteReady);
        self.store.save_quote(id, &quote).await?;
        self.store
            .transition(id, OrderState::Created, OrderState::Quoted, serde_json::json!({"quote_id": quote.id, "expected_out": quote.expected_out.to_string(), "min_out": quote.min_out.to_string()}))
            .await?;
        self.prepared.lock().insert(id, prep);
        if o.intent.requires_approval {
            self.store.transition(id, OrderState::Quoted, OrderState::AwaitingAuth, serde_json::json!({})).await?;
            self.emit(&o, OrderState::AwaitingAuth, "quote ready; awaiting confirmation");
            return Ok(());
        }
        self.execute(id, OrderState::Quoted).await
    }

    pub async fn approve(&self, id: Uuid, user_id: Uuid) -> Result<Order, EngineError> {
        let o = self.store.get_order(id).await?;
        if o.user_id != user_id {
            return Err(EngineError::Forbidden);
        }
        if o.state != OrderState::AwaitingAuth {
            return Ok(o); // idempotent: already approved/cancelled/expired
        }
        self.execute(id, OrderState::AwaitingAuth).await?;
        Ok(self.store.get_order(id).await?)
    }

    pub async fn cancel(&self, id: Uuid, user_id: Uuid) -> Result<Order, EngineError> {
        let o = self.store.get_order(id).await?;
        if o.user_id != user_id {
            return Err(EngineError::Forbidden);
        }
        if o.state.is_cancellable() {
            match self.store.reject(id, o.state, "cancelled_by_user").await {
                Ok(()) | Err(StoreError::Conflict { .. }) => {}
                Err(e) => return Err(e.into()),
            }
            self.prepared.lock().remove(&id);
        }
        Ok(self.store.get_order(id).await?)
    }

    async fn reject(&self, o: &Order, from: OrderState, reason: &str) -> Result<(), EngineError> {
        match self.store.reject(o.id, from, reason).await {
            Ok(()) | Err(StoreError::Conflict { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        self.prepared.lock().remove(&o.id);
        self.emit(o, OrderState::Rejected, reason);
        Ok(())
    }

    /// Sign once and submit. `from` is QUOTED or AWAITING_AUTH.
    async fn execute(&self, id: Uuid, from: OrderState) -> Result<(), EngineError> {
        let o = self.store.get_order(id).await?;
        if o.state != from {
            return Ok(());
        }
        let guard_lock = self.locks.lock(&o.wallet).await;
        // Re-read under lock: a concurrent approve may have won.
        let o = self.store.get_order(id).await?;
        if o.state != from {
            return Ok(());
        }
        let policy = match self.policies.policy(o.wallet_id).await {
            Ok(p) => p,
            Err(e) => return self.reject(&o, from, &format!("policy unavailable: {e}")).await,
        };
        // Re-check kill switch / lock at signing time (user may have paused since quoting).
        if policy.paused || policy.locked {
            return self.reject(&o, from, &PolicyViolation::Paused.to_string()).await;
        }
        let approved = o.quote.clone();
        let mut prep = self.prepared.lock().get(&id).cloned();
        let stale_prep = prep.as_ref().map(|p| p.at.elapsed() > Duration::from_secs(1)).unwrap_or(true);
        let quote_expired = approved.as_ref().map(|q| q.is_expired(Utc::now())).unwrap_or(true);
        let mut quote = approved.clone();
        if stale_prep || quote_expired {
            // Re-quote with fresh balances + state. The user's consent was to the
            // approved minimum output: refuse if the fresh price is worse.
            let (q, p) = match self.prepare(&o.intent, &o.wallet, &policy).await {
                Ok(x) => x,
                Err(e) => return self.reject(&o, from, &e.to_string()).await,
            };
            if let Some(a) = &approved {
                if q.expected_out < a.min_out {
                    return self.reject(&o, from, "price moved beyond your approved slippage; request a new quote").await;
                }
            }
            self.store.save_quote(id, &q).await?;
            quote = Some(q);
            prep = Some(p);
        }
        let (quote, prep) = match (quote, prep) {
            (Some(q), Some(p)) => (q, p),
            _ => return self.reject(&o, from, "no quote").await,
        };
        let avail = Lamports(prep.balances.sol.0.saturating_sub(self.locks.reserved_lamports(&o.wallet)));
        if let Err(v) = policy.check_quote(&o.intent, &quote, avail, Utc::now()) {
            return self.reject(&o, from, &v.to_string()).await;
        }
        let plan = match self.router.plan(&o.intent.senders, o.intent.fees.tip.0, policy.max_tip.0) {
            Ok(p) => p,
            Err(e) => return self.reject(&o, from, &e.to_string()).await,
        };
        let bh = match self.blockhash.get(self.cfg.max_blockhash_age) {
            Ok(b) => b,
            Err(e) => return self.reject(&o, from, &format!("blockhash unavailable: {e}")).await,
        };
        let unit_limit = self.unique_unit_limit(&o.wallet, &bh.hash, prep.pq.compute_units);
        let cb = self.compute_budget_with_limit(&o.intent, unit_limit, &policy);
        let signer = match self.signers.signer_for(o.wallet_id).await {
            Ok(s) => s,
            Err(e) => return self.reject(&o, from, &format!("signer unavailable: {e}")).await,
        };
        let swap = match self.adapters[prep.adapter].build_swap(&prep.pq, &o.wallet) {
            Ok(s) => s,
            Err(e) => return self.reject(&o, from, &e.to_string()).await,
        };
        self.store.transition(id, from, OrderState::Signing, serde_json::json!({})).await?;
        let params = BuildParams {
            network: o.intent.network,
            payer: o.wallet,
            instructions: swap.instructions,
            compute: cb,
            tip: plan.tip.as_ref().map(|t| TipInstruction { account: t.account, lamports: Lamports(t.lamports) }),
            blockhash: bh.hash,
            last_valid_block_height: bh.last_valid_block_height,
        };
        let gcfg = GuardConfig {
            max_priority_fee: o.intent.fees.max_priority_fee.min(policy.max_priority_fee),
            max_tip: policy.max_tip,
            tip_accounts: plan.tip.iter().map(|t| t.account).collect(),
        };
        self.mark(id, Stage::TxBuilt);
        let signed = match build_and_sign(&params, &self.guard, &gcfg, signer.as_ref()).await {
            Ok(s) => s,
            Err(e) => return self.reject(&o, OrderState::Signing, &format!("build/sign failed: {e}")).await,
        };
        self.mark(id, Stage::Signed);
        let rec = SignedRecord {
            signature: signed.signature_b58(),
            wire_b64: signed.wire_b64(),
            blockhash: signed.blockhash.to_string(),
            last_valid_block_height: signed.last_valid_block_height,
            compute_unit_limit: cb.unit_limit,
            compute_unit_price_micro_lamports: cb.unit_price_micro_lamports,
            tip_lamports: plan.tip.as_ref().map(|t| t.lamports).unwrap_or(0),
            tip_account: plan.tip.as_ref().map(|t| t.account.to_string()),
            senders: plan.kinds(),
            signed_at: Utc::now(),
        };
        // DURABLE before any send.
        if let Err(e) = self.store.save_signed(id, &rec).await {
            tracing::error!(order_id = %id, error = %e, "failed to persist signed tx; NOT sending");
            let _ = self.store.reject(id, OrderState::Signing, "persist failed before send").await;
            return Err(e.into());
        }
        let token_res = match o.intent.side {
            Side::Sell => Some((o.intent.mint, prep.pq.amount_in)),
            Side::Buy => None,
        };
        self.locks.reserve(&o.wallet, Reservation { order_id: id, lamports: quote.max_total_debit.0, token: token_res });
        drop(guard_lock);

        let ctx = SendContext { order_id: id, signature: rec.signature.clone(), last_valid_block_height: rec.last_valid_block_height, attempt: 0, rebroadcast: false };
        self.mark(id, Stage::SendStart);
        let attempts = Router::fan_out(&plan, &signed.wire, &ctx).await;
        if attempts.iter().any(|a| a.outcome.is_success()) {
            self.mark(id, Stage::SenderAck);
        }
        let recs: Vec<AttemptRecord> = attempts
            .iter()
            .map(|a| AttemptRecord { order_id: id, signature: rec.signature.clone(), attempt: a.clone(), rebroadcast: false })
            .collect();
        let _ = self.store.record_attempts(&recs).await;
        let mut o2 = o.clone();
        o2.signed = Some(rec.clone());
        match verdict(&attempts) {
            FanOutVerdict::NothingForwarded => {
                let reasons: Vec<String> = attempts.iter().map(|a| format!("{}: {:?}", a.sender.as_str(), a.outcome)).collect();
                self.store.reject(id, OrderState::Signed, &format!("all senders refused: {}", reasons.join("; "))).await?;
                self.locks.release(&o.wallet, id);
                self.emit(&o2, OrderState::Rejected, "all senders refused before forwarding; no fee paid");
            }
            FanOutVerdict::PossiblyInFlight => {
                self.store
                    .transition(id, OrderState::Signed, OrderState::Submitted, serde_json::json!({"senders": rec.senders, "attempts": attempts.len()}))
                    .await?;
                self.emit(&o2, OrderState::Submitted, format!("submitted; signature {}", rec.signature));
                let trace = self.traces.lock().get(&id).cloned();
                self.observer.track(Track {
                    order_id: id,
                    user_id: o.user_id,
                    wallet_id: o.wallet_id,
                    wallet: o.wallet,
                    mint: o.intent.mint,
                    side: o.intent.side,
                    signature: rec.signature.clone(),
                    wire: Arc::new(signed.wire.clone()),
                    last_valid_block_height: rec.last_valid_block_height,
                    plan: Some(plan),
                    state: OrderState::Submitted,
                    recovered: false,
                    tip_lamports: rec.tip_lamports,
                    trace,
                });
            }
        }
        self.prepared.lock().remove(&id);
        Ok(())
    }

    /// Startup reconciliation (master prompt §4: reconcile interrupted processes).
    pub async fn recover(&self) -> Result<usize, EngineError> {
        let mut n = 0;
        for o in self.store.list_unresolved().await? {
            n += 1;
            match o.state {
                OrderState::Created | OrderState::Quoted => {
                    // Nothing was signed: safe to reject.
                    let _ = self.store.reject(o.id, o.state, "process restarted before execution; nothing was signed").await;
                }
                OrderState::AwaitingAuth => {} // approval re-quotes
                OrderState::Signing => {
                    let _ = self.store.reject(o.id, OrderState::Signing, "process restarted while signing; signature never persisted, nothing sent").await;
                }
                OrderState::Signed | OrderState::Submitted | OrderState::Observed | OrderState::Confirmed | OrderState::UnknownReconcile => {
                    let Some(rec) = o.signed.clone() else {
                        tracing::error!(order_id = %o.id, "in-flight order without signed record");
                        continue;
                    };
                    let mut state = o.state;
                    if matches!(state, OrderState::Signed | OrderState::Submitted | OrderState::Observed) {
                        self.store
                            .transition(o.id, state, OrderState::UnknownReconcile, serde_json::json!({"reason": "restart"}))
                            .await?;
                        state = OrderState::UnknownReconcile;
                    }
                    use base64::Engine as _;
                    let wire = base64::engine::general_purpose::STANDARD.decode(&rec.wire_b64).unwrap_or_default();
                    // Reserve conservatively until resolved.
                    let debit = o.quote.as_ref().map(|q| q.max_total_debit.0).unwrap_or(0);
                    self.locks.reserve(&o.wallet, Reservation { order_id: o.id, lamports: debit, token: None });
                    self.observer.track(Track {
                        order_id: o.id,
                        user_id: o.user_id,
                        wallet_id: o.wallet_id,
                        wallet: o.wallet,
                        mint: o.intent.mint,
                        side: o.intent.side,
                        signature: rec.signature.clone(),
                        wire: Arc::new(wire),
                        last_valid_block_height: rec.last_valid_block_height,
                        plan: None, // recovered orders are observed, never rebroadcast
                        state,
                        recovered: true,
                        tip_lamports: rec.tip_lamports,
                        trace: None,
                    });
                }
                _ => {}
            }
        }
        Ok(n)
    }
}
