//! trader-cli — developer/operator tooling. State-changing commands run on a
//! local validator only, except `mainnet-canary`, which is a dry run
//! (simulate only) unless `--send`, MAINNET_TRADING_ENABLED and an interactive
//! confirmation are all present. Fixture capture is read-only.

mod canary;
mod localnet;
mod probe;
mod rpc_probe;
mod shred_feed;
mod smoke;

use std::collections::HashMap;

pub fn args() -> (String, HashMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| "help".into());
    let mut m = HashMap::new();
    let rest: Vec<String> = it.collect();
    let mut i = 0;
    while i < rest.len() {
        if let Some(k) = rest[i].strip_prefix("--") {
            let v = rest.get(i + 1).filter(|v| !v.starts_with("--")).cloned();
            if v.is_some() {
                i += 1;
            }
            m.insert(k.to_string(), v.unwrap_or_else(|| "true".into()));
        }
        i += 1;
    }
    (cmd, m)
}

const HELP: &str = r#"trader-cli <command> [--flags]

  clone-pump-accounts --rpc <devnet-url> --out localnet/accounts
      Fetch the admin-initialized Pump PDAs (+ fee recipients) from devnet and
      write solana-test-validator --account JSON files.
  create-coin --rpc http://127.0.0.1:8899
      Create a fresh Pump coin (create_v2) on a LOCAL validator. Prints the mint.
  capture-fixture --rpc <url> --mint <mint> --out fixtures/pump/<name>.json [--network devnet|mainnet]
      Read-only: capture Global/FeeConfig/BondingCurve/Mint account bytes.
  smoke-pump --rpc http://127.0.0.1:8899 --mint <mint> [--buy-sol 0.05] [--iterations 1] [--report out.json]
      Real engine (PumpAdapter + RpcSender + observer) buy/sell on LOCALNET; verifies balances on-chain.
  keygen [--keypair <path>]
      Create a canary wallet file (default ~/.trader/canary-keypair.json). Prints the pubkey only.
  mainnet-canary --mint <mint> [--buy-lamports 120000] [--iterations 1..25] [--priority-lamports 100000] [--cu-price N|auto] [--tip-lamports N|auto] [--senders rpc,tpu,jito|helius | --routes "tpu,jito;tpu,helius;tpu,jito@frankfurt+ny"] [--tpu-leaders 3] [--jito-regions frankfurt,amsterdam] [--no-slot-updates] [--shred-feed 0.0.0.0:9900] [--received-from <host>] [--poll-ms 400] [--no-ws] [--ws <url>] [--keypair <path>] [--sell-only] [--send]
      MAINNET. Without --send: DRY RUN (full engine path, signature zeroed, simulateTransaction only;
      nothing broadcast). With --send: real buy -> sell-100% round trips; needs RPC_URL (mainnet),
      MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK and typing a confirmation phrase.
      Hard caps: 0.001 SOL per buy, 25 iterations. --senders rpc,tpu,jito|helius (same signed bytes to each);
      jito needs --tip-lamports >= 1000 (Frankfurt block engine, --jito-url); helius >= 5000 (Frankfurt SWQoS-only, --helius-url).
  send-probe [--rpc <mainnet-url>] [--jito-regions frankfurt,amsterdam] [--helius-regions fra,ams] [--leaders 8]
      READ-ONLY: round trips to RPC, Jito, Helius Sender and QUIC handshakes to upcoming leaders. Sends nothing.
  rpc-probe [--rpc <url>] [--public] [--calls 20] [--seconds 30] [--report out.json]
      READ-ONLY: every configured provider (RPC_URL, ~/.trader/rpc_urls, ALCHEMY/ANKR/DRPC/HELIUS_API_KEY,
      QUICKNODE/CHAINSTACK_RPC_URL): getSlot round trips + which WebSocket announces each slot first.

Benchmarks live in the `trader-benchmarks` crate (cargo run -p trader-benchmarks --release -- help).
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let (cmd, a) = args();
    match cmd.as_str() {
        "clone-pump-accounts" => localnet::clone_pump_accounts(&a).await,
        "create-coin" => localnet::create_coin(&a).await,
        "capture-fixture" => localnet::capture_fixture(&a).await,
        "smoke-pump" => smoke::smoke_pump(&a).await,
        "keygen" => canary::keygen(&a).await,
        "mainnet-canary" => canary::mainnet_canary(&a).await,
        "send-probe" => probe::send_probe(&a).await,
        "rpc-probe" => rpc_probe::rpc_probe(&a).await,
        _ => {
            println!("{HELP}");
            Ok(())
        }
    }
}
