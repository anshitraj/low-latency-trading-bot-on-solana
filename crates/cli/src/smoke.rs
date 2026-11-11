//! Real-engine smoke trades against a LOCAL validator running the official
//! Pump program bytecode. Verifies every result independently via RPC.

use crate::localnet::{airdrop, require_localnet, rpc};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use trader_domain::intent::FeePreference;
use trader_domain::{AmountSpec, Bps, Commitment, Lamports, LatencyTrace, Network, OrderState, SenderKind, Side, Stage, TradeIntent, WalletPolicy};
use trader_execution::{BlockhashService, WalletLocks};
use trader_market_data::token::{associated_token_address, parse_token_account, TOKEN_2022_PROGRAM};
use trader_orders::balances::RpcBalances;
use trader_orders::{Engine, EngineConfig, MemStore, Observer, ObserverConfig, Order, OrderStore, PolicySource, SignerProvider};
use trader_protocol_pump::PumpAdapter;
use trader_router::{ManagedSender, Router};
use trader_sender_rpc::RpcSender;
use trader_wallets::{ManagedLocalSigner, TxSigner};
use uuid::Uuid;

struct OneSigner(Arc<dyn TxSigner>);
#[async_trait]
impl SignerProvider for OneSigner {
    async fn signer_for(&self, _: Uuid) -> Result<Arc<dyn TxSigner>, String> {
        Ok(self.0.clone())
    }
}
struct FixedPolicy(Mutex<WalletPolicy>);
#[async_trait]
impl PolicySource for FixedPolicy {
    async fn policy(&self, _: Uuid) -> Result<WalletPolicy, String> {
        Ok(self.0.lock().clone())
    }
}

async fn token_balance(rpc: &trader_market_data::RpcClient, owner: &Pubkey, mint: &Pubkey) -> anyhow::Result<u64> {
    let ata = associated_token_address(owner, mint, &TOKEN_2022_PROGRAM);
    let (_, acc) = rpc.get_account(&ata, Commitment::Confirmed).await?;
    Ok(acc.and_then(|a| parse_token_account(&a.owner, &a.data).ok()).map(|t| t.amount).unwrap_or(0))
}

async fn wait_terminal(store: &MemStore, id: Uuid, timeout: Duration) -> anyhow::Result<Order> {
    let start = std::time::Instant::now();
    loop {
        let o = store.get_order(id).await?;
        if o.state.is_terminal() && (o.state != OrderState::Finalized || o.fill.is_some()) {
            return Ok(o);
        }
        if start.elapsed() > timeout {
            bail!("order {id} stuck in {}", o.state);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn smoke_pump(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let rpc = rpc(a)?;
    require_localnet(&rpc).await?;
    let mint = Pubkey::from_str(a.get("mint").ok_or_else(|| anyhow!("--mint required"))?)?;
    let buy = Lamports::from_sol_str(a.get("buy-sol").map(String::as_str).unwrap_or("0.05")).map_err(|e| anyhow!("{e}"))?;
    let iterations: usize = a.get("iterations").map(|s| s.parse()).transpose()?.unwrap_or(1);

    let kp = Keypair::new();
    let wallet = kp.pubkey();
    airdrop(&rpc, &wallet, 20_000_000_000).await?;
    let signer: Arc<dyn TxSigner> = Arc::new(ManagedLocalSigner::new(kp, Network::Localnet));

    let store = Arc::new(MemStore::new());
    let locks = WalletLocks::default();
    let blockhash = BlockhashService::new();
    blockhash.refresh_once(&rpc).await?;
    blockhash.spawn(rpc.clone(), Duration::from_millis(400));
    let mut router = Router::new();
    router.add(ManagedSender::new(Arc::new(RpcSender::standard(rpc.clone(), false)), Duration::from_secs(5), None));
    let (tx, _rx) = broadcast::channel(1024);
    let observer = Arc::new(Observer::new(
        store.clone(),
        rpc.clone(),
        locks.clone(),
        tx.clone(),
        ObserverConfig { poll_interval: Duration::from_millis(100), rebroadcast_interval: Duration::from_secs(2), max_rebroadcasts: 10 },
    ));
    observer.clone().spawn();
    let policy = WalletPolicy {
        max_order: Lamports(2_000_000_000),
        max_daily_turnover: Lamports(1_000_000_000_000),
        max_trades_per_day: 100_000,
        allowed_senders: vec![SenderKind::Rpc],
        ..Default::default()
    };
    let adapter = Arc::new(PumpAdapter::new(rpc.clone()));
    let engine = Engine::new(
        EngineConfig { network: Network::Localnet, max_state_age: Duration::from_secs(3), ..Default::default() },
        store.clone(),
        vec![adapter],
        router,
        blockhash,
        locks,
        Arc::new(OneSigner(signer)),
        Arc::new(RpcBalances { rpc: rpc.clone() }),
        Arc::new(FixedPolicy(Mutex::new(policy))),
        observer.handle.clone(),
        tx,
    );
    let user_id = Uuid::new_v4();
    let wallet_id = Uuid::new_v4();
    let mk = |side: Side, amount: AmountSpec, key: String| TradeIntent {
        id: Uuid::new_v4(),
        user_id,
        wallet_id,
        network: Network::Localnet,
        side,
        mint,
        amount,
        slippage: Bps::new(500).unwrap(),
        fees: FeePreference { compute_unit_price_micro_lamports: 1_000, max_priority_fee: Lamports(100_000), tip: Lamports(0) },
        senders: vec![SenderKind::Rpc],
        idempotency_key: key,
        requires_approval: false,
        created_at: chrono::Utc::now(),
    };

    let mut rows = vec![];
    for it in 0..iterations {
        for side in [Side::Buy, Side::Sell] {
            let sol_before = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
            let tok_before = token_balance(&rpc, &wallet, &mint).await?;
            let amount = match side {
                Side::Buy => AmountSpec::ExactSolIn { lamports: buy },
                Side::Sell => AmountSpec::SellPercent { bps: Bps::MAX },
            };
            let intent = mk(side, amount, format!("smoke-{it}-{}", side.as_str()));
            let send_slot = rpc.get_slot(Commitment::Processed).await?;
            let o = engine.submit(intent, wallet, Some(LatencyTrace::start())).await?;
            let o = wait_terminal(&store, o.id, Duration::from_secs(90)).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let sol_after = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
            let tok_after = token_balance(&rpc, &wallet, &mint).await?;
            let trace = observer.traces.lock().iter().find(|(id, _, _)| *id == o.id).map(|(_, t, _)| t.clone());
            let fill = o.fill.clone();
            let q = o.quote.clone();
            let verified = match &fill {
                Some(f) if f.success => {
                    f.token_delta_raw == tok_after as i128 - tok_before as i128 && f.sol_delta_lamports == sol_after as i128 - sol_before as i128
                }
                _ => false,
            };
            let expected_out = q.as_ref().map(|q| q.expected_out);
            let actual_out = fill.as_ref().map(|f| match side {
                Side::Buy => f.token_delta_raw.max(0) as u64,
                Side::Sell => (f.sol_delta_lamports + f.fee_lamports as i128).max(0) as u64,
            });
            let slippage_bps = match (expected_out, actual_out) {
                (Some(e), Some(a)) if e > 0 => Some(((e as i128 - a as i128) * 10_000 / e as i128) as i64),
                _ => None,
            };
            let debit = (sol_before as i128 - sol_after as i128).max(0) as u64;
            let within_max = side == Side::Sell || q.as_ref().map(|q| debit <= q.max_total_debit.0).unwrap_or(false);
            let row = json!({
                "debit_lamports": debit,
                "quoted_max_total_debit": q.as_ref().map(|q| q.max_total_debit.0),
                "debit_within_quoted_max": within_max,
                "iteration": it,
                "side": side.as_str(),
                "order_id": o.id,
                "state": o.state.as_str(),
                "reject_reason": o.reject_reason,
                "signature": o.signed.as_ref().map(|s| s.signature.clone()),
                "send_slot": send_slot,
                "landed_slot": fill.as_ref().map(|f| f.slot),
                "slots_to_land": fill.as_ref().map(|f| f.slot.saturating_sub(send_slot)),
                "fee_lamports": fill.as_ref().map(|f| f.fee_lamports),
                "compute_units_consumed": fill.as_ref().and_then(|f| f.compute_units_consumed),
                "compute_unit_limit": o.signed.as_ref().map(|s| s.compute_unit_limit),
                "quote_expected_out": expected_out.map(|v| v.to_string()),
                "quote_min_out": q.as_ref().map(|q| q.min_out.to_string()),
                "actual_out_gross": actual_out.map(|v| v.to_string()),
                "quote_to_fill_slippage_bps": slippage_bps,
                "sol_delta": fill.as_ref().map(|f| f.sol_delta_lamports.to_string()),
                "token_delta": fill.as_ref().map(|f| f.token_delta_raw.to_string()),
                "independently_verified_balances": verified,
                "trace_us": trace.as_ref().map(|t| t.to_map()),
                "intent_to_signed_us": trace.as_ref().and_then(|t| t.between_us(Stage::IntentAccepted, Stage::Signed)),
                "signed_to_processed_us": trace.as_ref().and_then(|t| t.between_us(Stage::Signed, Stage::FirstObservedProcessed)),
            });
            println!("{}", serde_json::to_string(&row)?);
            if o.state != OrderState::Finalized || !verified || !within_max {
                rows.push(row);
                write_report(a, &rows)?;
                bail!("smoke failed: {} (verified={verified})", o.state);
            }
            rows.push(row);
        }
    }
    let pos = store.get_position(wallet_id, mint).await?;
    println!("{}", json!({"final_position": pos}));
    write_report(a, &rows)?;
    Ok(())
}

fn write_report(a: &HashMap<String, String>, rows: &[serde_json::Value]) -> anyhow::Result<()> {
    if let Some(p) = a.get("report") {
        if let Some(d) = std::path::Path::new(p).parent() {
            std::fs::create_dir_all(d)?;
        }
        let doc = json!({
            "kind": "localnet_pump_smoke",
            "cluster": "localnet (solana-test-validator, official pump.so from pump-rust-client 0.5.0)",
            "generated_at": chrono::Utc::now().to_rfc3339(),
            "host": std::env::var("COMPUTERNAME").unwrap_or_default(),
            "rows": rows,
        });
        std::fs::write(p, serde_json::to_vec_pretty(&doc)?)?;
    }
    Ok(())
}
