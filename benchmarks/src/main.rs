//! trader-bench: reproducible benchmarks. Every number printed here is
//! measured in this process; nothing is estimated. See docs/BENCHMARKS.md.
//!
//!   trader-bench hotpath  [--iterations N] [--fixture fixtures/pump/localnet_curve.json]
//!   trader-bench engine   [--iterations N] [--concurrency 1,10,50,100]
//!   trader-bench load     [--rates 100,500,1000] [--seconds 20]
//!   trader-bench faults   [--rates 1,5,10] [--orders 100]
//!   trader-bench live     --mint <pump mint> [--rpc URL] [--ws URL] [--no-ws] [--iterations 100] [--gap-ms 500] [--wallet <pubkey>]
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
use trader_domain::{
    AmountSpec, Bps, ComputeBudget, Lamports, LatencyTrace, Network, OrderState, SenderKind, Side,
    Stage, TradeIntent, WalletPolicy,
};
use trader_execution::protocol::{ProtocolAdapter, QuoteRequest};
use trader_execution::{
    build_and_sign, BlockhashEntry, BlockhashService, BuildParams, GuardConfig, TxGuard,
    WalletLocks,
};
use trader_orders::{
    BalanceSource, Engine, EngineConfig, MemStore, ObserverHandle, OrderStore, PolicySource,
    SignerProvider, WalletBalances,
};
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
    let p = format!(
        "{dir}/{kind}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    println!("saved {p}");
}

fn load_fixture(path: &str) -> (Pubkey, trader_protocol_pump::CurveSnapshot) {
    let v: Value =
        serde_json::from_slice(&std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}")))
            .unwrap();
    let acct = |k: &str| {
        let a = &v["accounts"][k]["account"];
        (
            Pubkey::from_str(a["owner"].as_str().unwrap()).unwrap(),
            base64::engine::general_purpose::STANDARD
                .decode(a["data"][0].as_str().unwrap())
                .unwrap(),
        )
    };
    let mint = Pubkey::from_str(v["mint"].as_str().unwrap()).unwrap();
    let (g, f, c, m) = (
        acct("global"),
        acct("fee_config"),
        acct("bonding_curve"),
        acct("mint"),
    );
    let ata: u64 = v["curve_base_ata_amount"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    (
        mint,
        PumpAdapter::decode_snapshot(
            &mint,
            (&g.0, &g.1),
            (&f.0, &f.1),
            (&c.0, &c.1),
            (&m.0, &m.1),
            ata,
            0,
        )
        .unwrap(),
    )
}

/// Offline: quote -> build_swap -> compile+guard+sign+serialize. No I/O at all.
async fn hotpath(a: &HashMap<String, String>) {
    let n: usize = a
        .get("iterations")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let fixture = a.get("fixture").cloned().unwrap_or_else(|| {
        format!(
            "{}/../fixtures/pump/localnet_curve.json",
            env!("CARGO_MANIFEST_DIR")
        )
    });
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
    let gcfg = GuardConfig {
        max_priority_fee: Lamports(1_000_000),
        max_tip: Lamports(0),
        tip_accounts: vec![],
    };
    let (mut hq, mut hb, mut hs, mut ht) = (hist(), hist(), hist(), hist());
    let mut sizes = vec![];
    for i in 0..(n + 1000) {
        let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
        let amount = if side == Side::Buy {
            10_000_000 + (i as u64 % 1000)
        } else {
            1_000_000_000 + i as u64
        };
        let r = QuoteRequest {
            mint,
            side,
            amount_in: amount,
            slippage_bps: 300,
            max_state_age: Duration::from_secs(3600),
        };
        let t0 = Instant::now();
        let q = adapter.quote_exact_in(&r).unwrap();
        let t1 = Instant::now();
        let swap = adapter.build_swap(&q, &user).unwrap();
        let t2 = Instant::now();
        let p = BuildParams {
            network: Network::Localnet,
            payer: user,
            instructions: swap.instructions,
            compute: ComputeBudget {
                unit_limit: 150_000,
                unit_price_micro_lamports: 1_000,
            },
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
    async fn balances(
        &self,
        _: &Pubkey,
        _: &Pubkey,
        extra: &[Pubkey],
    ) -> Result<WalletBalances, String> {
        Ok(WalletBalances {
            sol: Lamports(1_000_000_000_000),
            token_raw: Some(0),
            token_account_exists: true,
            slot: 1,
            extra_exists: vec![true; extra.len()],
        })
    }
}

/// Engine warm path: IntentAccepted -> Signed through the real Engine
/// (MemStore writes, policy, quote, guard, sign) with cached state/balances
/// and a MOCK sender. Excludes network and Postgres.
async fn engine(a: &HashMap<String, String>) {
    let n: usize = a
        .get("iterations")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);
    let levels: Vec<usize> = a
        .get("concurrency")
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or(vec![1, 10, 50, 100]);
    let fixture = format!(
        "{}/../fixtures/pump/localnet_curve.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut results = vec![];
    for c in levels {
        let (mint, snap) = load_fixture(&fixture);
        let adapter = Arc::new(PumpAdapter::offline());
        adapter.insert_snapshot(mint, snap);
        let pg = match a.get("database-url") {
            Some(url) => Some(Arc::new(
                trader_api::pg::PgStore::connect(url, Network::Localnet)
                    .await
                    .expect("postgres"),
            )),
            None => None,
        };
        let store: Arc<dyn OrderStore> = match &pg {
            Some(p) => p.clone(),
            None => Arc::new(MemStore::new()),
        };
        let bh = BlockhashService::new();
        bh.set(BlockhashEntry {
            hash: Hash::new_from_array([3; 32]),
            last_valid_block_height: u64::MAX / 2,
            context_slot: 1,
            fetched_at: Instant::now(),
        });
        // Keep the cached blockhash fresh like the real BlockhashService loop would.
        let bh_refresh = {
            let bh = bh.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    bh.set(BlockhashEntry {
                        hash: Hash::new_from_array([3; 32]),
                        last_valid_block_height: u64::MAX / 2,
                        context_slot: 1,
                        fetched_at: Instant::now(),
                    });
                }
            })
        };
        let mut router = Router::new();
        router.add(ManagedSender::new(
            Arc::new(MockSender::new(SenderKind::Mock, MockBehavior::Accept)),
            Duration::from_secs(1),
            None,
        ));
        let (tx, _rx) = tokio::sync::broadcast::channel(65_536);
        // Observer handle without a running observer: nothing polls RPC here.
        let observer = trader_orders::Observer::new(
            store.clone(),
            trader_market_data::RpcClient::new(trader_market_data::RpcConfig::new(
                "http://127.0.0.1:9",
            ))
            .unwrap(),
            WalletLocks::default(),
            tx.clone(),
            Default::default(),
        );
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
        // Postgres needs real user/wallet rows (foreign keys).
        let mut user_id = Uuid::nil();
        if let Some(p) = &pg {
            use trader_api::appstore::{AppStore, Wallet};
            let u = p
                .upsert_telegram_user(
                    (Uuid::new_v4().as_u128() % 1_000_000_000_000) as i64,
                    Some("bench".into()),
                    true,
                )
                .await
                .unwrap();
            user_id = u.id;
            for (i, wid) in wallet_ids.iter().enumerate() {
                p.create_wallet(
                    &Wallet {
                        id: *wid,
                        user_id,
                        network: Network::Localnet,
                        pubkey: wallets[i].to_string(),
                        signer_kind: trader_wallets::SignerKind::ManagedLocal,
                        label: "bench".into(),
                        created_at: chrono::Utc::now(),
                    },
                    None,
                )
                .await
                .unwrap();
            }
        }
        let ms = MultiSigner(
            wallet_ids
                .iter()
                .enumerate()
                .map(|(i, w)| (*w, signers[&i].clone()))
                .collect(),
        );
        let e = Engine::new(
            EngineConfig {
                network: Network::Localnet,
                max_state_age: Duration::from_secs(3600),
                ..Default::default()
            },
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
        let start_tag = Uuid::new_v4().simple().to_string();
        let mk = |i: usize, w: usize| TradeIntent {
            id: Uuid::new_v4(),
            user_id,
            wallet_id: wallet_ids[w],
            network: Network::Localnet,
            side: Side::Buy,
            mint,
            amount: AmountSpec::ExactSolIn {
                lamports: Lamports(10_000_000 + i as u64),
            },
            slippage: Bps::new(300).unwrap(),
            fees: FeePreference {
                compute_unit_price_micro_lamports: 1_000,
                max_priority_fee: Lamports(100_000),
                tip: Lamports(0),
            },
            senders: vec![SenderKind::Mock],
            idempotency_key: format!("bench-{c}-{i}-{}", start_tag),
            requires_approval: false,
            created_at: chrono::Utc::now(),
        };
        // h: accepted->signed; hs: accepted->send start (includes the durable
        // SIGNED write); hc: create (idempotent insert) ; ht: whole request
        // (submit() start -> send start).
        let h = Arc::new(Mutex::new(hist()));
        let hs = Arc::new(Mutex::new(hist()));
        let hc = Arc::new(Mutex::new(hist()));
        let ht = Arc::new(Mutex::new(hist()));
        let start = Instant::now();
        let per = n / c.max(1);
        let mut tasks = vec![];
        for (w, wallet) in wallets.iter().copied().enumerate() {
            let (e, h, hs, hc, ht) = (e.clone(), h.clone(), hs.clone(), hc.clone(), ht.clone());
            let intents: Vec<TradeIntent> = (0..per).map(|i| mk(w * per + i, w)).collect();
            tasks.push(tokio::spawn(async move {
                for it in intents {
                    let o = e
                        .submit(it, wallet, Some(LatencyTrace::start()))
                        .await
                        .unwrap();
                    if let Some(t) = e.take_trace(o.id) {
                        if let Some(us) = t.between_us(Stage::IntentAccepted, Stage::Signed) {
                            h.lock().record(us.max(1)).unwrap();
                        }
                        if let Some(us) = t.between_us(Stage::IntentAccepted, Stage::SendStart) {
                            hs.lock().record(us.max(1)).unwrap();
                        }
                        if let Some(us) = t.us(Stage::IntentAccepted) {
                            hc.lock().record(us.max(1)).unwrap();
                        }
                        if let Some(us) = t.us(Stage::SendStart) {
                            ht.lock().record(us.max(1)).unwrap();
                        }
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let wall = start.elapsed();
        bh_refresh.abort();
        let unresolved = store.list_unresolved().await.unwrap();
        let submitted = unresolved
            .iter()
            .filter(|o| o.state == OrderState::Submitted && o.user_id == user_id)
            .count();
        if submitted == 0 {
            let any = store.list_orders(user_id, 1).await.unwrap();
            println!(
                "WARNING: no order submitted; sample: {:?} {:?}",
                any.first().map(|o| o.state),
                any.first().and_then(|o| o.reject_reason.clone())
            );
        }
        let r = json!({"concurrency": c, "orders": per * c.max(1), "submitted": submitted, "wall_ms": wall.as_millis(),
            "intent_to_signed_us": summary(&h.lock(), "us"),
            "intent_to_send_start_us": summary(&hs.lock(), "us"),
            "create_order_us": summary(&hc.lock(), "us"),
            "request_to_send_start_us": summary(&ht.lock(), "us")});
        println!("{r}");
        results.push(r);
    }
    let async_presign = std::env::var("DATABASE_ASYNC_PRESIGN_COMMITS").as_deref() != Ok("false");
    let store_kind = match (a.contains_key("database-url"), async_presign) {
        (false, _) => "MemStore (in-memory)".to_string(),
        (true, true) => format!(
            "PostgreSQL: pre-sign writes synchronous_commit=off, SIGNED and later durable (fsync); DATABASE_MAX_CONNECTIONS={} DATABASE_PRESIGN_MAX_CONNECTIONS={}",
            std::env::var("DATABASE_MAX_CONNECTIONS").unwrap_or_else(|_| "default".into()),
            std::env::var("DATABASE_PRESIGN_MAX_CONNECTIONS").unwrap_or_else(|_| "default".into())
        ),
        (true, false) => "PostgreSQL: every order write durable (synchronous_commit=on)".to_string(),
    };
    let v = json!({"kind": "engine_warm_path", "store": store_kind, "env": env_info(), "note": "Real Engine, cached Pump state (fixture), cached balances, ManagedLocal signer, MOCK sender. Excludes RPC and KMS.", "levels": results});
    save(
        if a.contains_key("database-url") {
            "engine-pg"
        } else {
            "engine"
        },
        &v,
    );
}

/// End-to-end on the MOCK chain at fixed intent rates (submit -> FINALIZED).
async fn load(a: &HashMap<String, String>) {
    let rates: Vec<u64> = a
        .get("rates")
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or(vec![100, 500, 1000]);
    let secs: u64 = a.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut out = vec![];
    for rate in rates {
        let h = Harness::new(Opts {
            fund_lamports: 1_000_000_000_000,
            workers: true,
            ..Default::default()
        })
        .await;
        h.policies.0.lock().max_trades_per_day = u32::MAX;
        let total = rate * secs / 60;
        let gap = Duration::from_micros(60_000_000 / rate);
        let mut ids = vec![];
        let start = Instant::now();
        let mut ticker = tokio::time::interval(gap);
        for i in 0..total {
            ticker.tick().await;
            let o = h
                .engine
                .submit(
                    h.buy(1_000_000, &format!("load-{rate}-{i}")),
                    h.wallet,
                    Some(LatencyTrace::start()),
                )
                .await
                .unwrap();
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
    let rates: Vec<u64> = a
        .get("rates")
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or(vec![1, 5, 10]);
    let orders: u64 = a.get("orders").and_then(|s| s.parse().ok()).unwrap_or(100);
    let mut out = vec![];
    for pct in rates {
        let h = Harness::new(Opts {
            fund_lamports: 1_000_000_000_000,
            sender_timeout: Duration::from_millis(150),
            ..Default::default()
        })
        .await;
        let mut rng: u64 = 0xDEADBEEF ^ pct;
        let mut injected: HashMap<&str, u64> = HashMap::new();
        let mut ids = vec![];
        for i in 0..orders {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            if rng % 100 < pct {
                let (f, name) = match (rng / 100) % 4 {
                    0 => (
                        Fault::DelayAck(Duration::from_millis(400)),
                        "timeout_after_forward",
                    ),
                    1 => (Fault::Http500Forwarded, "http500_forwarded"),
                    2 => (Fault::Drop, "dropped"),
                    _ => (Fault::RateLimit, "rate_limited"),
                };
                h.chain.push_fault(f);
                *injected.entry(name).or_default() += 1;
            }
            let o = h
                .engine
                .submit(h.buy(1_000_000, &format!("f-{pct}-{i}")), h.wallet, None)
                .await
                .unwrap();
            // duplicate delivery of every request
            let d = h
                .engine
                .submit(h.buy(1_000_000, &format!("f-{pct}-{i}")), h.wallet, None)
                .await
                .unwrap();
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

struct LivePol;
#[async_trait]
impl PolicySource for LivePol {
    async fn policy(&self, _: Uuid) -> Result<WalletPolicy, String> {
        Ok(WalletPolicy {
            max_order: Lamports(10_000_000_000),
            max_daily_turnover: Lamports(u64::MAX / 2),
            max_trades_per_day: u32::MAX,
            mainnet_enabled: true,
            allowed_senders: vec![SenderKind::Mock],
            ..Default::default()
        })
    }
}

struct OneSigner(Arc<dyn TxSigner>);
#[async_trait]
impl SignerProvider for OneSigner {
    async fn signer_for(&self, _: Uuid) -> Result<Arc<dyn TxSigner>, String> {
        Ok(self.0.clone())
    }
}

/// LIVE, read-only: real cluster Pump state (default mainnet public RPC),
/// optionally streamed over WebSocket; real ed25519 signing with a throwaway
/// key; MOCK sender (nothing is broadcast) and cached balances (no funds).
/// `--no-ws` uses the RPC path with a one-slot state age so each order refetches.
/// `--wallet <pubkey>` also times a balance read for that public address
/// (RPC vs stream).
async fn live(a: &HashMap<String, String>) {
    use trader_market_data::{ws_url_from_http, AccountStream, RpcClient, RpcConfig};
    let rpc_url = a
        .get("rpc")
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());
    let mint = Pubkey::from_str(a.get("mint").expect("--mint required")).unwrap();
    let n: usize = a
        .get("iterations")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let gap = Duration::from_millis(a.get("gap-ms").and_then(|s| s.parse().ok()).unwrap_or(500));
    let use_ws = !a.contains_key("no-ws");
    let mut cfg = RpcConfig::new(rpc_url.clone());
    cfg.timeout = Duration::from_secs(10);
    let rpc = RpcClient::new(cfg).unwrap();
    let mut adapter = PumpAdapter::new(rpc.clone());
    let stream = if use_ws {
        let url = a
            .get("ws")
            .cloned()
            .unwrap_or_else(|| ws_url_from_http(&rpc_url));
        let s = AccountStream::spawn(url);
        adapter = adapter.with_stream(s.clone());
        Some(s)
    } else {
        None
    };
    let adapter = Arc::new(adapter);
    let mut warm_ms = None;
    if let Some(s) = &stream {
        let t = Instant::now();
        while !s.is_live(Duration::from_millis(1500)) && t.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            s.is_live(Duration::from_millis(1500)),
            "ws stream not live (no slot heartbeat)"
        );
        let t = Instant::now();
        let ok = adapter.prewarm(&mint).await.expect("prewarm");
        assert!(ok, "subscription not acknowledged");
        warm_ms = Some(t.elapsed().as_secs_f64() * 1e3);
        assert!(
            adapter.snapshot_from_stream(&mint).is_some(),
            "stream snapshot incomplete"
        );
    }
    let bh = BlockhashService::new();
    bh.refresh_once(&rpc).await.unwrap();
    bh.spawn(rpc.clone(), Duration::from_secs(2));
    let mut router = Router::new();
    router.add(ManagedSender::new(
        Arc::new(MockSender::new(SenderKind::Mock, MockBehavior::Accept)),
        Duration::from_secs(1),
        None,
    ));
    let store: Arc<dyn OrderStore> = Arc::new(MemStore::new());
    let store_view = store.clone();
    let (tx, _rx) = tokio::sync::broadcast::channel(65_536);
    let observer = trader_orders::Observer::new(
        store.clone(),
        RpcClient::new(RpcConfig::new("http://127.0.0.1:9")).unwrap(),
        WalletLocks::default(),
        tx.clone(),
        Default::default(),
    );
    let kp = Keypair::new();
    let wallet = kp.pubkey();
    let e = Engine::new(
        EngineConfig {
            // Orders are labelled Localnet so every mainnet gate (e.g. "no mock
            // sender on mainnet") stays intact; market state still comes from the
            // live cluster. The only sender is the in-memory mock: nothing leaves.
            network: Network::Localnet,
            max_state_age: if use_ws {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(400)
            },
            ..Default::default()
        },
        store,
        vec![adapter.clone()],
        router,
        bh,
        WalletLocks::default(),
        Arc::new(OneSigner(Arc::new(ManagedLocalSigner::new(
            kp,
            Network::Localnet,
        )))),
        Arc::new(CachedBalances),
        Arc::new(LivePol),
        observer.handle.clone(),
        tx,
    );
    let tag = Uuid::new_v4().simple().to_string();
    let (mut h_signed, mut h_req) = (hist(), hist());
    let mut stream_hits = 0;
    let mut failures: Vec<String> = vec![];
    for i in 0..n {
        if adapter.snapshot_from_stream(&mint).is_some() {
            stream_hits += 1;
        }
        let it = TradeIntent {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_id: Uuid::new_v4(),
            network: Network::Localnet,
            side: Side::Buy,
            mint,
            amount: AmountSpec::ExactSolIn {
                lamports: Lamports(120_000),
            },
            slippage: Bps::new(500).unwrap(),
            fees: FeePreference {
                compute_unit_price_micro_lamports: 20_000,
                max_priority_fee: Lamports(50_000),
                tip: Lamports(0),
            },
            senders: vec![SenderKind::Mock],
            idempotency_key: format!("live-{tag}-{i}"),
            requires_approval: false,
            created_at: chrono::Utc::now(),
        };
        match e.submit(it, wallet, Some(LatencyTrace::start())).await {
            Ok(o) => {
                if let Some(t) = e.take_trace(o.id) {
                    if let Some(us) = t.between_us(Stage::IntentAccepted, Stage::Signed) {
                        h_signed.record(us.max(1)).unwrap();
                    }
                    match t.us(Stage::SendStart) {
                        Some(us) => h_req.record(us.max(1)).unwrap(),
                        None if failures.len() < 3 => {
                            let cur = store_view.get_order(o.id).await.ok();
                            failures.push(format!(
                                "{:?} {:?}",
                                cur.as_ref().map(|c| c.state),
                                cur.and_then(|c| c.reject_reason)
                            ))
                        }
                        None => {}
                    }
                }
            }
            Err(err) if failures.len() < 3 => failures.push(err.to_string()),
            Err(_) => {}
        }
        tokio::time::sleep(gap).await;
    }
    // Balance read for a public address: RPC round trip vs stream (no I/O).
    let mut bal = json!(null);
    if let Some(w) = a.get("wallet").map(|s| Pubkey::from_str(s).unwrap()) {
        let rb = trader_orders::balances::RpcBalances { rpc: rpc.clone() };
        let mut h_rpc = hist();
        for _ in 0..20 {
            let t = Instant::now();
            rb.balances(&w, &mint, &[]).await.unwrap();
            h_rpc
                .record((t.elapsed().as_micros() as u64).max(1))
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut ws_summary = json!(null);
        if let Some(s) = &stream {
            let sb = trader_orders::balances::StreamBalances::new(rpc.clone(), s.clone());
            assert!(
                sb.prewarm(&w, &mint, &[]).await,
                "balance stream warm failed"
            );
            let mut h_ws = hist();
            for _ in 0..200 {
                let t = Instant::now();
                sb.from_stream(&w, &mint, &[])
                    .expect("stream balances")
                    .unwrap();
                h_ws.record((t.elapsed().as_nanos() as u64 / 1000).max(1))
                    .unwrap();
            }
            ws_summary = summary(&h_ws, "us");
        }
        bal = json!({"rpc_balances_us": summary(&h_rpc, "us"), "stream_balances_us": ws_summary});
    }
    let v = json!({
        "kind": if use_ws { "live_ws" } else { "live_rpc" },
        "rpc_host": rpc.host(),
        "mint": mint.to_string(),
        "env": env_info(),
        "note": "LIVE cluster Pump state; real signing with a throwaway key; MOCK sender (nothing broadcast); cached balances",
        "iterations": n,
        "gap_ms": gap.as_millis() as u64,
        "stream_warmup_ms": warm_ms,
        "stream_hits_before_submit": stream_hits,
        "ws_reconnects": stream.as_ref().map(|s| s.reconnects()),
        "intent_to_signed_us": summary(&h_signed, "us"),
        "request_to_send_start_us": summary(&h_req, "us"),
        "balances": bal,
        "sample_failures": failures,
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
    save(if use_ws { "live-ws" } else { "live-rpc" }, &v);
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let (cmd, a) = args();
    match cmd.as_str() {
        "hotpath" => hotpath(&a).await,
        "engine" => engine(&a).await,
        "load" => load(&a).await,
        "faults" => faults(&a).await,
        "live" => live(&a).await,
        _ => println!(
            "usage: trader-bench hotpath|engine|load|faults|live [--flags]  (see source header)"
        ),
    }
}
