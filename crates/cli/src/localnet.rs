use anyhow::{anyhow, bail, Context};
use base64::Engine;
use pump_rust_client::accounts::decode_global;
use pump_rust_client::{constants, pda, PumpSdk};
use serde_json::json;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_keypair::Keypair;
use solana_message::{v0, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use trader_domain::{Commitment, Network};
use trader_market_data::token::{associated_token_address, parse_token_account, TOKEN_2022_PROGRAM};
use trader_market_data::{AccountData, RpcClient, RpcConfig};

pub fn rpc(a: &HashMap<String, String>) -> anyhow::Result<RpcClient> {
    let url = a.get("rpc").cloned().unwrap_or_else(|| "http://127.0.0.1:8899".into());
    let mut c = RpcConfig::new(url);
    c.timeout = Duration::from_secs(20);
    Ok(RpcClient::new(c)?)
}

/// Refuse anything but a local validator for state-changing commands.
pub async fn require_localnet(rpc: &RpcClient) -> anyhow::Result<()> {
    let g = rpc.get_genesis_hash().await?;
    Network::Localnet.verify_genesis(&g).map_err(|e| anyhow!("refusing: {e}"))
}

fn account_json(key: &Pubkey, a: &AccountData) -> serde_json::Value {
    json!({
        "pubkey": key.to_string(),
        "account": {
            "lamports": a.lamports,
            "data": [base64::engine::general_purpose::STANDARD.encode(&a.data), "base64"],
            "owner": a.owner.to_string(),
            "executable": a.executable,
            "rentEpoch": 0,
            "space": a.data.len()
        }
    })
}

pub async fn clone_pump_accounts(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let rpc = rpc(a)?;
    let out = a.get("out").cloned().unwrap_or_else(|| "localnet/accounts".into());
    std::fs::create_dir_all(&out)?;
    let genesis = rpc.get_genesis_hash().await?;
    if Network::Devnet.verify_genesis(&genesis).is_err() {
        bail!("clone-pump-accounts expects a DEVNET rpc (genesis {genesis})");
    }
    let fixed: Vec<(&str, Pubkey, bool)> = vec![
        ("pump_global", pda::pump::global().0, true),
        ("pump_event_authority", pda::pump::event_authority().0, false),
        ("pump_mint_authority", pda::pump::mint_authority().0, false),
        ("pump_global_volume_accumulator", pda::pump::global_volume_accumulator().0, false),
        ("pump_fee_config", pda::pump::fee_config().0, true),
        ("pump_quote_control", pda::pump::quote_control().0, false),
        ("amm_global_config", pda::pump_amm::global_config().0, false),
        ("amm_event_authority", pda::pump_amm::event_authority().0, false),
        ("amm_global_volume_accumulator", pda::pump_amm::global_volume_accumulator().0, false),
        ("amm_fee_config", pda::pump_amm::fee_config().0, false),
        ("agent_payments_global_config", pda::pump_agent_payments::global_config().0, false),
        ("mayhem_global_params", pda::mayhem::global_params().0, true),
        ("mayhem_sol_vault", pda::mayhem::sol_vault().0, true),
    ];
    let keys: Vec<Pubkey> = fixed.iter().map(|f| f.1).collect();
    let (slot, accts) = rpc.get_multiple_accounts(&keys, Commitment::Confirmed).await?;
    let mut written = 0;
    let mut global_data = None;
    for ((name, key, required), acct) in fixed.iter().zip(accts.iter()) {
        match acct {
            Some(acc) => {
                if *name == "pump_global" {
                    global_data = Some(acc.data.clone());
                }
                std::fs::write(format!("{out}/{name}.json"), serde_json::to_vec_pretty(&account_json(key, acc))?)?;
                written += 1;
            }
            None if *required => bail!("required account {name} ({key}) missing on devnet"),
            None => eprintln!("note: optional {name} ({key}) not found; skipped"),
        }
    }
    // Fee recipients must exist on localnet or small fee transfers fail the rent check.
    let g = decode_global(&global_data.ok_or_else(|| anyhow!("no global"))?).map_err(|e| anyhow!("{e}"))?;
    let mut recips: Vec<Pubkey> = vec![g.fee_recipient, g.reserved_fee_recipient];
    recips.extend(g.fee_recipients.iter().copied());
    recips.extend(g.buyback_fee_recipients.iter().copied());
    recips.extend(g.reserved_fee_recipients.iter().copied());
    recips.retain(|p| *p != Pubkey::default());
    recips.sort();
    recips.dedup();
    let mut recip_json = vec![];
    for chunk in recips.chunks(100) {
        let (_, rs) = rpc.get_multiple_accounts(chunk, Commitment::Confirmed).await?;
        for (k, r) in chunk.iter().zip(rs) {
            // Write a funded system account regardless of devnet state.
            let acc = r.unwrap_or(AccountData { lamports: 0, owner: Pubkey::default(), data: vec![], executable: false });
            let acc = AccountData { lamports: acc.lamports.max(1_000_000_000), ..acc };
            recip_json.push(k.to_string());
            std::fs::write(format!("{out}/recipient_{k}.json"), serde_json::to_vec_pretty(&account_json(k, &acc))?)?;
            written += 1;
        }
    }
    std::fs::write(
        format!("{out}/manifest.json"),
        serde_json::to_vec_pretty(&json!({"source":"devnet","slot":slot,"fetched_at":chrono::Utc::now().to_rfc3339(),"accounts":written,"fee_recipients":recip_json}))?,
    )?;
    println!("wrote {written} account files to {out} (devnet slot {slot})");
    Ok(())
}

pub async fn wait_confirmed(rpc: &RpcClient, sig: &str, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let (_, st) = rpc.get_signature_statuses(&[sig.to_string()], true).await?;
        if let Some(Some(s)) = st.into_iter().next() {
            if let Some(e) = s.err {
                bail!("transaction failed: {e}");
            }
            if s.confirmation_status >= Some(Commitment::Confirmed) {
                return Ok(());
            }
        }
        if start.elapsed() > timeout {
            bail!("timeout waiting for {sig}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn airdrop(rpc: &RpcClient, to: &Pubkey, lamports: u64) -> anyhow::Result<()> {
    let sig = rpc.request_airdrop(to, lamports).await.context("airdrop")?;
    wait_confirmed(rpc, &sig, Duration::from_secs(30)).await
}

pub async fn create_coin(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let rpc = rpc(a)?;
    require_localnet(&rpc).await?;
    let creator = Keypair::new();
    let mint = Keypair::new();
    airdrop(&rpc, &creator.pubkey(), 10_000_000_000).await?;
    let sdk = PumpSdk::new();
    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        sdk.create_v2_instruction(
            mint.pubkey(),
            creator.pubkey(),
            "LocalTest",
            "LTEST",
            "https://example.invalid/ltest.json",
            creator.pubkey(),
            Pubkey::default(),
            constants::SPL_TOKEN_PROGRAM_ID,
            false,
            false,
            0,
        ),
    ];
    let (bh, _, _) = rpc.get_latest_blockhash(Commitment::Confirmed).await?;
    let msg = VersionedMessage::V0(v0::Message::try_compile(&creator.pubkey(), &ixs, &[], bh)?);
    let bytes = msg.serialize();
    let tx = VersionedTransaction { signatures: vec![creator.sign_message(&bytes), mint.sign_message(&bytes)], message: msg };
    let wire = bincode::serialize(&tx)?;
    let sig = rpc.send_transaction(&wire, false, Some(3), Commitment::Confirmed).await.context("create_v2 send")?;
    wait_confirmed(&rpc, &sig, Duration::from_secs(30)).await?;
    println!("{}", json!({"mint": mint.pubkey().to_string(), "creator": creator.pubkey().to_string(), "signature": sig}));
    Ok(())
}

pub async fn capture_fixture(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let rpc = rpc(a)?;
    let mint = Pubkey::from_str(a.get("mint").ok_or_else(|| anyhow!("--mint required"))?)?;
    let out = a.get("out").cloned().ok_or_else(|| anyhow!("--out required"))?;
    let genesis = rpc.get_genesis_hash().await?;
    let bc = pda::pump::bonding_curve(&mint).0;
    let keys = [pda::pump::global().0, pda::pump::fee_config().0, bc, mint, associated_token_address(&bc, &mint, &TOKEN_2022_PROGRAM)];
    let (slot, accts) = rpc.get_multiple_accounts(&keys, Commitment::Confirmed).await?;
    let names = ["global", "fee_config", "bonding_curve", "mint", "curve_base_ata"];
    let mut m = serde_json::Map::new();
    for ((n, k), acc) in names.iter().zip(keys.iter()).zip(accts.iter()) {
        let acc = acc.as_ref().ok_or_else(|| anyhow!("{n} ({k}) missing"))?;
        m.insert(n.to_string(), account_json(k, acc));
    }
    let ata_amount = accts[4].as_ref().and_then(|a| parse_token_account(&a.owner, &a.data).ok()).map(|t| t.amount).unwrap_or(0);
    let doc = json!({"mint": mint.to_string(), "genesis": genesis, "slot": slot, "captured_at": chrono::Utc::now().to_rfc3339(), "curve_base_ata_amount": ata_amount.to_string(), "accounts": m});
    if let Some(dir) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&doc)?)?;
    println!("captured fixture at slot {slot} -> {out}");
    Ok(())
}
