//! trader-cli — developer/operator tooling. Never touches mainnet unless the
//! explicit `--network mainnet --i-understand-mainnet` pair is passed, and
//! even then only read-only commands are allowed (fixture capture).

mod localnet;
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
        _ => {
            println!("{HELP}");
            Ok(())
        }
    }
}
