//! `send-probe`: READ-ONLY measurement of the send hop from this machine.
//! Sends no transaction. Measures:
//! - RPC round trip (`getSlot`, reused connection)
//! - Jito block engine round trip (`getTipAccounts`, 1 call per ~1.1 s per region to
//!   respect the default 1 req/s limit)
//! - Helius Sender regional `/ping`
//! - QUIC handshake time to the TPU of the next distinct mainnet leaders
//!   (throwaway identity; the connection is opened, nothing is written)

use anyhow::anyhow;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use trader_domain::{Commitment, Network};
use trader_market_data::{RpcClient, RpcConfig};

fn stats(v: &mut [f64]) -> serde_json::Value {
    if v.is_empty() {
        return json!(null);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    json!({"n": v.len(), "min_ms": v[0], "p50_ms": p(0.5), "max_ms": v[v.len() - 1]})
}

pub async fn send_probe(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let url = a
        .get("rpc")
        .cloned()
        .or_else(|| std::env::var("RPC_URL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());
    let mut cfg = RpcConfig::new(url);
    cfg.timeout = Duration::from_secs(10);
    let rpc = RpcClient::new(cfg)?;
    let genesis = rpc.get_genesis_hash().await?;
    Network::Mainnet
        .verify_genesis(&genesis)
        .map_err(|e| anyhow!("send-probe measures mainnet: {e}"))?;

    // RPC
    let mut rpc_ms = vec![];
    for _ in 0..10 {
        let t = Instant::now();
        rpc.get_slot(Commitment::Processed).await?;
        rpc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let rpc_first = rpc_ms.remove(0);

    // Jito regions
    let regions = a
        .get("jito-regions")
        .cloned()
        .unwrap_or_else(|| "frankfurt,amsterdam".into());
    let mut jito = serde_json::Map::new();
    for r in regions.split(',').map(str::trim).filter(|r| !r.is_empty()) {
        let base = format!("https://{r}.mainnet.block-engine.jito.wtf");
        let j = trader_sender_jito::JitoSender::new(base, false, None, Duration::from_secs(5));
        let mut ms = vec![];
        let mut err = None;
        for _ in 0..5 {
            let t = Instant::now();
            match j.fetch_tip_accounts().await {
                Ok(_) => ms.push(t.elapsed().as_secs_f64() * 1e3),
                Err(e) => err = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(1_100)).await;
        }
        let first = if ms.is_empty() {
            None
        } else {
            Some(ms.remove(0))
        };
        jito.insert(
            r.to_string(),
            json!({"first_ms": first, "reused": stats(&mut ms), "error": err}),
        );
    }

    // Helius Sender regional ping (keyless)
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let helius_regions = a
        .get("helius-regions")
        .cloned()
        .unwrap_or_else(|| "fra,ams".into());
    let mut helius = serde_json::Map::new();
    for r in helius_regions
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        // Regional Sender endpoints are plain HTTP (low latency, backend use).
        let u = format!("http://{r}-sender.helius-rpc.com/ping");
        let mut ms = vec![];
        let mut err = None;
        for _ in 0..6 {
            let t = Instant::now();
            match http.get(&u).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let _ = resp.bytes().await;
                    ms.push(t.elapsed().as_secs_f64() * 1e3)
                }
                Ok(resp) => err = Some(format!("http {}", resp.status())),
                Err(e) => err = Some(e.without_url().to_string()),
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let first = if ms.is_empty() {
            None
        } else {
            Some(ms.remove(0))
        };
        helius.insert(
            r.to_string(),
            json!({"first_ms": first, "reused": stats(&mut ms), "error": err}),
        );
    }

    // TPU QUIC handshakes to upcoming leaders
    let tracker = trader_sender_tpu::LeaderTracker::new(rpc.clone(), None);
    tracker
        .refresh()
        .await
        .map_err(|e| anyhow!("leader tracker: {e}"))?;
    let conns = trader_sender_tpu::ConnCache::new(Duration::from_secs(2))
        .map_err(|e| anyhow!("quic: {e}"))?;
    let n: usize = a.get("leaders").and_then(|s| s.parse().ok()).unwrap_or(8);
    let mut leaders = vec![];
    let mut hs = vec![];
    for (pk, addr) in tracker.upcoming(400).into_iter().take(n) {
        let r = conns.handshake_time(addr).await;
        let ms = r.as_ref().ok().map(|d| d.as_secs_f64() * 1e3);
        if let Some(m) = ms {
            hs.push(m);
        }
        leaders.push(json!({"leader": pk.to_string(), "tpu_quic": addr.to_string(), "handshake_ms": ms, "error": r.err()}));
    }
    let out = json!({
        "kind": "send_probe",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "host": std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default(),
        "note": "read-only: no transaction sent; QUIC connections opened with a throwaway identity, nothing written",
        "rpc_host": rpc.host(),
        "rpc_getslot": {"first_ms": rpc_first, "reused": stats(&mut rpc_ms)},
        "jito_get_tip_accounts": jito,
        "helius_sender_ping": helius,
        "tpu_known_nodes": tracker.known_nodes(),
        "tpu_quic_handshake": stats(&mut hs),
        "tpu_leaders": leaders,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    let path = a.get("report").cloned().unwrap_or_else(|| {
        format!(
            "benchmarks/results/send-probe-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&out)?)?;
    println!("{}", json!({ "report": path }));
    Ok(())
}
