//! trader-bench: reproducible benchmarks. Every number printed here is
//! measured in this process; nothing is estimated. See docs/BENCHMARKS.md.
//!
//!   trader-bench hotpath  [--iterations N] [--fixture fixtures/pump/localnet_curve.json]
//!   trader-bench engine   [--iterations N] [--concurrency 1,10,50,100]
//!   trader-bench load     [--rates 100,500,1000] [--seconds 20]
//!   trader-bench faults   [--rates 1,5,10] [--orders 100]
//!
//! Results: benchmarks/results/<kind>-<timestamp>.json

use async_trait::async_trait;
use base64::Engine as _;
use hdrhistogram::Histogram;
use parking_lot::Mutex;
use serde_json::{json, Value};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trader_domain::intent::FeePreference;
use trader_domain::{AmountSpec, Bps, ComputeBudget, Lamports, LatencyTrace, Network, OrderState, SenderKind, Side, Stage, TradeIntent, WalletPolicy};
use trader_execution::protocol::{ProtocolAdapter, QuoteRequest};
use trader_execution::{build_and_sign, BlockhashEntry, BlockhashService, BuildParams, GuardConfig, TxGuard, WalletLocks};
use trader_orders::{BalanceSource, Engine, EngineConfig, MemStore, ObserverHandle, OrderStore, PolicySource, SignerProvider, WalletBalances};
use trader_protocol_pump::PumpAdapter;
use trader_router::mock::{MockBehavior, MockSender};
use trader_router::{ManagedSender, Router};
use trader_testkit::Fault;
use trader_tests::{Harness, Opts};
use trader_wallets::{ManagedLocalSigner, TxSigner};
use uuid::Uuid;

fn args() -> (String, HashMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let rest: Vec<String> = it.collect();
    let mut m = HashMap::new();
    let mut i = 0;
    while i < rest.len() {
        if let Some(k) = rest[i].strip_prefix("--") {
            m.insert(k.to_string(), rest.get(i + 1).cloned().unwrap_or_default());
            i += 1;
        }
        i += 1;
    }
    (cmd, m)
}

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 600_000_000_000, 3).unwrap()
}

fn summary(h: &Histogram<u64>, unit: &str) -> Value {
    if h.is_empty() {
        return json!({"n": 0});
    }
    json!({
        "n": h.len(), "unit": unit,
        "p50": h.value_at_quantile(0.50), "p90": h.value_at_quantile(0.90), "p95": h.value_at_quantile(0.95),
        "p99": h.value_at_quantile(0.99), "max": h.max(), "min": h.min(), "mean": h.mean().round() as u64,
    })
}

fn env_info() -> Value {
    json!({
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "host": std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "generated_at": chrono::Utc::now().to_rfc3339(),
    })
}

fn save(kind: &str, v: &Value) {
    let dir = format!("{}/results", env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let p = format!("{dir}/{kind}-{}.json", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    println!("saved {p}");
}

fn load_fixture(path: &str) -> (Pubkey, trader_protocol_pump::CurveSnapshot) {
    let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap();
    let acct = |k: &str| {
        let a = &v["accounts"][k]["account"];
        (Pubkey::from_str(a["owner"].as_str().unwrap()).unwrap(), base64::engine::general_purpose::STANDARD.decode(a["data"][0].as_str().unwrap()).unwrap())
    };
    let mint = Pubkey::from_str(v["mint"].as_str().unwrap()).unwrap();
    let (g, f, c, m) = (acct("global"), acct("fee_config"), acct("bonding_curve"), acct("mint"));
    let ata: u64 = v["curve_base_ata_amount"].as_str().unwrap().parse().unwrap();
    (mint, PumpAdapter::decode_snapshot(&mint, (&g.0, &g.1), (&f.0, &f.1), (&c.0, &c.1), (&m.0, &m.1), ata, 0).unwrap())
}

/// Offline: quote -> build_swap -> compile+guard+sign+serialize. No I/O at all.
async fn hotpath(a: &HashMap<String, String>) {
    let n: usize = a.get("iterations").and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let fixture = a.get("fixture").cloned().unwrap_or_else(|| format!("{}/../fixtures/pump/localnet_curve.json", env!("CARGO_MANIFEST_DIR")));
    let (mint, snap) = load_fixture(&fixture);
    let adapter = PumpAdapter::offline();
    adapter.insert_snapshot(mint, snap);
    let kp = Keypair::new();
    let user = kp.pubkey();
    let signer = ManagedLocalSigner::new(kp, Network::Localnet);
    let mut guard = TxGuard::default();
    for (id, name, rule) in adapter.program_rules() {
        guard.allow_program_with_rule(id, name, rule.unwrap());
    }
    let gcfg = GuardConfig { max_priority_fee: Lamports(1_000_000), max_tip: Lamports(0), tip_accounts: vec![] };
    let (mut hq, mut hb, mut hs, mut ht) = (hist(), hist(), hist(), hist());
    let mut sizes = vec![];
    for i in 0..(n + 1000) {
        let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
        let amount = if side == Side::Buy { 10_000_000 + (i as u64 % 1000) } else { 1_000_000_000 + i as u64 };
        let r = QuoteRequest { mint, side, amount_in: amount, slippage_bps: 300, max_state_age: Duration::from_secs(3600) };
        let t0 = Instant::now();
        let q = adapter.quote_exact_in(&r).unwrap();
        let t1 = Instant::now();
        let swap = adapter.build_swap(&q, &user).unwrap();
        let t2 = Instant::now();
        let p = BuildParams {
            network: Network::Localnet,
            payer: user,
            instructions: swap.instructions,
            compute: ComputeBudget { unit_limit: 150_000, unit_price_micro_lamports: 1_000 },
            tip: None,
            blockhash: Hash::new_from_array([(i % 251) as u8; 32]),
            last_valid_block_height: 1,
        };
        let tx = build_and_sign(&p, &guard, &gcfg, &signer).await.unwrap();
        let t3 = Instant::now();
        if i >= 1000 {
            hq.record((t1 - t0).as_nanos() as u64).unwrap();
            hb.record((t2 - t1).as_nanos() as u64).unwrap();
            hs.record((t3 - t2).as_nanos() as u64).unwrap();
            ht.record((t3 - t0).as_nanos() as u64).unwrap();
            if i < 1010 {
                sizes.push(tx.wire.len());
            }
        }
    }
    let v = json!({
        "kind": "hotpath_offline", "env": env_info(), "fixture": fixture, "warmup": 1000, "iterations": n,
        "note": "Pump quote (official SDK math) + v3 builder + compile/guard/ed25519 sign/serialize; in-memory signer, no network, no DB",
        "quote_ns": summary(&hq, "ns"), "build_swap_ns": summary(&hb, "ns"),
        "compile_guard_sign_serialize_ns": summary(&hs, "ns"), "total_ns": summary(&ht, "ns"),
        "tx_size_bytes_sample": sizes,
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
    save("hotpath", &v);
}

struct Pol;
#[async_trait]
impl PolicySource for Pol {
    async fn policy(&self, _: Uuid) -> Result<WalletPolicy, String> {
        Ok(WalletPolicy {
            max_order: Lamports(10_000_000_000),
            max_daily_turnover: Lamports(u64::MAX / 2),
            max_trades_per_day: u32::MAX,
            allowed_senders: vec![SenderKind::Mock],
            ..Default::default()
        })
    }
}
/// Pre-fetched balances (models a WebSocket-fed balance cache; no I/O).
struct CachedBalances;
#[async_trait]
impl BalanceSource for CachedBalances {
    async fn balances(&self, _: &Pubkey, _: &Pubkey, extra: &[Pubkey]) -> Result<WalletBalances, String> {
        Ok(WalletBalances { sol: Lamports(1_000_000_000_000), token_raw: Some(0), token_account_exists: true, slot: 1, extra_exists: vec![true; extra.len()] })
    }
}

/// Engine warm path: IntentAccepted -> Signed through the real Engine
/// (MemStore writes, policy, quote, guard, sign) with cached state/balances
/// and a MOCK sender. Excludes network and Postgres.
async fn engine(a: &HashMap<String, String>) {
    let n: usize = a.get("iterations").and_then(|s| s.parse().ok()).unwrap_or(2_000);
    let levels: Vec<usize> = a.get("concurrency").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![1, 10, 50, 100]);
    let fixture = format!("{}/../fixtures/pump/localnet_curve.json", env!("CARGO_MANIFEST_DIR"));
    let mut results = vec![];
    for c in levels {
        let (mint, snap) = load_fixture(&fixture);
        let adapter = Arc::new(PumpAdapter::offline());
        adapter.insert_snapshot(mint, snap);
        let store = Arc::new(MemStore::new());
        let bh = BlockhashService::new();
        bh.set(BlockhashEntry { hash: Hash::new_from_array([3; 32]), last_valid_block_height: u64::MAX / 2, context_slot: 1, fetched_at: Instant::now() });
        let mut router = Router::new();
        router.add(ManagedSender::new(Arc::new(MockSender::new(SenderKind::Mock, MockBehavior::Accept)), Duration::from_secs(1), None));
        let (tx, _rx) = tokio::sync::broadcast::channel(65_536);
        // Observer handle without a running observer: nothing polls RPC here.
        let observer = trader_orders::Observer::new(store.clone(), trader_market_data::RpcClient::new(trader_market_data::RpcConfig::new("http://127.0.0.1:9")).unwrap(), WalletLocks::default(), tx.clone(), Default::default());
        let handle: ObserverHandle = observer.handle.clone();
        let mut wallets = vec![];
        let mut signers: HashMap<usize, Arc<dyn TxSigner>> = HashMap::new();
        for i in 0..c.max(1) {
            let kp = Keypair::new();
            wallets.push(kp.pubkey());
            signers.insert(i, Arc::new(ManagedLocalSigner::new(kp, Network::Localnet)));
        }
        // One engine per wallet-signer is not needed: SignerProvider keyed by wallet_id.
        struct MultiSigner(HashMap<Uuid, Arc<dyn TxSigner>>);
        #[async_trait]
        impl SignerProvider for MultiSigner {
            async fn signer_for(&self, w: Uuid) -> Result<Arc<dyn TxSigner>, String> {
                self.0.get(&w).cloned().ok_or_else(|| "no signer".into())
            }
        }
        let wallet_ids: Vec<Uuid> = (0..c.max(1)).map(|_| Uuid::new_v4()).collect();
        let ms = MultiSigner(wallet_ids.iter().enumerate().map(|(i, w)| (*w, signers[&i].clone())).collect());
        let e = Engine::new(
            EngineConfig { network: Network::Localnet, max_state_age: Duration::from_secs(3600), ..Default::default() },
            store.clone(),
            vec![adapter],
            router,
            bh,
            WalletLocks::default(),
            Arc::new(ms),
            Arc::new(CachedBalances),
            Arc::new(Pol),
            handle,
            tx,
        );
        let mk = |i: usize, w: usize| TradeIntent {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            wallet_id: wallet_ids[w],
            network: Network::Localnet,
            side: Side::Buy,
            mint,
            amount: AmountSpec::ExactSolIn { lamports: Lamports(10_000_000 + i as u64) },
            slippage: Bps::new(300).unwrap(),
            fees: FeePreference { compute_unit_price_micro_lamports: 1_000, max_priority_fee: Lamports(100_000), tip: Lamports(0) },
            senders: vec![SenderKind::Mock],
            idempotency_key: format!("bench-{c}-{i}"),
            requires_approval: false,
            created_at: chrono::Utc::now(),
        };
        let h = Arc::new(Mutex::new(hist()));
        let start = Instant::now();
        let per = n / c.max(1);
        let mut tasks = vec![];
        for w in 0..c.max(1) {
            let (e, h) = (e.clone(), h.clone());
            let intents: Vec<TradeIntent> = (0..per).map(|i| mk(w * per + i, w)).collect();
            let wallet = wallets[w];
            tasks.push(tokio::spawn(async move {
                for it in intents {
                    let o = e.submit(it, wallet, Some(LatencyTrace::start())).await.unwrap();
                    if let Some(t) = e.take_trace(o.id) {
                        if let Some(us) = t.between_us(Stage::IntentAccepted, Stage::Signed) {
                            h.lock().record(us.max(1)).unwrap();
                        }
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let wall = start.elapsed();
        let unresolved = store.list_unresolved().await.unwrap();
        let submitted = unresolved.iter().filter(|o| o.state == OrderState::Submitted).count();
        if submitted == 0 {
            let any = store.list_orders(Uuid::nil(), 1).await.unwrap();
            println!("WARNING: no order submitted; sample: {:?} {:?}", any.first().map(|o| o.state), any.first().and_then(|o| o.reject_reason.clone()));
        }
        let r = json!({"concurrency": c, "orders": per * c.max(1), "submitted": submitted, "wall_ms": wall.as_millis(), "intent_to_signed_us": summary(&h.lock(), "us")});
        println!("{r}");
        results.push(r);
    }
    let v = json!({"kind": "engine_warm_path", "env": env_info(), "note": "Real Engine with MemStore, cached Pump state (fixture), cached balances, ManagedLocal signer, MOCK sender. Excludes RPC, Postgres, KMS.", "levels": results});
    save("engine", &v);
}

/// End-to-end on the MOCK chain at fixed intent rates (submit -> FINALIZED).
async fn load(a: &HashMap<String, String>) {
    let rates: Vec<u64> = a.get("rates").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![100, 500, 1000]);
    let secs: u64 = a.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut out = vec![];
    for rate in rates {
        let h = Harness::new(Opts { fund_lamports: 1_000_000_000_000, workers: true, ..Default::default() }).await;
        h.policies.0.lock().max_trades_per_day = u32::MAX;
        let total = rate * secs / 60;
        let gap = Duration::from_micros(60_000_000 / rate);
        let mut ids = vec![];
        let start = Instant::now();
        let mut ticker = tokio::time::interval(gap);
        for i in 0..total {
            ticker.tick().await;
            let o = h.engine.submit(h.buy(1_000_000, &format!("load-{rate}-{i}")), h.wallet, Some(LatencyTrace::start())).await.unwrap();
            ids.push((o.id, Instant::now()));
        }
        let mut e2e = hist();
        let mut states: HashMap<String, u64> = HashMap::new();
        for (id, t0) in &ids {
            let o = h.wait_terminal(*id, Duration::from_secs(60)).await;
            *states.entry(o.state.as_str().into()).or_default() += 1;
            let _ = t0;
        }
        for (_, t, _) in h.observer.traces.lock().iter() {
            if let Some(us) = t.between_us(Stage::IntentAccepted, Stage::Finalized) {
                e2e.record(us.max(1)).unwrap();
            }
        }
        let landed = h.chain.landed_ok().len();
        let r = json!({"rate_per_min": rate, "seconds": secs, "orders": total, "states": states, "landed_ok": landed,
            "duplicates": (landed as i64) - (*states.get("FINALIZED").unwrap_or(&0) as i64),
            "intent_to_finalized_us_mock_chain": summary(&e2e, "us"), "wall_ms": start.elapsed().as_millis()});
        println!("{r}");
        out.push(r);
    }
    let v = json!({"kind": "mock_chain_load", "env": env_info(), "note": "MOCK chain (20 ms slots, finalize after 4 slots). Measures pipeline+observer behavior under load, not real network landing.", "runs": out});
    save("load", &v);
}

/// Failure injection at given percentages: timeouts/ambiguous/drops/429.
async fn faults(a: &HashMap<String, String>) {
    let rates: Vec<u64> = a.get("rates").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![1, 5, 10]);
    let orders: u64 = a.get("orders").and_then(|s| s.parse().ok()).unwrap_or(100);
    let mut out = vec![];
    for pct in rates {
        let h = Harness::new(Opts { fund_lamports: 1_000_000_000_000, sender_timeout: Duration::from_millis(150), ..Default::default() }).await;
        let mut rng: u64 = 0xDEADBEEF ^ pct;
        let mut injected: HashMap<&str, u64> = HashMap::new();
        let mut ids = vec![];
        for i in 0..orders {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            if rng % 100 < pct {
                let (f, name) = match (rng / 100) % 4 {
                    0 => (Fault::DelayAck(Duration::from_millis(400)), "timeout_after_forward"),
                    1 => (Fault::Http500Forwarded, "http500_forwarded"),
                    2 => (Fault::Drop, "dropped"),
                    _ => (Fault::RateLimit, "rate_limited"),
                };
                h.chain.push_fault(f);
                *injected.entry(name).or_default() += 1;
            }
            let o = h.engine.submit(h.buy(1_000_000, &format!("f-{pct}-{i}")), h.wallet, None).await.unwrap();
            // duplicate delivery of every request
            let d = h.engine.submit(h.buy(1_000_000, &format!("f-{pct}-{i}")), h.wallet, None).await.unwrap();
            assert_eq!(o.id, d.id);
            ids.push(o.id);
        }
        let mut states: HashMap<String, u64> = HashMap::new();
        for id in &ids {
            let o = h.wait_terminal(*id, Duration::from_secs(60)).await;
            *states.entry(o.state.as_str().into()).or_default() += 1;
        }
        let landed = h.chain.landed_ok().len() as u64;
        let fin = *states.get("FINALIZED").unwrap_or(&0);
        let r = json!({"fault_pct": pct, "orders": orders, "injected": injected, "states": states, "landed_ok": landed, "duplicate_executions": landed as i64 - fin as i64});
        println!("{r}");
        out.push(r);
    }
    let v = json!({"kind": "fault_injection", "env": env_info(), "note": "MOCK chain; each request delivered twice (duplicate webhook)", "runs": out});
    save("faults", &v);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let (cmd, a) = args();
    match cmd.as_str() {
        "hotpath" => hotpath(&a).await,
        "engine" => engine(&a).await,
        "load" => load(&a).await,
        "faults" => faults(&a).await,
        _ => println!("usage: trader-bench hotpath|engine|load|faults [--flags]  (see source header)"),
    }
}
