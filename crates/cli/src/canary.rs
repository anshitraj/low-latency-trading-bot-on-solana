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
use trader_market_data::{AccountStream, RpcClient, RpcConfig};
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
/// Round trips per run (raised for latency samples; each is ~US$0.002).
pub const MAX_ITERATIONS: usize = 25;
/// Priority fee ceiling (micro-lamports per CU).
pub const MAX_CU_PRICE: u64 = 2_500_000;
/// Default priority fee per transaction (lamports): the median paid by GMGN and
/// Photon trades on 2026-11-12 (BENCHMARKS 2j), ~US$0.008.
pub const DEFAULT_PRIORITY_LAMPORTS: u64 = 100_000;
/// Priority fee ceiling per transaction (0.00025 SOL, ~US$0.02).
pub const MAX_PRIORITY_LAMPORTS: u64 = 400_000;
/// Tip ceiling per transaction (0.00005 SOL).
pub const MAX_TIP_LAMPORTS: u64 = 50_000;
/// Tip ceiling for the Helius Sender Max route only (its minimum is 0.001 SOL,
/// ~US$0.08 per transaction).
pub const MAX_TIP_LAMPORTS_SENDER_MAX: u64 = trader_sender_rpc::HELIUS_MAX_MIN_TIP;
/// Helius Sender Max, Frankfurt: Jito auction + staked paths (no swqos_only).
pub const HELIUS_FRANKFURT_MAX: &str = "http://fra-sender.helius-rpc.com/fast";
pub const JITO_FRANKFURT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf";
/// Helius Sender, Frankfurt, SWQoS-only tier (regional endpoints are plain HTTP).
pub const HELIUS_FRANKFURT: &str = "http://fra-sender.helius-rpc.com/fast?swqos_only=true";
const MAINNET_ACK: &str = "I_UNDERSTAND_REAL_FUNDS_AT_RISK";
/// Estimate only (the engine sets the real limit from the adapter).
const EST_CU_LIMIT: u64 = trader_protocol_pump::DEFAULT_COMPUTE_UNITS as u64;
/// System-account rent-exempt minimum (0 data bytes); the fee payer must stay
/// above it. Read from the cluster at startup (650,240 on mainnet 2026-11-12).
fn wallet_rent_floor() -> u64 {
    rent_exempt_minimum(0).0
}

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

/// Retry a read on rate limits / transient transport errors (public RPCs
/// return 429 under bursts). Up to 6 attempts, 0.5 s doubling to 4 s.
async fn retry<T, F, Fut>(what: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut wait = Duration::from_millis(500);
    let mut last = None;
    for _ in 0..6 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = e.to_string();
                let transient = msg.contains("429")
                    || msg.contains("rate limit")
                    || msg.contains("timeout")
                    || msg.contains("transport");
                if !transient {
                    return Err(e);
                }
                eprintln!("{what}: {msg}; retrying in {} ms", wait.as_millis());
                last = Some(e);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(4));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("{what}: retries exhausted")))
}

async fn sol_balance(rpc: &RpcClient, wallet: &Pubkey) -> anyhow::Result<u64> {
    retry("getBalance", || async {
        Ok(rpc.get_balance(wallet, Commitment::Confirmed).await?.0)
    })
    .await
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
    // Providers: --rpc / RPC_URL first, then ~/.trader/rpc_urls (mainnet only;
    // a local test never mixes in other endpoints). EVERY provider must be on
    // the expected cluster; one that cannot be reached is dropped.
    let candidates = if net == Network::Localnet {
        trader_market_data::parse_endpoint_lines(&url)
    } else {
        trader_market_data::load_endpoint_specs(Some(&url))
    };
    let mut specs = vec![];
    for sp in candidates {
        let host = trader_market_data::rpc::url_host(&sp.http);
        let mut c = RpcConfig::new(sp.http.clone());
        c.timeout = Duration::from_secs(10);
        match RpcClient::new(c)?.get_genesis_hash().await {
            Ok(g) => {
                net.verify_genesis(&g)
                    .map_err(|e| anyhow!("refusing provider {host}: {e}"))?;
                specs.push(sp);
            }
            Err(e) => eprintln!("provider {host} unreachable, skipped: {e}"),
        }
    }
    if specs.is_empty() {
        bail!("no reachable RPC provider");
    }
    let rpc = if net == Network::Localnet {
        let mut cfg = RpcConfig::new(specs[0].http.clone());
        cfg.timeout = Duration::from_secs(20);
        RpcClient::new(cfg)?
    } else {
        RpcClient::pool(&specs, Duration::from_secs(20))?
    };
    let rent_per_byte = trader_market_data::rent::sync_rent(&rpc)
        .await
        .map_err(|e| anyhow!("reading rent rate: {e}"))?;

    let mint = Pubkey::from_str(
        a.get("mint")
            .ok_or_else(|| anyhow!("--mint <Pump bonding-curve mint> required"))?,
    )?;
    let buy: u64 = parse(a, "buy-lamports", DEFAULT_BUY_LAMPORTS)?;
    if buy == 0 || buy > MAX_BUY_LAMPORTS {
        bail!("--buy-lamports must be 1..={MAX_BUY_LAMPORTS} (hard cap 0.001 SOL)");
    }
    // --sell-only: one SELL of 100% of the held tokens (closes a position left
    // open by an interrupted run). Requires --send for a real sell.
    let sell_only = a.contains_key("sell-only");
    let iterations: usize = if sell_only {
        1
    } else {
        parse(a, "iterations", 1)?
    };
    if iterations == 0 || iterations > MAX_ITERATIONS {
        bail!("--iterations must be 1..={MAX_ITERATIONS}");
    }
    // Priority fee. Default: `--priority-lamports` (total per transaction,
    // default 100,000 = GMGN/Photon median on 2026-11-12, BENCHMARKS 2j) turned
    // into a per-CU price for the adapter's CU limit. `--cu-price N` sets the
    // per-CU price directly; `--cu-price auto` = p75 of getRecentPrioritizationFees
    // for the Pump program and this coin's curve, per order. All capped by
    // MAX_PRIORITY_LAMPORTS per transaction.
    let cu_auto = a.get("cu-price").map(String::as_str) == Some("auto");
    let cu_limit = trader_protocol_pump::DEFAULT_COMPUTE_UNITS as u64;
    // `--fee-ab 10000,100000,300000`: priority-fee levels (lamports at the buy
    // CU limit, i.e. a per-CU price) rotated per round trip with the route
    // fixed, to see whether a higher fee gets the trade into the block sooner.
    let fee_ab: Vec<u64> = match a.get("fee-ab") {
        Some(v) => v
            .split(',')
            .map(|x| x.trim().parse::<u64>())
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow!("--fee-ab: {e}"))?,
        None => vec![],
    };
    if fee_ab.iter().any(|f| *f > MAX_PRIORITY_LAMPORTS) {
        bail!("--fee-ab levels must be <= {MAX_PRIORITY_LAMPORTS}");
    }
    if !fee_ab.is_empty() && cu_auto {
        bail!("use --fee-ab or --cu-price auto, not both");
    }
    let priority_lamports: u64 = parse(a, "priority-lamports", DEFAULT_PRIORITY_LAMPORTS)?;
    if priority_lamports > MAX_PRIORITY_LAMPORTS {
        bail!("--priority-lamports must be <= {MAX_PRIORITY_LAMPORTS}");
    }
    let cu_price: u64 = if cu_auto {
        MAX_CU_PRICE
    } else if a.contains_key("cu-price") {
        parse(a, "cu-price", 0)?
    } else {
        (priority_lamports * 1_000_000).div_ceil(cu_limit)
    };
    if cu_price > MAX_CU_PRICE {
        bail!("--cu-price must be <= {MAX_CU_PRICE} micro-lamports/CU");
    }
    // 400 ms (one slot) keeps signature polling under public-RPC rate limits.
    // Fallback poll; with the WebSocket on, a landing notification triggers
    // fast polling anyway, so 1 s keeps RPC load low (free plans rate-limit).
    let poll_ms: u64 = parse(
        a,
        "poll-ms",
        if a.contains_key("no-ws") { 400 } else { 1_000 },
    )?;
    if !(100..=5_000).contains(&poll_ms) {
        bail!("--poll-ms must be 100..=5000");
    }
    // Routes. Each route is a set of senders that get the SAME signed bytes in
    // parallel: rpc (sendTransaction), tpu (QUIC straight to the current and
    // next leaders), jito (block engine; `jito@frankfurt+amsterdam+ny+tokyo`
    // picks regions), helius (Helius Sender, SWQoS-only). `--routes "a;b;c"`
    // assigns round trip i to route i % n, so routes are compared under the same
    // market conditions (phase 2 A/B). `--senders a,b` = one route.
    let jito_regions: Vec<String> = a
        .get("jito-regions")
        .map(|v| {
            v.split(',')
                .map(|x| x.trim().to_lowercase())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let jito_url = a
        .get("jito-url")
        .cloned()
        .unwrap_or_else(|| JITO_FRANKFURT.to_string());
    let default_jito_urls: Vec<String> = if jito_regions.is_empty() {
        vec![jito_url.clone()]
    } else {
        jito_regions.iter().map(|r| jito_region_url(r)).collect()
    };
    let route_text = match (a.get("routes"), a.get("senders")) {
        (Some(_), Some(_)) => bail!("use --routes or --senders, not both"),
        (Some(r), None) => r.clone(),
        (None, Some(s)) => s.clone(),
        (None, None) => "rpc".into(),
    };
    let tip_auto = a.get("tip-lamports").map(String::as_str) == Some("auto");
    let tip_arg: Option<u64> = if tip_auto || !a.contains_key("tip-lamports") {
        None
    } else {
        Some(parse(a, "tip-lamports", 0)?)
    };
    let routes = parse_routes(&route_text, &default_jito_urls, tip_arg, tip_auto)?;
    let kinds: Vec<SenderKind> =
        routes
            .iter()
            .flat_map(|r| r.kinds.iter().copied())
            .fold(vec![], |mut v, k| {
                if !v.contains(&k) {
                    v.push(k);
                }
                v
            });
    let sender_names: Vec<String> = routes.iter().map(|r| r.name.clone()).collect();
    let use_jito = kinds.contains(&SenderKind::Jito);
    // Largest tip any route pays (fee estimate and the policy ceiling).
    let tip: u64 = routes.iter().map(|r| r.tip).max().unwrap_or(0);
    let helius_url = a
        .get("helius-url")
        .cloned()
        .unwrap_or_else(|| HELIUS_FRANKFURT.to_string());
    let tpu_leaders: usize = parse(a, "tpu-leaders", 3)?;
    if !(1..=8).contains(&tpu_leaders) {
        bail!("--tpu-leaders must be 1..=8");
    }
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
    let need = wallet_rent_floor() + 100_000 + rent_new + buy + 2 * fee_per_tx + est_max_loss;
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
        "cu_price_micro_lamports": if cu_auto { json!("auto") } else { json!(cu_price) },
        "tip_mode": if tip_auto { "auto" } else { "fixed" },
        "providers": rpc.hosts(),
        "tpu_leaders": tpu_leaders,
        "jito_regions": jito_regions,
        "routes": routes.iter().map(|r| json!({
            "route": r.name,
            "tip_lamports": if r.tip_auto { json!("auto") } else { json!(r.tip) },
            "jito_hosts": r.jito_urls.iter().map(|u| trader_market_data::rpc::url_host(u)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "route_assignment": "round trip i uses route i % routes",
        "shred_feed": a.get("shred-feed"),
        "fee_ab": fee_ab,
        "shred_watch": a.get("shred-watch"),
        "received_notifications_from": a.get("received-from"),
        "senders": sender_names,
        "tip_lamports_per_tx": tip,
        "est_fee_per_tx_lamports": fee_per_tx,
        "rent_locked_if_new_lamports": rent_new,
        "rent_lamports_per_byte": rent_per_byte,
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
    let streams: Vec<AccountStream> = if a.contains_key("no-ws") {
        vec![]
    } else if let Some(w) = a.get("ws") {
        vec![AccountStream::spawn(w.clone())]
    } else {
        specs
            .iter()
            .map(|sp| AccountStream::spawn(sp.ws_url()))
            .collect()
    };
    if !a.contains_key("no-slot-updates") {
        for st in &streams {
            st.enable_slot_updates();
        }
    }
    // `--received-from 10.30.0.3`: also ask these providers (Agave, full RPC
    // API) for `receivedSignature` = entry batch assembled from shreds.
    let received_hosts: Vec<String> = a
        .get("received-from")
        .map(|v| {
            v.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for st in &streams {
        if received_hosts.iter().any(|h| *h == st.host()) {
            st.enable_received_notifications();
        }
    }
    // The first provider's stream feeds coin state and balances; all of them
    // report signatures (independent landing observers) and slots.
    let stream = streams.first().cloned();
    // `--shred-feed 0.0.0.0:9900`: signatures reported by tools/shred-observer on
    // our own node the moment their entry batch is in the received shreds.
    let shred_feed = match a.get("shred-feed") {
        Some(b) => Some(
            crate::shred_feed::ShredFeed::spawn(b).map_err(|e| anyhow!("--shred-feed {b}: {e}"))?,
        ),
        None => None,
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
    // ~2 slots: a fresh hash costs ~2.5 getLatestBlockhash/s, spread over the pool.
    blockhash.spawn(rpc.clone(), Duration::from_millis(400));
    let sims = Arc::new(Mutex::new(vec![]));
    let jito_tip_accounts = if use_jito {
        let first = routes
            .iter()
            .find_map(|r| r.jito_urls.first().cloned())
            .expect("a jito route has a url");
        let j = trader_sender_jito::JitoSender::new(first, false, None, Duration::from_secs(5));
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
    let helius_tip_accounts: Vec<Pubkey> = trader_sender_rpc::HELIUS_TIP_ACCOUNTS
        .iter()
        .map(|k| Pubkey::from_str(k).expect("valid tip account"))
        .collect();
    // Senders shared by every route that uses them (one TPU client and QUIC
    // connection cache, one client per Jito region, one Helius client).
    let tpu_sender = if send && kinds.contains(&SenderKind::Tpu) {
        Some(
            trader_sender_tpu::TpuSender::start(rpc.clone(), stream.clone(), tpu_leaders)
                .await
                .map_err(|e| anyhow!("tpu sender: {e}"))?,
        )
    } else {
        None
    };
    let helius_sender = if send
        && routes
            .iter()
            .any(|r| r.kinds.contains(&SenderKind::Helius) && !r.helius_max)
    {
        let mut hc = RpcConfig::new(helius_url.clone());
        hc.timeout = Duration::from_secs(5);
        Some(Arc::new(RpcSender::helius_sender(
            RpcClient::new(hc)?,
            true,
        )))
    } else {
        None
    };
    let helius_max_sender = if send && routes.iter().any(|r| r.helius_max) {
        let mut hc = RpcConfig::new(
            a.get("helius-max-url")
                .cloned()
                .unwrap_or_else(|| HELIUS_FRANKFURT_MAX.to_string()),
        );
        hc.timeout = Duration::from_secs(5);
        Some(Arc::new(RpcSender::helius_sender(
            RpcClient::new(hc)?,
            false,
        )))
    } else {
        None
    };
    let rpc_sender = Arc::new(RpcSender::standard(rpc.clone(), false));
    let mut jito_clients: HashMap<String, Arc<trader_sender_jito::JitoSender>> = HashMap::new();
    let mut routers = vec![];
    for route in &routes {
        let mut router = Router::new();
        if send {
            for k in &route.kinds {
                match k {
                    SenderKind::Rpc => router.add(ManagedSender::new(
                        rpc_sender.clone(),
                        Duration::from_secs(10),
                        None,
                    )),
                    SenderKind::Tpu => router.add(ManagedSender::new(
                        tpu_sender.clone().expect("tpu started above"),
                        Duration::from_secs(3),
                        None,
                    )),
                    SenderKind::Jito => {
                        let accts = jito_tip_accounts.clone().expect("jito set up above");
                        for ju in &route.jito_urls {
                            let j = jito_clients
                                .entry(ju.clone())
                                .or_insert_with(|| {
                                    Arc::new(
                                        trader_sender_jito::JitoSender::new(
                                            ju.clone(),
                                            false,
                                            None,
                                            Duration::from_secs(5),
                                        )
                                        .with_tip_accounts(accts.clone()),
                                    )
                                })
                                .clone();
                            // Jito's default limit is 1 request/s per IP per region.
                            router.add(ManagedSender::new(
                                j,
                                Duration::from_secs(5),
                                Some(trader_router::TokenBucket::new(1, Duration::from_secs(1))),
                            ));
                        }
                    }
                    SenderKind::Helius => router.add(ManagedSender::new(
                        if route.helius_max {
                            helius_max_sender.clone().expect("helius-max set up above")
                        } else {
                            helius_sender.clone().expect("helius set up above")
                        },
                        Duration::from_secs(5),
                        Some(trader_router::TokenBucket::new(1, Duration::from_secs(1))),
                    )),
                    _ => unreachable!(),
                }
            }
        } else {
            // Dry run: one simulate-only sender carrying this route's tip policy,
            // so the simulated transaction has the shape that would be sent.
            let tip = if route.kinds.contains(&SenderKind::Jito) {
                TipPolicy::Required {
                    min_lamports: trader_sender_jito::JITO_MIN_TIP_LAMPORTS,
                    accounts: jito_tip_accounts.clone().expect("jito set up above"),
                }
            } else if route.kinds.contains(&SenderKind::Helius) {
                TipPolicy::Required {
                    min_lamports: if route.helius_max {
                        trader_sender_rpc::HELIUS_MAX_MIN_TIP
                    } else {
                        trader_sender_rpc::HELIUS_SWQOS_MIN_TIP
                    },
                    accounts: helius_tip_accounts.clone(),
                }
            } else {
                TipPolicy::None
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
        routers.push(router);
    }
    // Policy sender list; dry run only has the simulate-only (rpc) sender.
    let allowed_senders: Vec<SenderKind> = if send {
        kinds.clone()
    } else {
        vec![SenderKind::Rpc]
    };
    let route_senders = |r: usize| -> Vec<SenderKind> {
        if send {
            routes[r].kinds.clone()
        } else {
            vec![SenderKind::Rpc]
        }
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
    if !streams.is_empty() {
        // signatureSubscribe on every provider; the first notification wakes the observer.
        observer = observer.with_streams(streams.clone());
    }
    let observer = Arc::new(observer);
    observer.clone().spawn();
    // Canary wallet policy (explicit, not defaults): per-wallet mainnet switch
    // on for this disposable wallet only; reserve = system-account rent floor
    // plus margin (the 0.01 SOL default reserve would exceed a ~US$1 wallet);
    // tips capped at the largest route tip; daily loss capped at 0.01 SOL.
    let policy = WalletPolicy {
        max_order: Lamports(MAX_BUY_LAMPORTS),
        max_daily_turnover: Lamports(50_000_000),
        max_trades_per_day: 2 * MAX_ITERATIONS as u32 + 4,
        min_sol_reserve: Lamports(wallet_rent_floor() + 100_000),
        max_priority_fee: Lamports(MAX_PRIORITY_LAMPORTS),
        max_tip: Lamports(tip),
        daily_loss_limit: Lamports(10_000_000),
        mainnet_enabled: true,
        allowed_senders: allowed_senders.clone(),
        ..Default::default()
    };
    // One engine per route, sharing store, wallet lock, blockhash, balances,
    // policy and observer: orders stay serialized per wallet and one observer
    // tracks them all; only the delivery route differs.
    let policy_src = Arc::new(FixedPolicy(Mutex::new(policy)));
    let engines: Vec<Arc<Engine>> = routers
        .into_iter()
        .map(|router| {
            Engine::new(
                EngineConfig {
                    network: net,
                    // Operator gate for this process. In dry-run mode the router
                    // holds only the simulate-only sender, which cannot forward.
                    mainnet_enabled: true,
                    max_state_age: Duration::from_secs(3),
                    ..Default::default()
                },
                store.clone(),
                vec![adapter.clone()],
                router,
                blockhash.clone(),
                locks.clone(),
                Arc::new(OneSigner(signer.clone())),
                balances.clone(),
                policy_src.clone(),
                observer.handle.clone(),
                tx.clone(),
            )
        })
        .collect();
    // `--shred-watch 10.30.0.3:9901`: tell tools/shred-observer each signature
    // right before sending, so it can report the first shred carrying it
    // without waiting for the whole entry batch. One non-blocking UDP send.
    if let Some(addr) = a.get("shred-watch") {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        sock.connect(addr)
            .map_err(|e| anyhow!("--shred-watch {addr}: {e}"))?;
        sock.set_nonblocking(true)?;
        let sock = Arc::new(sock);
        for e in &engines {
            let s = sock.clone();
            e.set_presend_hook(Arc::new(move |sig: &str| {
                if let Ok(b) = bs58::decode(sig).into_vec() {
                    let _ = s.send(&b);
                }
            }));
        }
    }
    let user_id = Uuid::new_v4();
    let wallet_id = Uuid::new_v4();
    let run_tag = Uuid::new_v4().simple().to_string();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let fee_keys = [
        trader_protocol_pump::PUMP_PROGRAM_ID,
        pump_rust_client::pda::pump::bonding_curve(&mint).0,
    ];
    let mk = |side: Side,
              amount: AmountSpec,
              key: String,
              cu_price: u64,
              tip: u64,
              senders: Vec<SenderKind>| TradeIntent {
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
            max_priority_fee: Lamports(MAX_PRIORITY_LAMPORTS),
            tip: Lamports(tip),
        },
        senders,
        idempotency_key: key,
        requires_approval: false,
        created_at: chrono::Utc::now(),
    };

    let mut rows = vec![];
    let mut failed: Option<String> = None;
    let runs = iterations;
    'outer: for it in 0..runs {
        // Round trip `it` (buy and sell) uses route it % n, so each route sees
        // both sides and the routes interleave in time.
        let ri = it % routes.len();
        let route = &routes[ri];
        let engine = &engines[ri];
        // Dry run simulates the buy only: a sell needs tokens the wallet does not hold.
        let sides: &[Side] = if sell_only {
            &[Side::Sell]
        } else if send {
            &[Side::Buy, Side::Sell]
        } else {
            &[Side::Buy]
        };
        for &side in sides {
            let sol_before = sol_balance(&rpc, &wallet).await?;
            let tok_before = retry("token balance", || {
                token_balance(&rpc, &wallet, &mint, &token_program)
            })
            .await?;
            if side == Side::Sell && tok_before == 0 {
                failed = Some("nothing to sell: wallet holds 0 tokens of this mint".into());
                break 'outer;
            }
            let amount = match side {
                Side::Buy => AmountSpec::ExactSolIn {
                    lamports: Lamports(buy),
                },
                Side::Sell => AmountSpec::SellPercent { bps: Bps::MAX },
            };
            let fee_level = (!fee_ab.is_empty()).then(|| fee_ab[it % fee_ab.len()]);
            let (cu_now, cu_info) = if let Some(lvl) = fee_level {
                (
                    (lvl * 1_000_000).div_ceil(cu_limit),
                    json!({"fee_level": lvl}),
                )
            } else if cu_auto {
                auto_cu_price(&rpc, &fee_keys, MAX_CU_PRICE).await
            } else {
                (cu_price, json!(null))
            };
            let (tip_now, tip_info) = if route.tip_auto {
                auto_jito_tip(&http, MAX_TIP_LAMPORTS).await
            } else {
                (route.tip, json!(null))
            };
            let intent = mk(
                side,
                amount,
                format!("canary-{run_tag}-{it}-{}", side.as_str()),
                cu_now,
                tip_now,
                route_senders(ri),
            );
            // Slot at submit: the stream's live slot (no extra round trip), else RPC.
            let (send_slot, send_slot_source) = match &stream {
                Some(st) if st.is_live(Duration::from_millis(1500)) => (st.last_slot(), "ws"),
                _ => (
                    retry("getSlot", || async {
                        Ok(rpc.get_slot(Commitment::Processed).await?)
                    })
                    .await?,
                    "rpc",
                ),
            };
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
            let o = match wait_terminal(&store, o.id, Duration::from_secs(120)).await {
                Ok(o) => o,
                Err(e) => {
                    // Still write the report (and the open-position hint) below.
                    failed = Some(format!("{}: {e}", side.as_str()));
                    break 'outer;
                }
            };
            tokio::time::sleep(Duration::from_millis(800)).await;
            let sol_after = sol_balance(&rpc, &wallet).await?;
            let tok_after = retry("token balance", || {
                token_balance(&rpc, &wallet, &mint, &token_program)
            })
            .await?;
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
            let signed_at = trace.as_ref().and_then(|t| t.instant(Stage::Signed));
            let ms_after = |t: std::time::Instant| -> Option<f64> {
                signed_at.map(|s0| {
                    if t >= s0 {
                        t.duration_since(s0).as_secs_f64() * 1e3
                    } else {
                        -(s0.duration_since(t).as_secs_f64() * 1e3)
                    }
                })
            };
            let landed = fill.as_ref().map(|f| f.slot);
            let shred_seen = match (&shred_feed, sig.as_deref()) {
                (Some(f), Some(sg)) => f.seen(sg),
                _ => None,
            };
            let mut providers = vec![];
            for (i, st) in streams.iter().enumerate() {
                let seen = sig
                    .as_deref()
                    .and_then(|sg| st.seen_signature(sg))
                    .and_then(|(_, t)| ms_after(t));
                let received = sig
                    .as_deref()
                    .and_then(|sg| st.received_signature(sg))
                    .and_then(|(_, t)| ms_after(t));
                let slot_first = landed
                    .and_then(|ls| st.slot_first_seen(ls))
                    .and_then(ms_after);
                let events: serde_json::Map<String, serde_json::Value> = landed
                    .map(|ls| st.slot_events(ls))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| (e.kind, json!(ms_after(e.arrived))))
                    .collect();
                providers.push(json!({
                    "provider": format!("p{i}:{}", st.host()),
                    "signature_processed_ms_after_signed": seen,
                    "signature_received_ms_after_signed": received,
                    "landed_slot_first_seen_ms_after_signed": slot_first,
                    "landed_slot_events_ms_after_signed": events,
                    "slot_updates_supported": st.slot_updates_supported(),
                }));
            }
            let senders: Vec<serde_json::Value> = store
                .attempts()
                .into_iter()
                .filter(|r| r.order_id == o.id && !r.rebroadcast)
                .map(|r| {
                    json!({
                        "sender": r.attempt.sender.as_str(),
                        "endpoint": r.attempt.endpoint,
                        "latency_us": r.attempt.latency_us,
                        "outcome": r.attempt.outcome.code(),
                    })
                })
                .collect();
            let row = json!({
                "iteration": it,
                "route": route.name,
                "fee_level": fee_level.map(|l| format!("{l:>7} lamports")),
                "compute_unit_limit": o.signed.as_ref().map(|s| s.compute_unit_limit),
                "side": side.as_str(),
                "order_id": o.id,
                "state": o.state.as_str(),
                "reject_reason": o.reject_reason,
                "signature": sig,
                "explorer": sig.as_ref().filter(|_| send && net == Network::Mainnet).map(|s| format!("https://solscan.io/tx/{s}")),
                "send_slot": send_slot,
                "send_slot_source": send_slot_source,
                "cu_price_micro_lamports": o.signed.as_ref().map(|s| s.compute_unit_price_micro_lamports),
                "tip_lamports": o.signed.as_ref().map(|s| s.tip_lamports),
                "cu_price_auto": cu_info,
                "tip_auto": tip_info,
                "senders": senders,
                "providers": providers,
                "shred_seen_ms_after_signed": shred_seen.and_then(|(t, _)| ms_after(t)),
                "shred_slot": shred_seen.map(|(_, s)| s),
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
    if send {
        // Read-only: who led each landing slot, how far that leader is from us,
        // and where our transaction sits in its block.
        annotate_landings(&rpc, &mut rows).await;
    }
    let summary = summarize(&rows);
    if send || !rows.is_empty() {
        println!("{}", json!({ "summary": summary }));
    }
    let report = json!({
        "kind": "mainnet_pump_canary",
        "summary": summary,
        "rpc_providers": rpc.stats(),
        "shred_feed_records": shred_feed.as_ref().map(|f| f.records()),
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
        if send {
            let held = token_balance(&rpc, &wallet, &mint, &token_program)
                .await
                .unwrap_or(0);
            if held > 0 {
                eprintln!(
                    "position still open ({held} raw tokens). Close it with: trader-cli mainnet-canary --mint {mint} --senders {} --sell-only --send",
                    sender_names.join(",")
                );
            }
        }
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

/// Leader location bucket from our QUIC handshake RTT to its TPU (measured
/// from wherever this runs; from Frankfurt: Amsterdam/Frankfurt < 20 ms,
/// rest of Europe < 60 ms, Americas/Asia above).
pub fn rtt_region(rtt_ms: Option<f64>) -> &'static str {
    match rtt_ms {
        Some(x) if x < 20.0 => "near(<20ms)",
        Some(x) if x < 60.0 => "mid(20-60ms)",
        Some(_) => "far(>=60ms)",
        None => "unknown",
    }
}

/// Adds to each landed row: `leader` (of the landing slot), `send_slot_leader`,
/// `leader_tpu`, `leader_rtt_ms` (QUIC handshake from here, best of 2),
/// `leader_region`, `block_index` / `block_tx_count` / `block_position_pct`.
/// Read-only; failures leave the fields null.
async fn annotate_landings(rpc: &RpcClient, rows: &mut [serde_json::Value]) {
    let leader_of = |slot: u64| async move {
        rpc.call::<Vec<String>>("getSlotLeaders", json!([slot, 1]))
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
    };
    let tpu: HashMap<String, String> = rpc
        .call::<Vec<serde_json::Value>>("getClusterNodes", json!([]))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| {
            Some((
                n["pubkey"].as_str()?.to_string(),
                n["tpuQuic"].as_str()?.to_string(),
            ))
        })
        .collect();
    let conns = trader_sender_tpu::ConnCache::new(Duration::from_secs(2)).ok();
    let mut rtt_cache: HashMap<String, Option<f64>> = HashMap::new();
    for row in rows.iter_mut() {
        let Some(landed) = row["landed_slot"].as_u64() else {
            continue;
        };
        let leader = leader_of(landed).await;
        let send_leader = match row["send_slot"].as_u64() {
            Some(s) => leader_of(s).await,
            None => None,
        };
        let addr = leader.as_ref().and_then(|l| tpu.get(l).cloned());
        let rtt = match (&addr, &conns) {
            (Some(a), Some(c)) => match rtt_cache.get(a) {
                Some(v) => *v,
                None => {
                    let mut best: Option<f64> = None;
                    if let Ok(sa) = a.parse::<std::net::SocketAddr>() {
                        for _ in 0..2 {
                            if let Ok(d) = c.handshake_time(sa).await {
                                let ms = d.as_secs_f64() * 1e3;
                                best = Some(best.map_or(ms, |b: f64| b.min(ms)));
                            }
                        }
                    }
                    rtt_cache.insert(a.clone(), best);
                    best
                }
            },
            _ => None,
        };
        let (idx, count) = match (
            row["signature"].as_str(),
            rpc.call::<serde_json::Value>(
                "getBlock",
                json!([landed, {"transactionDetails": "signatures", "rewards": false,
                                "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}]),
            )
            .await,
        ) {
            (Some(sig), Ok(b)) => {
                let sigs = b["signatures"].as_array().cloned().unwrap_or_default();
                (
                    sigs.iter().position(|s| s.as_str() == Some(sig)),
                    Some(sigs.len()),
                )
            }
            _ => (None, None),
        };
        row["leader"] = json!(leader);
        row["send_slot_leader"] = json!(send_leader);
        row["leader_tpu"] = json!(addr);
        row["leader_rtt_ms"] = json!(rtt);
        row["leader_region"] = json!(rtt_region(rtt));
        row["block_index"] = json!(idx);
        row["block_tx_count"] = json!(count);
        row["block_position_pct"] = json!(match (idx, count) {
            (Some(i), Some(n)) if n > 0 => Some(100.0 * i as f64 / n as f64),
            _ => None,
        });
    }
}

/// One delivery route: a set of senders that get the same signed bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteSpec {
    pub name: String,
    pub kinds: Vec<SenderKind>,
    /// Jito block engines for this route (empty unless it contains jito).
    pub jito_urls: Vec<String>,
    /// Tip embedded in this route's transactions (lamports).
    pub tip: u64,
    /// Jito tip chosen per order from the tip-floor API (capped by `tip`).
    pub tip_auto: bool,
    /// The Helius sender in this route is Sender Max (not SWQoS-only).
    pub helius_max: bool,
}

pub fn jito_region_url(region: &str) -> String {
    format!("https://{region}.mainnet.block-engine.jito.wtf")
}

/// `"tpu,jito;tpu,helius;tpu,jito@frankfurt+ny"` -> routes. Tip per route:
/// helius >= its SWQoS minimum, jito >= its minimum (default 1,000, or `auto`),
/// none otherwise. Jito and Helius in one route are refused (one tip account set).
pub fn parse_routes(
    text: &str,
    default_jito_urls: &[String],
    tip_arg: Option<u64>,
    tip_auto: bool,
) -> anyhow::Result<Vec<RouteSpec>> {
    let mut out = vec![];
    for raw in text.split(';').map(str::trim).filter(|r| !r.is_empty()) {
        let mut kinds = vec![];
        let mut jito_urls = vec![];
        let mut helius_max = false;
        for tok in raw.split(',').map(|t| t.trim().to_lowercase()) {
            let (name, regions) = match tok.split_once('@') {
                Some((n, r)) => (n.to_string(), Some(r.to_string())),
                None => (tok.clone(), None),
            };
            let k = match name.as_str() {
                "rpc" => SenderKind::Rpc,
                "tpu" => SenderKind::Tpu,
                "jito" => SenderKind::Jito,
                "helius" => SenderKind::Helius,
                "helius-max" => {
                    helius_max = true;
                    SenderKind::Helius
                }
                "" => continue,
                other => bail!(
                    "unknown sender {other} in route `{raw}` (use rpc,tpu,jito,helius,helius-max)"
                ),
            };
            if regions.is_some() && k != SenderKind::Jito {
                bail!("only jito takes @regions (route `{raw}`)");
            }
            if k == SenderKind::Jito {
                jito_urls = match regions {
                    Some(r) => r
                        .split('+')
                        .map(str::trim)
                        .filter(|x| !x.is_empty())
                        .map(jito_region_url)
                        .collect(),
                    None => default_jito_urls.to_vec(),
                };
                if jito_urls.is_empty() {
                    bail!("route `{raw}`: jito needs at least one region");
                }
            }
            if !kinds.contains(&k) {
                kinds.push(k);
            }
        }
        if kinds.is_empty() {
            bail!("empty route `{raw}`");
        }
        let jito = kinds.contains(&SenderKind::Jito);
        let helius = kinds.contains(&SenderKind::Helius);
        if jito && helius {
            bail!("route `{raw}`: jito and helius use different tip accounts; put them in separate routes");
        }
        let tip = if helius && helius_max {
            // Sender Max: its own minimum; a smaller --tip-lamports (meant for
            // the other routes) does not apply here.
            let t = tip_arg
                .unwrap_or(0)
                .max(trader_sender_rpc::HELIUS_MAX_MIN_TIP);
            if t > MAX_TIP_LAMPORTS_SENDER_MAX {
                bail!("route `{raw}`: helius-max tip must be <= {MAX_TIP_LAMPORTS_SENDER_MAX}");
            }
            t
        } else if helius {
            let t = tip_arg.unwrap_or(trader_sender_rpc::HELIUS_SWQOS_MIN_TIP);
            if t < trader_sender_rpc::HELIUS_SWQOS_MIN_TIP {
                bail!(
                    "route `{raw}`: helius (SWQoS-only) needs --tip-lamports >= {}",
                    trader_sender_rpc::HELIUS_SWQOS_MIN_TIP
                );
            }
            t
        } else if jito {
            let t = if tip_auto {
                MAX_TIP_LAMPORTS
            } else {
                tip_arg.unwrap_or(1_000)
            };
            if t < trader_sender_jito::JITO_MIN_TIP_LAMPORTS {
                bail!(
                    "route `{raw}`: jito needs --tip-lamports >= {}",
                    trader_sender_jito::JITO_MIN_TIP_LAMPORTS
                );
            }
            t
        } else {
            0
        };
        if tip > MAX_TIP_LAMPORTS && !helius_max {
            bail!("--tip-lamports must be <= {MAX_TIP_LAMPORTS}");
        }
        out.push(RouteSpec {
            name: raw.to_string(),
            kinds,
            jito_urls,
            tip,
            tip_auto: tip_auto && jito,
            helius_max,
        });
    }
    if out.is_empty() {
        bail!("no route given");
    }
    if out.len() > 4 {
        bail!("at most 4 routes per run");
    }
    Ok(out)
}

/// p75 of recent prioritization fees (micro-lamports/CU) for `keys`, clamped
/// to [1_000, cap]. Falls back to 1_000 when nothing is reported.
async fn auto_cu_price(rpc: &RpcClient, keys: &[Pubkey], cap: u64) -> (u64, serde_json::Value) {
    let ks: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    match rpc
        .call::<Vec<serde_json::Value>>("getRecentPrioritizationFees", json!([ks]))
        .await
    {
        Ok(v) => {
            let mut fees: Vec<u64> = v
                .iter()
                .filter_map(|x| x["prioritizationFee"].as_u64())
                .filter(|f| *f > 0)
                .collect();
            fees.sort_unstable();
            let p = |q: f64| -> Option<u64> {
                (!fees.is_empty()).then(|| fees[((fees.len() - 1) as f64 * q).round() as usize])
            };
            let chosen = p(0.75).unwrap_or(1_000).clamp(1_000, cap);
            (
                chosen,
                json!({"samples": v.len(), "nonzero": fees.len(), "p50": p(0.5), "p75": p(0.75), "chosen": chosen, "cap": cap}),
            )
        }
        Err(e) => (1_000, json!({"error": e.to_string(), "chosen": 1_000})),
    }
}

/// Median landed Jito tip (lamports) from the public tip-floor API, clamped to
/// [JITO_MIN_TIP_LAMPORTS, cap]. Falls back to the minimum on any error.
async fn auto_jito_tip(http: &reqwest::Client, cap: u64) -> (u64, serde_json::Value) {
    let min = trader_sender_jito::JITO_MIN_TIP_LAMPORTS;
    let url = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";
    let res = async {
        let v: serde_json::Value = http.get(url).send().await?.json().await?;
        Ok::<_, reqwest::Error>(v)
    }
    .await;
    match res {
        Ok(v) => {
            let row = if v.is_array() {
                v[0].clone()
            } else {
                v.clone()
            };
            let sol = row["landed_tips_50th_percentile"].as_f64();
            let lamports = sol.map(|x| (x * 1e9).ceil() as u64);
            let chosen = lamports.unwrap_or(min).clamp(min, cap);
            (
                chosen,
                json!({"p50_sol": sol, "p75_sol": row["landed_tips_75th_percentile"].as_f64(), "chosen": chosen, "cap": cap}),
            )
        }
        Err(e) => (
            min,
            json!({"error": e.without_url().to_string(), "chosen": min}),
        ),
    }
}

/// Nearest-rank p50/p95/p99/max over the numeric values found at `path`.
fn stat(
    rows: &[serde_json::Value],
    f: impl Fn(&serde_json::Value) -> Option<f64>,
) -> serde_json::Value {
    let mut v: Vec<f64> = rows.iter().filter_map(&f).collect();
    if v.is_empty() {
        return json!({"n": 0});
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| v[(((v.len() as f64) * p).ceil() as usize).clamp(1, v.len()) - 1];
    json!({"n": v.len(), "p50": q(0.50), "p95": q(0.95), "p99": q(0.99), "max": v[v.len() - 1], "min": v[0]})
}

/// Benchmark v3 summary (ms unless noted). "landed" = first processed
/// signature notification from any provider; sender acks are reported apart
/// from inclusion evidence.
fn summarize(rows: &[serde_json::Value]) -> serde_json::Value {
    let ms = |k: &'static str| move |r: &serde_json::Value| r[k].as_f64().map(|us| us / 1e3);
    let mut by_provider = serde_json::Map::new();
    let names: Vec<String> = rows
        .iter()
        .flat_map(|r| r["providers"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["provider"].as_str().map(str::to_string))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for name in names {
        let pick = |field: &'static str, name: String| {
            move |r: &serde_json::Value| {
                r["providers"]
                    .as_array()?
                    .iter()
                    .find(|p| p["provider"].as_str() == Some(name.as_str()))?[field]
                    .as_f64()
            }
        };
        by_provider.insert(
            name.clone(),
            json!({
                "signature_processed_ms": stat(rows, pick("signature_processed_ms_after_signed", name.clone())),
                "landed_slot_first_seen_ms": stat(rows, pick("landed_slot_first_seen_ms_after_signed", name.clone())),
            }),
        );
    }
    json!({
        "trades": rows.len(),
        "order_to_signed_ms": stat(rows, ms("intent_to_signed_us")),
        "signed_to_first_sender_ack_ms": stat(rows, ms("signed_to_sender_ack_us")),
        "signed_to_first_landed_notification_ms": stat(rows, ms("signed_to_processed_us")),
        "signed_to_shred_seen_ms": stat(rows, |r| r["shred_seen_ms_after_signed"].as_f64()),
        "signed_to_bot_final_ms": stat(rows, ms("signed_to_finalized_us")),
        "slots_to_land": stat(rows, |r| r["slots_to_land"].as_f64()),
        "same_slot_share": same_slot_share(rows),
        "block_position_pct": stat(rows, |r| r["block_position_pct"].as_f64()),
        "fee_lamports": stat(rows, |r| r["fee_lamports"].as_f64()),
        "tip_lamports": stat(rows, |r| r["tip_lamports"].as_f64()),
        "by_provider": by_provider,
        "by_route": group_stats(rows, "route"),
        "by_fee_level": group_stats(rows, "fee_level"),
        "by_leader_region": group_stats(rows, "leader_region"),
    })
}

/// Share of landed trades whose landing slot equals the slot we saw at submit.
fn same_slot_share(rows: &[serde_json::Value]) -> serde_json::Value {
    let v: Vec<u64> = rows
        .iter()
        .filter_map(|r| r["slots_to_land"].as_u64())
        .collect();
    if v.is_empty() {
        return json!(null);
    }
    json!((v.iter().filter(|x| **x == 0).count() as f64 / v.len() as f64 * 1000.0).round() / 1000.0)
}

/// The landing metrics, split by the string value of `key` in each row.
fn group_stats(rows: &[serde_json::Value], key: &str) -> serde_json::Value {
    let groups: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|r| r[key].as_str().map(str::to_string))
        .collect();
    let mut out = serde_json::Map::new();
    for g in groups {
        let sub: Vec<serde_json::Value> = rows
            .iter()
            .filter(|r| r[key].as_str() == Some(g.as_str()))
            .cloned()
            .collect();
        out.insert(
            g,
            json!({
                "trades": sub.len(),
                "not_finalized": sub.iter().filter(|r| r["state"].as_str() != Some("FINALIZED")).count(),
                "signed_to_first_sender_ack_ms": stat(&sub, |r| r["signed_to_sender_ack_us"].as_f64().map(|u| u / 1e3)),
                "signed_to_first_landed_notification_ms": stat(&sub, |r| r["signed_to_processed_us"].as_f64().map(|u| u / 1e3)),
                "signed_to_shred_seen_ms": stat(&sub, |r| r["shred_seen_ms_after_signed"].as_f64()),
                "slots_to_land": stat(&sub, |r| r["slots_to_land"].as_f64()),
                "same_slot_share": same_slot_share(&sub),
                "block_position_pct": stat(&sub, |r| r["block_position_pct"].as_f64()),
            }),
        );
    }
    json!(out)
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
    fn routes_parse_tips_and_regions() {
        let fra = vec![jito_region_url("frankfurt")];
        let r = parse_routes(
            "tpu,jito; tpu,helius ;tpu,jito@frankfurt+ny",
            &fra,
            None,
            false,
        )
        .unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].kinds, vec![SenderKind::Tpu, SenderKind::Jito]);
        assert_eq!(r[0].tip, 1_000);
        assert_eq!(r[0].jito_urls, fra);
        assert_eq!(r[1].tip, trader_sender_rpc::HELIUS_SWQOS_MIN_TIP);
        assert!(r[1].jito_urls.is_empty());
        assert_eq!(
            r[2].jito_urls,
            vec![jito_region_url("frankfurt"), jito_region_url("ny")]
        );
        assert_eq!(
            parse_routes("tpu", &fra, Some(5_000), false).unwrap()[0].tip,
            0
        );
        assert!(parse_routes("tpu,jito,helius", &fra, None, false).is_err());
        assert!(parse_routes("tpu,helius", &fra, Some(1_000), false).is_err());
        assert!(parse_routes("tpu@ny", &fra, None, false).is_err());
        assert!(parse_routes("foo", &fra, None, false).is_err());
        assert!(parse_routes("tpu;tpu;tpu;tpu;tpu", &fra, None, false).is_err());
        let m = parse_routes("helius-max;tpu,helius", &fra, Some(5_000), false).unwrap();
        assert!(m[0].helius_max && !m[1].helius_max);
        assert_eq!(m[0].tip, trader_sender_rpc::HELIUS_MAX_MIN_TIP);
        assert_eq!(m[1].tip, 5_000);
        assert!(parse_routes("helius-max", &fra, Some(2_000_000), false).is_err());
        let auto = parse_routes("tpu,jito;tpu", &fra, None, true).unwrap();
        assert!(auto[0].tip_auto && !auto[1].tip_auto);
    }

    #[test]
    fn summary_percentiles_nearest_rank() {
        let rows: Vec<serde_json::Value> = (1..=20)
            .map(|i| json!({"intent_to_signed_us": i * 1000, "providers": [{"provider": "p0:a", "signature_processed_ms_after_signed": i as f64}]}))
            .collect();
        let s = summarize(&rows);
        assert_eq!(s["order_to_signed_ms"]["n"], 20);
        assert_eq!(s["order_to_signed_ms"]["p50"], 10.0);
        assert_eq!(s["order_to_signed_ms"]["p95"], 19.0);
        assert_eq!(s["order_to_signed_ms"]["p99"], 20.0);
        assert_eq!(
            s["by_provider"]["p0:a"]["signature_processed_ms"]["max"],
            20.0
        );
    }

    #[test]
    fn route_groups_and_same_slot_share() {
        let rows = vec![
            json!({"route": "tpu,jito", "state": "FINALIZED", "slots_to_land": 0, "signed_to_processed_us": 150_000, "leader_region": "near(<20ms)"}),
            json!({"route": "tpu,jito", "state": "FINALIZED", "slots_to_land": 1, "signed_to_processed_us": 250_000, "leader_region": "far(>=60ms)"}),
            json!({"route": "tpu,helius", "state": "EXPIRED", "leader_region": "near(<20ms)"}),
        ];
        let s = summarize(&rows);
        assert_eq!(s["same_slot_share"], 0.5);
        let j = &s["by_route"]["tpu,jito"];
        assert_eq!(j["trades"], 2);
        assert_eq!(j["not_finalized"], 0);
        assert_eq!(j["signed_to_first_landed_notification_ms"]["p50"], 150.0);
        assert_eq!(s["by_route"]["tpu,helius"]["not_finalized"], 1);
        assert_eq!(s["by_leader_region"]["near(<20ms)"]["trades"], 2);
        assert_eq!(rtt_region(Some(5.0)), "near(<20ms)");
        assert_eq!(rtt_region(Some(90.0)), "far(>=60ms)");
        assert_eq!(rtt_region(None), "unknown");
    }

    #[test]
    fn zero_signatures_rejects_bad_layout() {
        assert!(!zero_signatures(&mut []));
        assert!(!zero_signatures(&mut [0u8]));
        assert!(!zero_signatures(&mut [1u8, 1, 2]));
        assert!(!zero_signatures(&mut [0x80u8; 200]));
    }
}
