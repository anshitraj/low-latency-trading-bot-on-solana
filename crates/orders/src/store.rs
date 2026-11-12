//! Persistence contract. Implementations: `MemStore` (tests, mock mode) and
//! `PgStore` (crates/api). Every state change is a compare-and-set on the
//! current state plus an append-only event row, so two workers can never
//! both advance the same order.

use crate::model::*;
use async_trait::async_trait;
use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use trader_domain::pnl::{Fill, Position};
use trader_domain::{OrderState, Quote, Side, TradeIntent, UsageSnapshot};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("state conflict: expected {expected}, found {found}")]
    Conflict {
        expected: OrderState,
        found: OrderState,
    },
    #[error("illegal transition: {0}")]
    Illegal(#[from] trader_domain::TransitionError),
    #[error("duplicate signature")]
    DuplicateSignature,
    #[error("backend: {0}")]
    Backend(String),
}

#[async_trait]
pub trait OrderStore: Send + Sync {
    /// Idempotent on (user_id, idempotency_key).
    async fn create_order(
        &self,
        intent: &TradeIntent,
        wallet: Pubkey,
    ) -> Result<CreateOutcome, StoreError>;
    async fn get_order(&self, id: Uuid) -> Result<Order, StoreError>;
    async fn list_orders(&self, user_id: Uuid, limit: usize) -> Result<Vec<Order>, StoreError>;
    /// Orders that may need recovery after restart (non-terminal).
    async fn list_unresolved(&self) -> Result<Vec<Order>, StoreError>;
    /// CAS transition with an event row. `detail` is stored in the journal.
    async fn transition(
        &self,
        id: Uuid,
        from: OrderState,
        to: OrderState,
        detail: serde_json::Value,
    ) -> Result<(), StoreError>;
    async fn save_quote(&self, id: Uuid, quote: &Quote) -> Result<(), StoreError>;
    /// CREATED -> QUOTED (storing `quote`) -> `then` (AWAITING_AUTH or SIGNING),
    /// both journaled. PgStore does it in one atomic round trip; this default
    /// is CAS-safe (it can only lose, never double-apply).
    /// `Conflict` if the order is no longer CREATED.
    async fn quote_and_advance(
        &self,
        id: Uuid,
        quote: &Quote,
        detail: serde_json::Value,
        then: OrderState,
    ) -> Result<(), StoreError> {
        self.transition(id, OrderState::Created, OrderState::Quoted, detail)
            .await?;
        self.save_quote(id, quote).await?;
        self.transition(id, OrderState::Quoted, then, serde_json::json!({}))
            .await
    }
    /// Must be durable before any send. Transitions SIGNING -> SIGNED atomically.
    async fn save_signed(&self, id: Uuid, signed: &SignedRecord) -> Result<(), StoreError>;
    async fn reject(&self, id: Uuid, from: OrderState, reason: &str) -> Result<(), StoreError>;
    async fn record_attempts(&self, recs: &[AttemptRecord]) -> Result<(), StoreError>;
    async fn save_fill(&self, id: Uuid, fill: &FillRecord) -> Result<(), StoreError>;
    async fn usage_today(&self, wallet_id: Uuid) -> Result<UsageSnapshot, StoreError>;
    /// Idempotent per order: a second call for the same order_id is a no-op
    /// returning None (restart reconciliation can re-observe a fill).
    async fn apply_position_fill(
        &self,
        order_id: Uuid,
        user_id: Uuid,
        wallet_id: Uuid,
        mint: Pubkey,
        side: Side,
        fill: &Fill,
    ) -> Result<Option<Position>, StoreError>;
    async fn get_position(
        &self,
        wallet_id: Uuid,
        mint: Pubkey,
    ) -> Result<Option<Position>, StoreError>;
    async fn list_positions(&self, wallet_id: Uuid) -> Result<Vec<(Pubkey, Position)>, StoreError>;
    async fn events(&self, id: Uuid) -> Result<Vec<OrderEvent>, StoreError>;
    async fn find_by_signature(&self, sig: &str) -> Result<Option<Order>, StoreError>;
}

#[derive(Default)]
struct MemInner {
    orders: HashMap<Uuid, Order>,
    idem: HashMap<(Uuid, String), Uuid>,
    sigs: HashMap<String, Uuid>,
    events: Vec<OrderEvent>,
    attempts: Vec<AttemptRecord>,
    positions: HashMap<(Uuid, Pubkey), Position>,
    /// (wallet_id, day) -> (non-rejected trades, buy turnover)
    daily: HashMap<(Uuid, chrono::NaiveDate), (u32, u64)>,
    applied_fills: std::collections::HashSet<Uuid>,
}

#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<MemInner>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn attempts(&self) -> Vec<AttemptRecord> {
        self.inner.lock().attempts.clone()
    }
    pub fn all_events(&self) -> Vec<OrderEvent> {
        self.inner.lock().events.clone()
    }
    pub fn order_count(&self) -> usize {
        self.inner.lock().orders.len()
    }
    /// Test helper: force a persisted state (simulates a crash snapshot).
    pub fn force_state(&self, id: Uuid, s: OrderState) {
        if let Some(o) = self.inner.lock().orders.get_mut(&id) {
            o.state = s;
        }
    }
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

#[async_trait]
impl OrderStore for MemStore {
    async fn create_order(
        &self,
        intent: &TradeIntent,
        wallet: Pubkey,
    ) -> Result<CreateOutcome, StoreError> {
        let mut g = self.inner.lock();
        let k = (intent.user_id, intent.idempotency_key.clone());
        if let Some(id) = g.idem.get(&k) {
            return Ok(CreateOutcome::Existing(g.orders[id].clone()));
        }
        let o = Order {
            id: intent.id,
            user_id: intent.user_id,
            wallet_id: intent.wallet_id,
            wallet,
            intent: intent.clone(),
            state: OrderState::Created,
            quote: None,
            signed: None,
            fill: None,
            reject_reason: None,
            created_at: now(),
            updated_at: now(),
        };
        g.idem.insert(k, o.id);
        let turnover = match o.intent.amount {
            trader_domain::AmountSpec::ExactSolIn { lamports } => lamports.0,
            _ => 0,
        };
        let d = g
            .daily
            .entry((o.wallet_id, o.created_at.date_naive()))
            .or_default();
        d.0 += 1;
        d.1 = d.1.saturating_add(turnover);
        g.orders.insert(o.id, o.clone());
        g.events.push(OrderEvent {
            order_id: o.id,
            user_id: o.user_id,
            from: None,
            to: OrderState::Created,
            at: now(),
            detail: serde_json::json!({}),
        });
        Ok(CreateOutcome::Created(o))
    }

    async fn get_order(&self, id: Uuid) -> Result<Order, StoreError> {
        self.inner
            .lock()
            .orders
            .get(&id)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn list_orders(&self, user_id: Uuid, limit: usize) -> Result<Vec<Order>, StoreError> {
        let g = self.inner.lock();
        let mut v: Vec<Order> = g
            .orders
            .values()
            .filter(|o| o.user_id == user_id)
            .cloned()
            .collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v.truncate(limit);
        Ok(v)
    }

    async fn list_unresolved(&self) -> Result<Vec<Order>, StoreError> {
        Ok(self
            .inner
            .lock()
            .orders
            .values()
            .filter(|o| !o.state.is_terminal())
            .cloned()
            .collect())
    }

    async fn transition(
        &self,
        id: Uuid,
        from: OrderState,
        to: OrderState,
        detail: serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut g = self.inner.lock();
        let o = g.orders.get_mut(&id).ok_or(StoreError::NotFound)?;
        if o.state != from {
            return Err(StoreError::Conflict {
                expected: from,
                found: o.state,
            });
        }
        o.state = from.transition(to)?;
        o.updated_at = now();
        let user_id = o.user_id;
        g.events.push(OrderEvent {
            order_id: id,
            user_id,
            from: Some(from),
            to,
            at: now(),
            detail,
        });
        Ok(())
    }

    async fn save_quote(&self, id: Uuid, quote: &Quote) -> Result<(), StoreError> {
        let mut g = self.inner.lock();
        let o = g.orders.get_mut(&id).ok_or(StoreError::NotFound)?;
        o.quote = Some(quote.clone());
        Ok(())
    }

    async fn save_signed(&self, id: Uuid, signed: &SignedRecord) -> Result<(), StoreError> {
        let mut g = self.inner.lock();
        if g.sigs.contains_key(&signed.signature) {
            return Err(StoreError::DuplicateSignature);
        }
        let o = g.orders.get_mut(&id).ok_or(StoreError::NotFound)?;
        if o.state != OrderState::Signing {
            return Err(StoreError::Conflict {
                expected: OrderState::Signing,
                found: o.state,
            });
        }
        if o.signed.is_some() {
            return Err(StoreError::DuplicateSignature);
        }
        o.state = OrderState::Signed;
        o.signed = Some(signed.clone());
        o.updated_at = now();
        let user_id = o.user_id;
        g.sigs.insert(signed.signature.clone(), id);
        g.events.push(OrderEvent {
            order_id: id,
            user_id,
            from: Some(OrderState::Signing),
            to: OrderState::Signed,
            at: now(),
            detail: serde_json::json!({"signature": signed.signature, "last_valid_block_height": signed.last_valid_block_height}),
        });
        Ok(())
    }

    async fn reject(&self, id: Uuid, from: OrderState, reason: &str) -> Result<(), StoreError> {
        self.transition(
            id,
            from,
            OrderState::Rejected,
            serde_json::json!({"reason": reason}),
        )
        .await?;
        let mut g = self.inner.lock();
        let mut key = None;
        if let Some(o) = g.orders.get_mut(&id) {
            o.reject_reason = Some(reason.to_string());
            let t = match o.intent.amount {
                trader_domain::AmountSpec::ExactSolIn { lamports } => lamports.0,
                _ => 0,
            };
            key = Some(((o.wallet_id, o.created_at.date_naive()), t));
        }
        if let Some((k, t)) = key {
            if let Some(d) = g.daily.get_mut(&k) {
                d.0 = d.0.saturating_sub(1);
                d.1 = d.1.saturating_sub(t);
            }
        }
        Ok(())
    }

    async fn record_attempts(&self, recs: &[AttemptRecord]) -> Result<(), StoreError> {
        self.inner.lock().attempts.extend_from_slice(recs);
        Ok(())
    }

    async fn save_fill(&self, id: Uuid, fill: &FillRecord) -> Result<(), StoreError> {
        let mut g = self.inner.lock();
        let o = g.orders.get_mut(&id).ok_or(StoreError::NotFound)?;
        o.fill = Some(fill.clone());
        Ok(())
    }

    async fn usage_today(&self, wallet_id: Uuid) -> Result<UsageSnapshot, StoreError> {
        let g = self.inner.lock();
        let today = now().date_naive();
        let mut u = UsageSnapshot::default();
        if let Some((n, t)) = g.daily.get(&(wallet_id, today)) {
            u.trades_today = *n;
            u.turnover_today.0 = *t;
        }
        for ((w, _), p) in g.positions.iter() {
            if *w == wallet_id && p.realized_pnl_lamports < 0 {
                // Approximation in MemStore: cumulative realized loss (PgStore uses today's fills).
                u.realized_loss_today.0 = u
                    .realized_loss_today
                    .0
                    .saturating_add((-p.realized_pnl_lamports) as u64);
            }
        }
        Ok(u)
    }

    async fn apply_position_fill(
        &self,
        order_id: Uuid,
        _user_id: Uuid,
        wallet_id: Uuid,
        mint: Pubkey,
        side: Side,
        fill: &Fill,
    ) -> Result<Option<Position>, StoreError> {
        let mut g = self.inner.lock();
        if !g.applied_fills.insert(order_id) {
            return Ok(None);
        }
        let p = g.positions.entry((wallet_id, mint)).or_default();
        match side {
            Side::Buy => p
                .apply_buy(fill)
                .map_err(|e| StoreError::Backend(e.to_string()))?,
            Side::Sell => {
                p.apply_sell(fill)
                    .map_err(|e| StoreError::Backend(e.to_string()))?;
            }
        }
        Ok(Some(p.clone()))
    }

    async fn get_position(
        &self,
        wallet_id: Uuid,
        mint: Pubkey,
    ) -> Result<Option<Position>, StoreError> {
        Ok(self.inner.lock().positions.get(&(wallet_id, mint)).cloned())
    }

    async fn events(&self, id: Uuid) -> Result<Vec<OrderEvent>, StoreError> {
        Ok(self
            .inner
            .lock()
            .events
            .iter()
            .filter(|e| e.order_id == id)
            .cloned()
            .collect())
    }

    async fn list_positions(&self, wallet_id: Uuid) -> Result<Vec<(Pubkey, Position)>, StoreError> {
        Ok(self
            .inner
            .lock()
            .positions
            .iter()
            .filter(|((w, _), _)| *w == wallet_id)
            .map(|((_, m), p)| (*m, p.clone()))
            .collect())
    }

    async fn find_by_signature(&self, sig: &str) -> Result<Option<Order>, StoreError> {
        let g = self.inner.lock();
        Ok(g.sigs.get(sig).and_then(|id| g.orders.get(id)).cloned())
    }
}
