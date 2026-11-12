//! Guarded MAINNET canary for the official Pump bonding-curve program.
//!
//! Default is a DRY RUN: the full engine path runs against live mainnet state
//! (quote, policy, balance checks, pre-sign guard, sign), but the only
//! "sender" zeroes the signature, calls `simulateTransaction` with
//! `sigVerify=false`, and refuses to forward. No broadcastable bytes leave
//! the process and no funds move.
//!
//! `--send` broadcasts real transactions. It additionally requires
//! `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK` and the operator
//! typing a confirmation phrase on an interactive terminal. Hard caps:
//! `MAX_BUY_LAMPORTS` per buy, `MAX_ITERATIONS` round trips per run.

use crate::smoke::{FixedPolicy, OneSigner};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use trader_domain::fees::{
    rent_exempt_minimum, BASE_FEE_LAMPORTS_PER_SIGNATURE, TOKEN_2022_ATA_LEN, TOKEN_ACCOUNT_LEN,
};
use trader_domain::intent::FeePreference;
use trader_domain::{
    AmountSpec, Bps, Commitment, Lamports, LatencyTrace, Network, OrderState, SenderKind, Side,
    Stage, TradeIntent, WalletPolicy,
};
use trader_execution::{BlockhashService, WalletLocks};
use trader_market_data::token::{
    associated_token_address, parse_token_account, TOKEN_2022_PROGRAM,
};
use trader_market_data::{ws_url_from_http, AccountStream, RpcClient, RpcConfig};
use trader_orders::balances::{RpcBalances, StreamBalances};
use trader_orders::BalanceSource;
use trader_orders::{Engine, EngineConfig, MemStore, Observer, ObserverConfig, Order, OrderStore};
use trader_protocol_pump::PumpAdapter;
use trader_router::{
    ManagedSender, Router, SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender,
};
use trader_sender_rpc::RpcSender;
use trader_wallets::{ManagedLocalSigner, TxSigner};
use uuid::Uuid;

/// 0.001 SOL. Hard ceiling per buy, whatever the flags say.
pub const MAX_BUY_LAMPORTS: u64 = 1_000_000;
/// ~US$0.01 at ~US$83/SOL (2026-11-12).
pub const DEFAULT_BUY_LAMPORTS: u64 = 120_000;
pub const MAX_ITERATIONS: usize = 10;
/// Priority fee ceiling (micro-lamports per CU).
pub const MAX_CU_PRICE: u64 = 200_000;
/// Tip ceiling per transaction (0.00005 SOL).
pub const MAX_TIP_LAMPORTS: u64 = 50_000;
pub const JITO_FRANKFURT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf";
/// Helius Sender, Frankfurt, SWQoS-only tier (regional endpoints are plain HTTP).
pub const HELIUS_FRANKFURT: &str = "http://fra-sender.helius-rpc.com/fast?swqos_only=true";
const MAINNET_ACK: &str = "I_UNDERSTAND_REAL_FUNDS_AT_RISK";
/// Estimate only (the engine sets the real limit from the adapter).
const EST_CU_LIMIT: u64 = 150_000;
/// System-account rent-exempt minimum (0 data bytes); the fee payer must stay above it.
const WALLET_RENT_FLOOR: u64 = 890_880;

pub fn default_keypair_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".trader")
        .join("canary-keypair.json")
}

fn keypair_path(a: &HashMap<String, String>) -> PathBuf {
    a.get("keypair")
        .cloned()
        .or_else(|| {
            std::env::var("CANARY_KEYPAIR")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .map(PathBuf::from)
        .unwrap_or_else(default_keypair_path)
}

/// Create a fresh canary keypair file (Solana CLI JSON format). Never
/// overwrites. Prints only the public key.
pub async fn keygen(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let path = keypair_path(a);
    if path.exists() {
        bail!("{} already exists; refusing to overwrite", path.display());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let kp = Keypair::new();
    let body = serde_json::to_string(&kp.to_bytes().to_vec())?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(body.as_bytes())?;
    println!(
        "{}",
        json!({
            "pubkey": kp.pubkey().to_string(),
            "keypair_file": path.display().to_string(),
            "note": "The secret key is only in this file. Never paste it anywhere. Send a small amount of SOL to the pubkey."
        })
    );
    Ok(())
}

fn load_keypair(path: &PathBuf) -> anyhow::Result<Keypair> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("reading keypair file {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&s)
        .context("keypair file must be a Solana CLI JSON array of 64 numbers")?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("invalid keypair file: {e}"))
}

/// Dry-run "sender": never forwards. Zeroes every signature before calling
/// `simulateTransaction` with `sigVerify=false`.
struct SimulateOnly {
    rpc: RpcClient,
    out: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Mirrors the tip policy of the real senders so the simulated bytes match.
    tip: TipPolicy,
}

/// Zero all signatures of a serialized transaction in place. Returns false if
/// the layout is not the expected `compact-u16 count (< 128) + 64-byte sigs`.
fn zero_signatures(wire: &mut [u8]) -> bool {
    let n = match wire.first() {
        Some(&n) if n > 0 && n < 0x80 => n as usize,
        _ => return false,
    };
    if wire.len() < 1 + 64 * n {
        return false;
    }
    wire[1..1 + 64 * n].iter_mut().for_each(|b| *b = 0);
    true
}

#[async_trait]
impl TransactionSender for SimulateOnly {
    fn kind(&self) -> SenderKind {
        SenderKind::Rpc
    }
    fn endpoint(&self) -> String {
        format!("simulate-only:{}", self.rpc.host())
    }
    fn tip_policy(&self) -> TipPolicy {
        self.tip.clone()
    }
    async fn submit(&self, signed_tx: &[u8], ctx: &SendContext) -> SendOutcome {
        let mut wire = signed_tx.to_vec();
        if !zero_signatures(&mut wire) {
            return SendOutcome::Rejected {
                reason: "dry-run: unexpected wire layout; not simulated, not forwarded".into(),
            };
        }
        let (row, reason) = match self
            .rpc
            .simulate_transaction_opts(&wire, Commitment::Processed, false)
            .await
        {
            Ok(v) => {
                let val = &v["value"];
                let err = val["err"].clone();
                let units = val["unitsConsumed"].clone();
                (
                    json!({"order_id": ctx.order_id, "err": err, "units_consumed": units, "logs": val["logs"]}),
                    format!("dry-run: simulated (err={err}, units={units}); not forwarded"),
                )
            }
            Err(e) => (
                json!({"order_id": ctx.order_id, "rpc_error": e.to_string()}),
                format!("dry-run: simulateTransaction failed ({e}); not forwarded"),
            ),
        };
        self.out.lock().push(row);
        SendOutcome::Rejected { reason }
    }
    async fn health(&self) -> SenderHealth {
        SenderHealth {
            configured: true,
            note: Some("simulate-only (dry run)".into()),
            ..Default::default()
        }
    }
}

async fn token_balance(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    program: &Pubkey,
) -> anyhow::Result<u64> {
    let ata = associated_token_address(owner, mint, program);
    let (_, acc) = rpc.get_account(&ata, Commitment::Confirmed).await?;
    Ok(acc
        .and_then(|a| parse_token_account(&a.owner, &a.data).ok())
        .map(|t| t.amount)
        .unwrap_or(0))
}

async fn exists(rpc: &RpcClient, key: &Pubkey) -> anyhow::Result<bool> {
    Ok(rpc
        .get_account(key, Commitment::Confirmed)
        .await?
        .1
        .is_some())
}

async fn wait_terminal(store: &MemStore, id: Uuid, timeout: Duration) -> anyhow::Result<Order> {
    let start = std::time::Instant::now();
    loop {
        let o = store.get_order(id).await?;
        if o.state.is_terminal() && (!o.state.has_landed() || o.fill.is_some()) {
            return Ok(o);
        }
        if start.elapsed() > timeout {
            bail!(
                "order {id} still {} after {timeout:?}; check the signature on an explorer",
                o.state
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn parse<T: FromStr>(a: &HashMap<String, String>, k: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match a.get(k) {
        Some(v) => v.parse().map_err(|e| anyhow!("--{k} {v}: {e}")),
        None => Ok(default),
    }
}

pub async fn mainnet_canary(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let send = a.contains_key("send");
    // `--localnet-test` runs the identical code path against a LOCAL validator
    // (genesis must NOT be a public cluster), airdrops the wallet and skips the
    // mainnet confirmation. It can only loosen toward localnet, never mainnet.
    let net = if a.contains_key("localnet-test") {
        Network::Localnet
    } else {
        Network::Mainnet
    };
    let url = a
        .get("rpc")
        .cloned()
        .or_else(|| std::env::var("RPC_URL").ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| anyhow!("set RPC_URL to a mainnet RPC endpoint (or pass --rpc)"))?;
    let mut cfg = RpcConfig::new(url.clone());
    cfg.timeout = Duration::from_secs(20);
    let rpc = RpcClient::new(cfg)?;
    let genesis = rpc.get_genesis_hash().await?;
    net.verify_genesis(&genesis)
        .map_err(|e| anyhow!("refusing: {e}"))?;

    let mint = Pubkey::from_str(
        a.get("mint")
            .ok_or_else(|| anyhow!("--mint <Pump bonding-curve mint> required"))?,
    )?;
    let buy: u64 = parse(a, "buy-lamports", DEFAULT_BUY_LAMPORTS)?;
    if buy == 0 || buy > MAX_BUY_LAMPORTS {
        bail!("--buy-lamports must be 1..={MAX_BUY_LAMPORTS} (hard cap 0.001 SOL)");
    }
    let iterations: usize = parse(a, "iterations", 1)?;
    if iterations == 0 || iterations > MAX_ITERATIONS {
        bail!("--iterations must be 1..={MAX_ITERATIONS}");
    }
    let cu_price: u64 = parse(a, "cu-price", 20_000)?;
    if cu_price > MAX_CU_PRICE {
        bail!("--cu-price must be <= {MAX_CU_PRICE} micro-lamports/CU");
    }
    // 400 ms (one slot) keeps signature polling under public-RPC rate limits.
    let poll_ms: u64 = parse(a, "poll-ms", 400)?;
    if !(100..=5_000).contains(&poll_ms) {
        bail!("--poll-ms must be 100..=5000");
    }
    // Senders get the SAME signed bytes in parallel: rpc (sendTransaction),
    // tpu (QUIC straight to the next leaders), jito (block engine, tip required).
    let sender_names: Vec<String> = a
        .get("senders")
        .map(String::as_str)
        .unwrap_or("rpc")
        .split(',')
        .map(|x| x.trim().to_lowercase())
        .filter(|x| !x.is_empty())
        .collect();
    let mut kinds: Vec<SenderKind> = vec![];
    for n in &sender_names {
        let k = match n.as_str() {
            "rpc" => SenderKind::Rpc,
            "tpu" => SenderKind::Tpu,
            "jito" => SenderKind::Jito,
            "helius" => SenderKind::Helius,
            other => bail!("unknown sender {other} (use rpc,tpu,jito,helius)"),
        };
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    let use_jito = kinds.contains(&SenderKind::Jito);
    let use_helius = kinds.contains(&SenderKind::Helius);
    if use_jito && use_helius {
        bail!("pick one tipped sender per run: jito and helius use different tip accounts");
    }
    let tip: u64 = parse(
        a,
        "tip-lamports",
        if use_helius {
            trader_sender_rpc::HELIUS_SWQOS_MIN_TIP
        } else if use_jito {
            1_000
        } else {
            0
        },
    )?;
    if use_helius && tip < trader_sender_rpc::HELIUS_SWQOS_MIN_TIP {
        bail!(
            "helius (SWQoS-only) needs --tip-lamports >= {}",
            trader_sender_rpc::HELIUS_SWQOS_MIN_TIP
        );
    }
    let helius_url = a
        .get("helius-url")
        .cloned()
        .unwrap_or_else(|| HELIUS_FRANKFURT.to_string());
    if tip > MAX_TIP_LAMPORTS {
        bail!("--tip-lamports must be <= {MAX_TIP_LAMPORTS}");
    }
    if use_jito && tip < trader_sender_jito::JITO_MIN_TIP_LAMPORTS {
        bail!(
            "jito needs --tip-lamports >= {}",
            trader_sender_jito::JITO_MIN_TIP_LAMPORTS
        );
    }
    let jito_url = a
        .get("jito-url")
        .cloned()
        .unwrap_or_else(|| JITO_FRANKFURT.to_string());
    let slippage =
        Bps::new(parse(a, "slippage-bps", 500u32)?).map_err(|e| anyhow!("--slippage-bps: {e}"))?;

    let kp_path = keypair_path(a);
    let kp = load_keypair(&kp_path)?;
    let wallet = kp.pubkey();
    let mut sol = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
    let (_, mint_acc) = rpc.get_account(&mint, Commitment::Confirmed).await?;
    let token_program = mint_acc
        .ok_or_else(|| anyhow!("mint {mint} not found on {net}"))?
        .owner;
    let ata = associated_token_address(&wallet, &mint, &token_program);
    let uva = pump_rust_client::pda::pump::user_volume_accumulator(&wallet).0;
    let ata_rent = if token_program == TOKEN_2022_PROGRAM {
        rent_exempt_minimum(TOKEN_2022_ATA_LEN).0
    } else {
        rent_exempt_minimum(TOKEN_ACCOUNT_LEN).0
    };
    let uva_rent = rent_exempt_minimum(trader_protocol_pump::USER_VOLUME_ACCUMULATOR_LEN).0;
    let rent_new = if exists(&rpc, &ata).await? {
        0
    } else {
        ata_rent
    } + if exists(&rpc, &uva).await? {
        0
    } else {
        uva_rent
    };
    let fee_per_tx = BASE_FEE_LAMPORTS_PER_SIGNATURE
        + (cu_price as u128 * EST_CU_LIMIT as u128 / 1_000_000) as u64
        + tip;
    // Pump trade fees are well under 3% per side; 3% is a conservative ceiling.
    let est_loss_per_round_trip = 2 * fee_per_tx + buy * 6 / 100;
    let est_max_loss = iterations as u64 * est_loss_per_round_trip;
    let need = WALLET_RENT_FLOOR + 100_000 + rent_new + buy + 2 * fee_per_tx + est_max_loss;
    if net == Network::Localnet && sol < need {
        crate::localnet::airdrop(&rpc, &wallet, 2_000_000_000).await?;
        sol = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
    }
    let plan = json!({
        "network": net.as_str(),
        "mode": if send { "SEND (real transactions)" } else { "DRY RUN (simulate only, nothing broadcast)" },
        "rpc_host": rpc.host(),
        "wallet": wallet.to_string(),
        "wallet_sol_lamports": sol,
        "mint": mint.to_string(),
        "token_program": token_program.to_string(),
        "buy_lamports_per_iteration": buy,
        "iterations": iterations,
        "websocket": !a.contains_key("no-ws"),
        "cu_price_micro_lamports": cu_price,
        "senders": sender_names,
        "tip_lamports_per_tx": tip,
        "est_fee_per_tx_lamports": fee_per_tx,
        "rent_locked_if_new_lamports": rent_new,
        "rent_note": "token account + Pump user_volume_accumulator rent stays in those accounts (recoverable by closing them; not automated yet)",
        "est_max_loss_lamports_excl_rent": est_max_loss,
        "min_wallet_balance_needed_lamports": need,
    });
    println!("{}", serde_json::to_string_pretty(&plan)?);
    if sol < need {
        bail!(
            "wallet has {sol} lamports; needs at least {need} ({:.6} SOL). Fund {} and rerun.",
            need as f64 / 1e9,
            wallet
        );
    }

    if send && net == Network::Mainnet {
        if std::env::var("MAINNET_TRADING_ENABLED").as_deref() != Ok(MAINNET_ACK) {
            bail!("--send requires MAINNET_TRADING_ENABLED={MAINNET_ACK}");
        }
        if !std::io::stdin().is_terminal() {
            bail!("--send must be confirmed on an interactive terminal; nothing sent");
        }
        let phrase = format!("SEND {iterations} MAINNET");
        print!("Type exactly `{phrase}` to broadcast real transactions: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != phrase {
            bail!("confirmation did not match; nothing sent");
        }
    }

    let signer: Arc<dyn TxSigner> = Arc::new(ManagedLocalSigner::new(kp, net));
    let store = Arc::new(MemStore::new());
    let locks = WalletLocks::default();
    // WebSocket account stream (default on): coin state and wallet balances are
    // pushed by the RPC, so a warm order needs no read round trip before signing.
    let stream = if a.contains_key("no-ws") {
        None
    } else {
        Some(AccountStream::spawn(
            a.get("ws")
                .cloned()
                .unwrap_or_else(|| ws_url_from_http(&url)),
        ))
    };
    let mut adapter = PumpAdapter::new(rpc.clone());
    if let Some(s) = &stream {
        adapter = adapter.with_stream(s.clone());
    }
    let adapter = Arc::new(adapter);
    let stream_bal = stream
        .as_ref()
        .map(|s| Arc::new(StreamBalances::new(rpc.clone(), s.clone())));
    let balances: Arc<dyn BalanceSource> = match &stream_bal {
        Some(b) => b.clone(),
        None => Arc::new(RpcBalances { rpc: rpc.clone() }),
    };
    let mut ws_info = json!(null);
    if let (Some(s), Some(sb)) = (&stream, &stream_bal) {
        let t = Instant::now();
        while !s.is_live(Duration::from_millis(1500)) && t.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let live = s.is_live(Duration::from_millis(1500));
        let coin_warm = adapter.prewarm(&mint).await.unwrap_or(false);
        let balances_warm = sb.prewarm(&wallet, &mint, &[uva]).await;
        ws_info = json!({
            "host": s.host(),
            "live": live,
            "coin_warm": coin_warm,
            "balances_warm": balances_warm,
            "warmup_ms": t.elapsed().as_millis() as u64,
            "note": "if not warm the engine falls back to RPC reads",
        });
        println!("{}", json!({ "websocket": ws_info }));
    }
    let blockhash = BlockhashService::new();
    blockhash.refresh_once(&rpc).await?;
    blockhash.spawn(rpc.clone(), Duration::from_secs(2));
    let sims = Arc::new(Mutex::new(vec![]));
    let mut router = Router::new();
    let jito_tip_accounts = if use_jito {
        let j = trader_sender_jito::JitoSender::new(
            jito_url.clone(),
            false,
            None,
            Duration::from_secs(5),
        );
        let accts = j
            .fetch_tip_accounts()
            .await
            .map_err(|e| anyhow!("jito getTipAccounts: {e}"))?;
        if accts.is_empty() {
            bail!("jito returned no tip accounts");
        }
        Some(accts)
    } else {
        None
    };
    if send {
        for k in &kinds {
            match k {
                SenderKind::Rpc => router.add(ManagedSender::new(
                    Arc::new(RpcSender::standard(rpc.clone(), false)),
                    Duration::from_secs(10),
                    None,
                )),
                SenderKind::Tpu => {
                    let t = trader_sender_tpu::TpuSender::start(rpc.clone(), stream.clone(), 2)
                        .await
                        .map_err(|e| anyhow!("tpu sender: {e}"))?;
                    router.add(ManagedSender::new(t, Duration::from_secs(3), None));
                }
                SenderKind::Jito => {
                    let accts = jito_tip_accounts.clone().expect("jito set up above");
                    // Jito's default limit is 1 request/s per IP per region.
                    router.add(ManagedSender::new(
                        Arc::new(
                            trader_sender_jito::JitoSender::new(
                                jito_url.clone(),
                                false,
                                None,
                                Duration::from_secs(5),
                            )
                            .with_tip_accounts(accts),
                        ),
                        Duration::from_secs(5),
                        Some(trader_router::TokenBucket::new(1, Duration::from_secs(1))),
                    ));
                }
                SenderKind::Helius => {
                    let mut hc = RpcConfig::new(helius_url.clone());
                    hc.timeout = Duration::from_secs(5);
                    router.add(ManagedSender::new(
                        Arc::new(RpcSender::helius_sender(RpcClient::new(hc)?, true)),
                        Duration::from_secs(5),
                        Some(trader_router::TokenBucket::new(1, Duration::from_secs(1))),
                    ));
                }
                _ => unreachable!(),
            }
        }
    } else {
        // Dry run: one simulate-only sender carrying the same tip policy, so the
        // simulated transaction is byte-for-byte the shape that would be sent.
        let tip = match &jito_tip_accounts {
            Some(accts) => TipPolicy::Required {
                min_lamports: trader_sender_jito::JITO_MIN_TIP_LAMPORTS,
                accounts: accts.clone(),
            },
            None if use_helius => TipPolicy::Required {
                min_lamports: trader_sender_rpc::HELIUS_SWQOS_MIN_TIP,
                accounts: trader_sender_rpc::HELIUS_TIP_ACCOUNTS
                    .iter()
                    .map(|k| Pubkey::from_str(k).expect("valid tip account"))
                    .collect(),
            },
            None => TipPolicy::None,
        };
        router.add(ManagedSender::new(
            Arc::new(SimulateOnly {
                rpc: rpc.clone(),
                out: sims.clone(),
                tip,
            }),
            Duration::from_secs(10),
            None,
        ));
    }
    // Intent / policy sender lists: dry run only has the simulate-only (rpc) sender.
    let intent_senders: Vec<SenderKind> = if send {
        kinds.clone()
    } else {
        vec![SenderKind::Rpc]
    };
    let (tx, _rx) = broadcast::channel(1024);
    let mut observer = Observer::new(
        store.clone(),
        rpc.clone(),
        locks.clone(),
        tx.clone(),
        ObserverConfig {
            poll_interval: Duration::from_millis(poll_ms),
            rebroadcast_interval: Duration::from_secs(2),
            max_rebroadcasts: 10,
        },
    );
    if let Some(s) = &stream {
        // signatureSubscribe wakes the observer the moment a send is processed.
        observer = observer.with_stream(s.clone());
    }
    let observer = Arc::new(observer);
    observer.clone().spawn();
    // Canary wallet policy (explicit, not defaults): per-wallet mainnet switch
    // on for this disposable wallet only; reserve = system-account rent floor
    // plus margin (the 0.01 SOL default reserve would exceed a ~US$1 wallet);
    // no tips; daily loss capped at 0.01 SOL.
    let policy = WalletPolicy {
        max_order: Lamports(MAX_BUY_LAMPORTS),
        max_daily_turnover: Lamports(50_000_000),
        max_trades_per_day: 2 * MAX_ITERATIONS as u32 + 4,
        min_sol_reserve: Lamports(WALLET_RENT_FLOOR + 100_000),
        max_priority_fee: Lamports(50_000),
        max_tip: Lamports(tip),
        daily_loss_limit: Lamports(10_000_000),
        mainnet_enabled: true,
        allowed_senders: intent_senders.clone(),
        ..Default::default()
    };
    let engine = Engine::new(
        EngineConfig {
            network: net,
            // Operator gate for this process. In dry-run mode the router holds
            // only the simulate-only sender, which cannot forward.
            mainnet_enabled: true,
            max_state_age: Duration::from_secs(3),
            ..Default::default()
        },
        store.clone(),
        vec![adapter.clone()],
        router,
        blockhash,
        locks,
        Arc::new(OneSigner(signer)),
        balances,
        Arc::new(FixedPolicy(Mutex::new(policy))),
        observer.handle.clone(),
        tx,
    );
    let user_id = Uuid::new_v4();
    let wallet_id = Uuid::new_v4();
    let run_tag = Uuid::new_v4().simple().to_string();
    let mk = |side: Side, amount: AmountSpec, key: String| TradeIntent {
        id: Uuid::new_v4(),
        user_id,
        wallet_id,
        network: net,
        side,
        mint,
        amount,
        slippage,
        fees: FeePreference {
            compute_unit_price_micro_lamports: cu_price,
            max_priority_fee: Lamports(50_000),
            tip: Lamports(tip),
        },
        senders: intent_senders.clone(),
        idempotency_key: key,
        requires_approval: false,
        created_at: chrono::Utc::now(),
    };

    let mut rows = vec![];
    let mut failed: Option<String> = None;
    let runs = iterations;
    'outer: for it in 0..runs {
        // Dry run simulates the buy only: a sell needs tokens the wallet does not hold.
        let sides: &[Side] = if send {
            &[Side::Buy, Side::Sell]
        } else {
            &[Side::Buy]
        };
        for &side in sides {
            let sol_before = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
            let tok_before = token_balance(&rpc, &wallet, &mint, &token_program).await?;
            let amount = match side {
                Side::Buy => AmountSpec::ExactSolIn {
                    lamports: Lamports(buy),
                },
                Side::Sell => AmountSpec::SellPercent { bps: Bps::MAX },
            };
            let intent = mk(
                side,
                amount,
                format!("canary-{run_tag}-{it}-{}", side.as_str()),
            );
            let send_slot = rpc.get_slot(Commitment::Processed).await?;
            let o = match engine
                .submit(intent, wallet, Some(LatencyTrace::start()))
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    failed = Some(format!("{} refused by engine: {e}", side.as_str()));
                    break 'outer;
                }
            };
            let o = wait_terminal(&store, o.id, Duration::from_secs(120)).await?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            let sol_after = rpc.get_balance(&wallet, Commitment::Confirmed).await?.0;
            let tok_after = token_balance(&rpc, &wallet, &mint, &token_program).await?;
            let trace = observer
                .traces
                .lock()
                .iter()
                .find(|(id, _, _)| *id == o.id)
                .map(|(_, t, _)| t.clone())
                .or_else(|| engine.take_trace(o.id));
            let fill = o.fill.clone();
            let verified = match &fill {
                Some(f) if f.success => {
                    f.token_delta_raw == tok_after as i128 - tok_before as i128
                        && f.sol_delta_lamports == sol_after as i128 - sol_before as i128
                }
                _ => false,
            };
            let sig = o.signed.as_ref().map(|s| s.signature.clone());
            let row = json!({
                "iteration": it,
                "side": side.as_str(),
                "order_id": o.id,
                "state": o.state.as_str(),
                "reject_reason": o.reject_reason,
                "signature": sig,
                "explorer": sig.as_ref().filter(|_| send && net == Network::Mainnet).map(|s| format!("https://solscan.io/tx/{s}")),
                "send_slot": send_slot,
                "landed_slot": fill.as_ref().map(|f| f.slot),
                "slots_to_land": fill.as_ref().map(|f| f.slot.saturating_sub(send_slot)),
                "fee_lamports": fill.as_ref().map(|f| f.fee_lamports),
                "compute_units_consumed": fill.as_ref().and_then(|f| f.compute_units_consumed),
                "quote_expected_out": o.quote.as_ref().map(|q| q.expected_out.to_string()),
                "quote_min_out": o.quote.as_ref().map(|q| q.min_out.to_string()),
                "sol_delta": (sol_after as i128 - sol_before as i128).to_string(),
                "token_delta": (tok_after as i128 - tok_before as i128).to_string(),
                "independently_verified_balances": verified,
                "intent_to_signed_us": trace.as_ref().and_then(|t| t.between_us(Stage::IntentAccepted, Stage::Signed)),
                "signed_to_sender_ack_us": trace.as_ref().and_then(|t| t.between_us(Stage::Signed, Stage::SenderAck)),
                "signed_to_processed_us": trace.as_ref().and_then(|t| t.between_us(Stage::Signed, Stage::FirstObservedProcessed)),
                "signed_to_finalized_us": trace.as_ref().and_then(|t| t.between_us(Stage::Signed, Stage::Finalized)),
            });
            println!("{}", serde_json::to_string(&row)?);
            rows.push(row);
            if send && (o.state != OrderState::Finalized || !verified) {
                failed = Some(format!(
                    "{} ended {} (verified={verified})",
                    side.as_str(),
                    o.state
                ));
                break 'outer;
            }
        }
    }
    let sims = sims.lock().clone();
    if !send {
        for s in &sims {
            println!(
                "{}",
                json!({"simulation": {"err": s["err"], "units_consumed": s["units_consumed"], "rpc_error": s["rpc_error"]}})
            );
        }
    }
    let report = json!({
        "kind": "mainnet_pump_canary",
        "network": net.as_str(),
        "mode": if send { "send" } else { "dry_run" },
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "plan": plan,
        "rows": rows,
        "simulations": sims,
        "websocket": ws_info,
        "failure": failed,
    });
    let path = a.get("report").cloned().unwrap_or_else(|| {
        format!(
            "benchmarks/results/{}-canary-{}-{}.json",
            net.as_str(),
            if send { "send" } else { "dryrun" },
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    });
    if let Some(d) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", json!({"report": path}));
    if let Some(f) = failed {
        bail!("canary stopped: {f}");
    }
    if !send {
        let ok = sims
            .iter()
            .any(|s| s["err"].is_null() && s["rpc_error"].is_null());
        if !ok {
            bail!("dry run: simulation did not succeed; see report");
        }
        println!("dry run OK: the buy simulates cleanly on {net}. Nothing was broadcast.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_signatures_clears_only_signature_bytes() {
        let mut wire = vec![1u8];
        wire.extend([7u8; 64]);
        wire.extend([9u8; 10]);
        assert!(zero_signatures(&mut wire));
        assert!(wire[1..65].iter().all(|b| *b == 0));
        assert!(wire[65..].iter().all(|b| *b == 9));
        assert_eq!(wire[0], 1);
    }

    #[test]
    fn zero_signatures_rejects_bad_layout() {
        assert!(!zero_signatures(&mut []));
        assert!(!zero_signatures(&mut [0u8]));
        assert!(!zero_signatures(&mut [1u8, 1, 2]));
        assert!(!zero_signatures(&mut [0x80u8; 200]));
    }
}
