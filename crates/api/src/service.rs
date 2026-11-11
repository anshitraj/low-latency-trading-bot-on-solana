//! Use-cases shared by REST and the Telegram bot.

use crate::appstore::{User, UserSettings, Wallet};
use crate::state::AppState;
use serde::Serialize;
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use trader_domain::intent::FeePreference;
use trader_domain::{AmountSpec, Commitment, Lamports, Network, Quote, Side, TradeIntent};
use trader_execution::risk::screen_mint;
use trader_market_data::token::parse_mint;
use trader_orders::{EngineError, Order};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("private beta: this account is not on the tester allowlist")]
    NotAllowlisted,
    #[error("no trading wallet; create one first")]
    NoWallet,
    #[error("rate limited")]
    RateLimited,
    #[error("{0}")]
    Engine(#[from] EngineError),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<crate::appstore::AppStoreError> for ServiceError {
    fn from(e: crate::appstore::AppStoreError) -> Self {
        match e {
            crate::appstore::AppStoreError::NotFound => ServiceError::NotFound,
            other => ServiceError::Internal(other.to_string()),
        }
    }
}

impl ServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            ServiceError::BadRequest(_) => "bad_request",
            ServiceError::NotFound => "not_found",
            ServiceError::Forbidden => "forbidden",
            ServiceError::NotAllowlisted => "not_allowlisted",
            ServiceError::NoWallet => "no_wallet",
            ServiceError::RateLimited => "rate_limited",
            ServiceError::Engine(EngineError::Policy(_)) => "policy_violation",
            ServiceError::Engine(EngineError::NoMarket) => "unsupported_market",
            ServiceError::Engine(EngineError::Protocol(_)) => "market_unavailable",
            ServiceError::Engine(EngineError::Busy) => "busy",
            ServiceError::Engine(_) => "engine_error",
            ServiceError::Internal(_) => "internal",
        }
    }
}

pub async fn ensure_tg_user(
    st: &AppState,
    telegram_id: i64,
    username: Option<String>,
) -> Result<User, ServiceError> {
    Ok(st
        .app
        .upsert_telegram_user(telegram_id, username, st.cfg.beta_allowed(telegram_id))
        .await?)
}

pub async fn settings(st: &AppState, user: &User) -> Result<UserSettings, ServiceError> {
    Ok(st
        .app
        .get_settings(user.id)
        .await?
        .unwrap_or_else(|| UserSettings::defaults_for(vec![trader_domain::SenderKind::Rpc])))
}

pub async fn default_wallet(st: &AppState, user: &User) -> Result<Option<Wallet>, ServiceError> {
    Ok(st
        .app
        .list_wallets(user.id)
        .await?
        .into_iter()
        .find(|w| w.network == st.cfg.mode))
}

pub async fn create_wallet(st: &AppState, user: &User) -> Result<Wallet, ServiceError> {
    if !user.beta_allowed {
        return Err(ServiceError::NotAllowlisted);
    }
    if let Some(w) = default_wallet(st, user).await? {
        return Ok(w);
    }
    let kek = st
        .kek
        .as_ref()
        .ok_or_else(|| ServiceError::Internal("no KEK configured".into()))?;
    crate::providers::create_managed_wallet(&st.app, kek, user.id, st.cfg.mode)
        .await
        .map_err(ServiceError::Internal)
}

pub fn parse_mint_arg(s: &str) -> Result<Pubkey, ServiceError> {
    trader_domain::pubkey::parse(s)
        .map_err(|e| ServiceError::BadRequest(format!("invalid mint: {e}")))
}

#[allow(clippy::too_many_arguments)]
pub async fn build_intent(
    st: &AppState,
    user: &User,
    wallet: &Wallet,
    side: Side,
    mint: Pubkey,
    amount: AmountSpec,
    idempotency_key: String,
    require_approval: Option<bool>,
) -> Result<TradeIntent, ServiceError> {
    let s = settings(st, user).await?;
    let mut senders = s.senders.clone();
    senders.retain(|k| st.senders.contains(k));
    if senders.is_empty() {
        senders = vec![trader_domain::SenderKind::Rpc];
    }
    Ok(TradeIntent {
        id: Uuid::new_v4(),
        user_id: user.id,
        wallet_id: wallet.id,
        network: st.cfg.mode,
        side,
        mint,
        amount,
        slippage: s.slippage,
        fees: FeePreference {
            compute_unit_price_micro_lamports: s.compute_unit_price_micro_lamports,
            max_priority_fee: s.max_priority_fee,
            tip: s.tip,
        },
        senders,
        idempotency_key,
        requires_approval: require_approval.unwrap_or(s.confirm_trades),
        created_at: chrono::Utc::now(),
    })
}

pub async fn submit(
    st: &AppState,
    user: &User,
    intent: TradeIntent,
    trace: Option<trader_domain::LatencyTrace>,
) -> Result<Order, ServiceError> {
    if !user.beta_allowed {
        return Err(ServiceError::NotAllowlisted);
    }
    if !st.allow(&format!("orders:{}", user.id), 30) {
        return Err(ServiceError::RateLimited);
    }
    let wallet = st.app.get_wallet(intent.wallet_id).await?;
    if wallet.user_id != user.id {
        return Err(ServiceError::Forbidden);
    }
    let pk = trader_domain::pubkey::parse(&wallet.pubkey).map_err(ServiceError::Internal)?;
    let o = st.engine.submit(intent, pk, trace).await?;
    st.metrics.inc("orders_submitted_total", &[]);
    Ok(o)
}

pub async fn preview(
    st: &AppState,
    user: &User,
    intent: &TradeIntent,
) -> Result<Quote, ServiceError> {
    let wallet = st.app.get_wallet(intent.wallet_id).await?;
    if wallet.user_id != user.id {
        return Err(ServiceError::Forbidden);
    }
    let pk = trader_domain::pubkey::parse(&wallet.pubkey).map_err(ServiceError::Internal)?;
    Ok(st.engine.preview(intent, &pk).await?)
}

pub async fn order_for_user(st: &AppState, user: &User, id: Uuid) -> Result<Order, ServiceError> {
    let o = st
        .orders
        .get_order(id)
        .await
        .map_err(|_| ServiceError::NotFound)?;
    if o.user_id != user.id {
        return Err(ServiceError::NotFound); // do not reveal existence
    }
    Ok(o)
}

#[derive(Serialize)]
pub struct TokenInfo {
    pub mint: String,
    pub network: Network,
    pub supported: bool,
    pub adapter: Option<&'static str>,
    pub market: Option<Value>,
    pub mint_info: Option<Value>,
    pub risk: Option<trader_execution::RiskReport>,
    pub unsupported_reason: Option<String>,
    pub disclaimer: &'static str,
}

pub const RISK_DISCLAIMER: &str = "Risk flags come from on-chain data only and cannot detect every scam. No token is 'safe'. Trading memecoins can lose 100% of funds.";

pub async fn token_info(st: &AppState, mint: Pubkey) -> TokenInfo {
    let (mint_info, risk) = match st.rpc.get_account(&mint, Commitment::Confirmed).await {
        Ok((_, Some(acc))) => match parse_mint(&acc.owner, &acc.data) {
            Ok(m) => {
                let top10 = match st.rpc.get_token_largest_accounts(&mint).await {
                    Ok(v) if m.supply > 0 => Some(
                        (v.iter().take(10).map(|(_, a)| *a as u128).sum::<u128>() * 10_000
                            / m.supply as u128) as u32,
                    ),
                    _ => None,
                };
                let r = screen_mint(&m, top10);
                (
                    Some(
                        json!({"decimals": m.decimals, "supply": m.supply.to_string(), "mint_authority": m.mint_authority.map(|p| p.to_string()), "freeze_authority": m.freeze_authority.map(|p| p.to_string()), "token_program": m.program.id().to_string(), "extensions": m.extensions.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>(), "top10_includes_curve_or_pool": true}),
                    ),
                    Some(r),
                )
            }
            Err(e) => (
                None,
                Some(trader_execution::RiskReport {
                    flags: vec![trader_execution::RiskFlag {
                        code: "not_a_mint",
                        level: trader_execution::RiskLevel::Block,
                        message: e.to_string(),
                    }],
                    top10_holder_bps: None,
                    source: "on-chain",
                }),
            ),
        },
        _ => (None, None),
    };
    match st.engine.describe_market(&mint).await {
        Ok((name, market)) => TokenInfo {
            mint: mint.to_string(),
            network: st.cfg.mode,
            supported: true,
            adapter: Some(name),
            market,
            mint_info,
            risk,
            unsupported_reason: None,
            disclaimer: RISK_DISCLAIMER,
        },
        Err(e) => TokenInfo {
            mint: mint.to_string(),
            network: st.cfg.mode,
            supported: false,
            adapter: None,
            market: None,
            mint_info,
            risk,
            unsupported_reason: Some(e.to_string()),
            disclaimer: RISK_DISCLAIMER,
        },
    }
}

pub async fn portfolio(st: &AppState, user: &User) -> Result<Value, ServiceError> {
    let mut out = vec![];
    for w in st.app.list_wallets(user.id).await? {
        let pk = trader_domain::pubkey::parse(&w.pubkey).map_err(ServiceError::Internal)?;
        let sol = st
            .rpc
            .get_balance(&pk, Commitment::Confirmed)
            .await
            .ok()
            .map(|(l, _)| Lamports(l));
        let mut positions = vec![];
        for (mint, p) in st
            .orders
            .list_positions(w.id)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?
        {
            // Unrealized PnL uses an executable sell quote for the full size; no quote => null (never fabricated).
            let est = if p.qty_raw > 0 {
                let intent = build_intent(
                    st,
                    user,
                    &w,
                    Side::Sell,
                    mint,
                    AmountSpec::ExactTokenIn {
                        raw: p.qty_raw.min(u64::MAX as u128) as u64,
                    },
                    format!("pv:{}", Uuid::new_v4()),
                    Some(true),
                )
                .await
                .ok();
                match intent {
                    Some(i) => st
                        .engine
                        .preview(&i, &pk)
                        .await
                        .ok()
                        .map(|q| q.expected_out as u128),
                    None => None,
                }
            } else {
                None
            };
            positions.push(json!({
                "mint": mint.to_string(),
                "qty_raw": p.qty_raw.to_string(),
                "cost_basis_lamports": p.cost_basis_lamports.to_string(),
                "realized_pnl_lamports": p.realized_pnl_lamports.to_string(),
                "unrealized_pnl_lamports": p.unrealized(est).map(|v| v.to_string()),
                "estimated_exit_lamports": est.map(|v| v.to_string()),
                "price_source": if est.is_some() { "executable sell quote (on-chain state)" } else { "unavailable" },
                "fees_paid_lamports": p.fees_paid_lamports.to_string(),
                "rent_locked_lamports": p.rent_locked_lamports.to_string(),
                "buys": p.buys, "sells": p.sells,
            }));
        }
        out.push(json!({"wallet": w, "sol_lamports": sol, "positions": positions}));
    }
    Ok(json!({"cost_basis_method": "weighted_average", "wallets": out}))
}

pub async fn airdrop_mock(st: &AppState, user: &User) -> Result<Lamports, ServiceError> {
    let w = default_wallet(st, user)
        .await?
        .ok_or(ServiceError::NoWallet)?;
    let pk = trader_domain::pubkey::parse(&w.pubkey).map_err(ServiceError::Internal)?;
    match (&st.mock, st.cfg.mode) {
        (Some(m), Network::Mock) => {
            m.chain.fund(&pk, 2_000_000_000);
            Ok(Lamports(2_000_000_000))
        }
        (_, Network::Localnet | Network::Devnet) => {
            let amt = if st.cfg.mode == Network::Devnet {
                1_000_000_000
            } else {
                5_000_000_000
            };
            st.rpc
                .request_airdrop(&pk, amt)
                .await
                .map_err(|e| ServiceError::BadRequest(format!("airdrop failed: {e}")))?;
            Ok(Lamports(amt))
        }
        _ => Err(ServiceError::BadRequest(
            "airdrop is only available on mock/localnet/devnet".into(),
        )),
    }
}

pub fn provider_health(st: &AppState) -> Value {
    let bh = st
        .engine
        .blockhash
        .get(std::time::Duration::from_secs(3600))
        .ok();
    let senders: Vec<Value> = st.engine.router.all().iter().map(|s| json!({"kind": s.inner.kind().as_str(), "endpoint": s.inner.endpoint(), "health": s.health.snapshot()})).collect();
    json!({
        "mode": st.cfg.mode,
        "mainnet_trading_enabled": st.cfg.mainnet_trading_enabled,
        "rpc_host": st.rpc.host(),
        "blockhash_age_ms": bh.map(|b| b.fetched_at.elapsed().as_millis() as u64),
        "blockhash_slot": bh.map(|b| b.context_slot),
        "senders": senders,
        "tpu_sender": "not built (see docs/RPC_AND_ROUTING.md)",
    })
}
