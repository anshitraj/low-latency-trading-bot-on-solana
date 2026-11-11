//! Independent landing observer + reconciler.
//!
//! Polls `getSignatureStatuses` for every tracked signature, advances the
//! order state from on-chain evidence only, rebroadcasts the SAME bytes while
//! the blockhash is valid, and declares EXPIRED only when the *finalized*
//! block height exceeds `lastValidBlockHeight` and a history search still
//! cannot find the signature.

use crate::engine::OrderUpdate;
use crate::model::{AttemptRecord, FillRecord};
use crate::store::{OrderStore, StoreError};
use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use trader_domain::fees::rent_exempt_minimum;
use trader_domain::pnl::Fill;
use trader_domain::{Commitment, LatencyTrace, OrderState, Side, Stage};
use trader_execution::WalletLocks;
use trader_market_data::{RpcClient, SignatureStatus, TransactionDetails};
use trader_router::{RoutePlan, Router, SendContext};
use uuid::Uuid;

#[derive(Clone)]
pub struct Track {
    pub order_id: Uuid,
    pub user_id: Uuid,
    pub wallet_id: Uuid,
    pub wallet: Pubkey,
    pub mint: Pubkey,
    pub side: Side,
    pub signature: String,
    pub wire: Arc<Vec<u8>>,
    pub last_valid_block_height: u64,
    pub plan: Option<RoutePlan>,
    pub state: OrderState,
    pub recovered: bool,
    pub tip_lamports: u64,
    pub trace: Option<LatencyTrace>,
}

struct Tracked {
    t: Track,
    last_rebroadcast: Instant,
    rebroadcasts: u32,
    missing_after_observed: u32,
    fill_saved: bool,
}

#[derive(Clone, Debug)]
pub struct ObserverConfig {
    pub poll_interval: Duration,
    pub rebroadcast_interval: Duration,
    pub max_rebroadcasts: u32,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self { poll_interval: Duration::from_millis(250), rebroadcast_interval: Duration::from_millis(2_000), max_rebroadcasts: 30 }
    }
}

/// Completed traces for benchmarks (order id, trace, landing slot).
pub type TraceSink = Arc<Mutex<Vec<(Uuid, LatencyTrace, Option<u64>)>>>;

#[derive(Clone)]
pub struct ObserverHandle {
    inner: Arc<Mutex<HashMap<String, Tracked>>>,
}

impl ObserverHandle {
    pub fn track(&self, t: Track) {
        let mut g = self.inner.lock();
        g.entry(t.signature.clone()).or_insert(Tracked {
            t,
            last_rebroadcast: Instant::now(),
            rebroadcasts: 0,
            missing_after_observed: 0,
            fill_saved: false,
        });
    }
    pub fn tracked(&self) -> usize {
        self.inner.lock().len()
    }
}

pub struct Observer {
    pub handle: ObserverHandle,
    store: Arc<dyn OrderStore>,
    rpc: RpcClient,
    locks: WalletLocks,
    updates: broadcast::Sender<OrderUpdate>,
    cfg: ObserverConfig,
    pub traces: TraceSink,
}

impl Observer {
    pub fn new(store: Arc<dyn OrderStore>, rpc: RpcClient, locks: WalletLocks, updates: broadcast::Sender<OrderUpdate>, cfg: ObserverConfig) -> Self {
        Self { handle: ObserverHandle { inner: Default::default() }, store, rpc, locks, updates, cfg, traces: Default::default() }
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.cfg.poll_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = self.poll_once().await {
                    tracing::warn!(error = %e, "observer poll failed");
                }
            }
        })
    }

    fn notify(&self, t: &Track, state: OrderState, detail: String, fill: Option<FillRecord>) {
        let _ = self.updates.send(OrderUpdate { order_id: t.order_id, user_id: t.user_id, state, signature: Some(t.signature.clone()), detail, fill });
    }

    async fn advance(&self, tr: &mut Tracked, to: OrderState, detail: serde_json::Value) -> Result<bool, StoreError> {
        if tr.t.state == to || !tr.t.state.can_transition_to(to) {
            return Ok(false);
        }
        match self.store.transition(tr.t.order_id, tr.t.state, to, detail).await {
            Ok(()) => {
                tr.t.state = to;
                Ok(true)
            }
            Err(StoreError::Conflict { found, .. }) => {
                tr.t.state = found;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// One reconciliation pass. Public for deterministic tests.
    pub async fn poll_once(&self) -> Result<(), String> {
        let sigs: Vec<(String, bool)> = self.handle.inner.lock().values().map(|t| (t.t.signature.clone(), t.t.recovered || t.t.state == OrderState::UnknownReconcile)).collect();
        if sigs.is_empty() {
            return Ok(());
        }
        let finalized_height = self.rpc.get_block_height(Commitment::Finalized).await.ok();
        for chunk in sigs.chunks(256) {
            let list: Vec<String> = chunk.iter().map(|(s, _)| s.clone()).collect();
            let search = chunk.iter().any(|(_, r)| *r);
            let (_, statuses) = self.rpc.get_signature_statuses(&list, search).await.map_err(|e| e.to_string())?;
            for (sig, st) in list.iter().zip(statuses) {
                self.handle_status(sig, st, finalized_height).await;
            }
        }
        Ok(())
    }

    async fn handle_status(&self, sig: &str, st: Option<SignatureStatus>, finalized_height: Option<u64>) {
        let Some(mut tr) = self.handle.inner.lock().remove(sig) else { return };
        let keep = match self.handle_one(&mut tr, st, finalized_height).await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(signature = sig, error = %e, "observer store error");
                true
            }
        };
        if keep {
            self.handle.inner.lock().insert(sig.to_string(), tr);
        } else {
            self.locks.release(&tr.t.wallet, tr.t.order_id);
            if let Some(trace) = tr.t.trace.take() {
                self.traces.lock().push((tr.t.order_id, trace, None));
            }
        }
    }

    /// Returns whether to keep tracking.
    async fn handle_one(&self, tr: &mut Tracked, st: Option<SignatureStatus>, finalized_height: Option<u64>) -> Result<bool, StoreError> {
        match st {
            Some(s) => {
                tr.missing_after_observed = 0;
                let failed = s.err.is_some();
                let commitment = s.confirmation_status.unwrap_or(Commitment::Processed);
                if let Some(t) = tr.t.trace.as_mut() {
                    t.mark(Stage::FirstObservedProcessed);
                    if commitment >= Commitment::Confirmed {
                        t.mark(Stage::Confirmed);
                    }
                    if commitment == Commitment::Finalized {
                        t.mark(Stage::Finalized);
                    }
                }
                let target = commitment.landed_state(failed);
                let detail = serde_json::json!({"slot": s.slot, "commitment": commitment.as_str(), "err": s.err});
                // Walk through OBSERVED first for a coherent journal.
                if matches!(tr.t.state, OrderState::Submitted | OrderState::UnknownReconcile | OrderState::Signed) && target != OrderState::Observed && self.advance(tr, OrderState::Observed, detail.clone()).await? {
                    self.notify(&tr.t, OrderState::Observed, format!("landed in slot {} (processed)", s.slot), None);
                }
                if self.advance(tr, target, detail).await? {
                    let msg = match target {
                        OrderState::Observed => format!("landed in slot {} (processed)", s.slot),
                        OrderState::Confirmed => format!("confirmed in slot {}", s.slot),
                        OrderState::Finalized => format!("finalized in slot {}", s.slot),
                        OrderState::FailedOnchain => format!("failed on-chain in slot {}: {}", s.slot, s.err.as_ref().map(|e| e.to_string()).unwrap_or_default()),
                        _ => String::new(),
                    };
                    if !matches!(target, OrderState::Confirmed | OrderState::Finalized | OrderState::FailedOnchain) {
                        self.notify(&tr.t, target, msg, None);
                    } else {
                        // Fill decode happens below; notify after so the message has real deltas.
                        let fill = if !tr.fill_saved { self.save_fill(tr, failed).await } else { None };
                        self.notify(&tr.t, target, msg, fill);
                    }
                } else if commitment >= Commitment::Confirmed && !tr.fill_saved {
                    self.save_fill(tr, failed).await;
                }
                if commitment >= Commitment::Confirmed && tr.fill_saved {
                    // Funds are now reflected in balances; drop the reservation early.
                    self.locks.release(&tr.t.wallet, tr.t.order_id);
                }
                Ok(!tr.t.state.is_terminal())
            }
            None => {
                if tr.t.state == OrderState::Observed {
                    tr.missing_after_observed += 1;
                    if tr.missing_after_observed >= 3 {
                        // Observation dropped (minority fork / node disagreement).
                        self.advance(tr, OrderState::Submitted, serde_json::json!({"reason": "observation dropped"})).await?;
                        self.notify(&tr.t, OrderState::Submitted, "landing observation dropped (fork?); still watching".into(), None);
                    }
                }
                if let Some(h) = finalized_height {
                    if h > tr.t.last_valid_block_height {
                        // Last check with full history before declaring expiry.
                        let (_, again) = self
                            .rpc
                            .get_signature_statuses(std::slice::from_ref(&tr.t.signature), true)
                            .await
                            .map_err(|e| StoreError::Backend(e.to_string()))?;
                        if let Some(Some(s)) = again.into_iter().next() {
                            return Box::pin(self.handle_one(tr, Some(s), Some(h))).await;
                        }
                        self.advance(tr, OrderState::Expired, serde_json::json!({"finalized_block_height": h, "last_valid_block_height": tr.t.last_valid_block_height})).await?;
                        self.notify(&tr.t, OrderState::Expired, "expired: blockhash no longer valid and transaction never landed; no fee paid".into(), None);
                        return Ok(false);
                    }
                }
                // Rebroadcast the same bytes (never re-sign) while still valid.
                if !tr.t.recovered && tr.rebroadcasts < self.cfg.max_rebroadcasts && tr.last_rebroadcast.elapsed() >= self.cfg.rebroadcast_interval {
                    if let Some(plan) = tr.t.plan.clone() {
                        tr.rebroadcasts += 1;
                        tr.last_rebroadcast = Instant::now();
                        let ctx = SendContext {
                            order_id: tr.t.order_id,
                            signature: tr.t.signature.clone(),
                            last_valid_block_height: tr.t.last_valid_block_height,
                            attempt: tr.rebroadcasts,
                            rebroadcast: true,
                        };
                        let at = Router::fan_out(&plan, &tr.t.wire, &ctx).await;
                        let recs: Vec<AttemptRecord> = at.into_iter().map(|a| AttemptRecord { order_id: tr.t.order_id, signature: tr.t.signature.clone(), attempt: a, rebroadcast: true }).collect();
                        let _ = self.store.record_attempts(&recs).await;
                    }
                }
                Ok(true)
            }
        }
    }

    async fn save_fill(&self, tr: &mut Tracked, failed: bool) -> Option<FillRecord> {
        let tx = match self.rpc.get_transaction(&tr.t.signature, Commitment::Confirmed).await {
            Ok(Some(tx)) => tx,
            Ok(None) => return None, // not yet available from this node; retry next poll
            Err(e) => {
                tracing::warn!(error = %e, "getTransaction failed");
                return None;
            }
        };
        let fill = compute_fill(&tx, &tr.t.wallet, &tr.t.mint, tr.t.tip_lamports, failed);
        if self.store.save_fill(tr.t.order_id, &fill).await.is_err() {
            return None;
        }
        tr.fill_saved = true;
        if fill.success {
            let rent = created_token_account_rent(&tx, &tr.t.wallet, &tr.t.mint);
            let pf = match tr.t.side {
                Side::Buy if fill.token_delta_raw > 0 => Some(Fill {
                    qty_raw: fill.token_delta_raw as u128,
                    lamports: ((-fill.sol_delta_lamports).max(0) as u128).saturating_sub(rent as u128),
                    fees_lamports: (fill.fee_lamports + fill.tip_lamports) as u128,
                    rent_lamports: rent as u128,
                }),
                Side::Sell if fill.token_delta_raw < 0 => Some(Fill {
                    qty_raw: (-fill.token_delta_raw) as u128,
                    lamports: fill.sol_delta_lamports.max(0) as u128,
                    fees_lamports: (fill.fee_lamports + fill.tip_lamports) as u128,
                    rent_lamports: 0,
                }),
                _ => None,
            };
            if let Some(pf) = pf {
                if let Err(e) = self.store.apply_position_fill(tr.t.user_id, tr.t.wallet_id, tr.t.mint, tr.t.side, &pf).await {
                    tracing::warn!(error = %e, "position update failed");
                }
            }
        }
        Some(fill)
    }
}

pub fn compute_fill(tx: &TransactionDetails, wallet: &Pubkey, mint: &Pubkey, tip: u64, failed: bool) -> FillRecord {
    let w = wallet.to_string();
    let m = mint.to_string();
    let decimals = tx.token_balances.iter().find(|t| t.mint == m).map(|t| t.decimals);
    FillRecord {
        slot: tx.slot,
        success: !failed && tx.err.is_none(),
        error: tx.err.as_ref().map(|e| e.to_string()),
        fee_lamports: tx.fee,
        sol_delta_lamports: tx.lamport_delta(&w).unwrap_or(0),
        token_delta_raw: tx.token_delta(&w, &m),
        token_decimals: decimals,
        compute_units_consumed: tx.compute_units_consumed,
        tip_lamports: if failed { 0 } else { tip },
    }
}

/// Rent moved into the wallet's token account if this tx created it.
pub fn created_token_account_rent(tx: &TransactionDetails, wallet: &Pubkey, mint: &Pubkey) -> u64 {
    let w = wallet.to_string();
    let m = mint.to_string();
    for t in tx.token_balances.iter().filter(|t| t.mint == m && t.owner.as_deref() == Some(w.as_str())) {
        let i = t.account_index as usize;
        if let (Some(pre), Some(post)) = (tx.pre_balances.get(i), tx.post_balances.get(i)) {
            if *pre == 0 && *post > 0 {
                return *post;
            }
        }
    }
    let _ = rent_exempt_minimum(0);
    0
}
