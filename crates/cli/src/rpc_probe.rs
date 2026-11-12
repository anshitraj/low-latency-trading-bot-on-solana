//! `rpc-probe`: READ-ONLY comparison of every configured RPC provider
//! (RPC_URL / --rpc, ~/.trader/rpc_urls, provider env vars; `--public` adds the
//! public mainnet endpoint as a baseline). Sends nothing, needs no wallet.
//! - HTTP: genesis check, then `getSlot` round trips (first call = new connection).
//! - WebSocket: `slotSubscribe` on all providers at once for `--seconds`; for
//!   every slot seen by >= 2 providers, each provider's delay behind the first
//!   one to announce it. A trade's processed notification rides the same feed.
//! - `slotsUpdatesSubscribe` support (short second pass).
//!
//! Providers are named by host only (keys live in the path / query).

use anyhow::anyhow;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use trader_domain::{Commitment, Network};
use trader_market_data::{AccountStream, RpcClient, RpcConfig};

fn stats(v: &mut [f64]) -> serde_json::Value {
    if v.is_empty() {
        return json!(null);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| v[(((v.len() as f64) * p).ceil() as usize).clamp(1, v.len()) - 1];
    json!({"n": v.len(), "min_ms": v[0], "p50_ms": q(0.5), "p95_ms": q(0.95), "max_ms": v[v.len() - 1]})
}

pub async fn rpc_probe(a: &HashMap<String, String>) -> anyhow::Result<()> {
    let primary = a
        .get("rpc")
        .cloned()
        .or_else(|| std::env::var("RPC_URL").ok().filter(|s| !s.is_empty()));
    let mut specs = trader_market_data::load_endpoint_specs(primary.as_deref());
    if a.contains_key("public") {
        specs.extend(trader_market_data::parse_endpoint_lines(
            "https://api.mainnet-beta.solana.com",
        ));
    }
    if specs.is_empty() {
        return Err(anyhow!(
            "no providers: set RPC_URL, ~/.trader/rpc_urls or provider env vars (RPC_AND_ROUTING.md)"
        ));
    }
    let calls: usize = a.get("calls").and_then(|s| s.parse().ok()).unwrap_or(20);
    let seconds: u64 = a.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(30);

    // --- HTTP round trips
    let mut http = vec![];
    let mut live: Vec<(String, trader_market_data::EndpointSpec)> = vec![];
    for (i, sp) in specs.iter().enumerate() {
        let label = format!("p{i}:{}", trader_market_data::rpc::url_host(&sp.http));
        let mut cfg = RpcConfig::new(sp.http.clone());
        cfg.timeout = Duration::from_secs(10);
        let rpc = RpcClient::new(cfg)?;
        let t = Instant::now();
        let g = rpc.get_genesis_hash().await;
        let first_ms = t.elapsed().as_secs_f64() * 1e3;
        match g {
            Ok(g) => {
                if let Err(e) = Network::Mainnet.verify_genesis(&g) {
                    http.push(json!({"provider": label, "error": format!("not mainnet: {e}")}));
                    continue;
                }
            }
            Err(e) => {
                http.push(json!({"provider": label, "error": short(&e.to_string())}));
                continue;
            }
        }
        let mut ms = vec![];
        let mut errors = serde_json::Map::new();
        let mut slot = 0;
        for _ in 0..calls {
            let t = Instant::now();
            match rpc.get_slot(Commitment::Processed).await {
                Ok(s) => {
                    ms.push(t.elapsed().as_secs_f64() * 1e3);
                    slot = s;
                }
                Err(e) => {
                    let k = short(&e.to_string());
                    let n = errors.get(&k).and_then(|v| v.as_u64()).unwrap_or(0);
                    errors.insert(k, json!(n + 1));
                }
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        http.push(json!({
            "provider": label,
            "first_call_ms": first_ms,
            "get_slot": stats(&mut ms),
            "errors": errors,
            "processed_slot": slot,
        }));
        live.push((label, sp.clone()));
    }

    // --- WebSocket slot race (all providers at once)
    let streams: Vec<(String, AccountStream)> = live
        .iter()
        .map(|(l, sp)| (l.clone(), AccountStream::spawn(sp.ws_url())))
        .collect();
    eprintln!(
        "racing slot notifications on {} WebSocket(s) for {seconds} s ...",
        streams.len()
    );
    tokio::time::sleep(Duration::from_secs(3)).await; // connect + first slots
    let start = streams
        .iter()
        .map(|(_, s)| s.last_slot())
        .max()
        .unwrap_or(0);
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let end = streams
        .iter()
        .map(|(_, s)| s.last_slot())
        .max()
        .unwrap_or(0);
    let n = streams.len();
    let mut lags: Vec<Vec<f64>> = vec![vec![]; n];
    let mut firsts = vec![0usize; n];
    let mut missed = vec![0usize; n];
    let mut compared = 0usize;
    if start > 0 {
        // Skip the edges (a slot may be half-announced at either end).
        for slot in (start + 1)..end.saturating_sub(1) {
            let seen: Vec<Option<Instant>> = streams
                .iter()
                .map(|(_, s)| s.slot_first_seen(slot))
                .collect();
            if seen.iter().flatten().count() < 2 {
                continue;
            }
            let t0 = *seen.iter().flatten().min().expect("two seen");
            compared += 1;
            for (i, t) in seen.iter().enumerate() {
                match t {
                    Some(t) => {
                        let lag = t.duration_since(t0).as_secs_f64() * 1e3;
                        if *t == t0 {
                            firsts[i] += 1;
                        }
                        lags[i].push(lag);
                    }
                    None => missed[i] += 1,
                }
            }
        }
    }
    let ws: Vec<serde_json::Value> = streams
        .iter()
        .enumerate()
        .map(|(i, (l, s))| {
            json!({
                "provider": l,
                "connected": s.last_slot() > 0,
                "reconnects": s.reconnects(),
                "first_to_announce": firsts[i],
                "missed_slots": missed[i],
                "lag_behind_first_ms": stats(&mut lags[i]),
            })
        })
        .collect();
    drop(streams);

    // --- slotsUpdatesSubscribe support (short pass, separate connections so the
    // race above compares plain slotNotification only)
    let su: Vec<(String, AccountStream)> = live
        .iter()
        .map(|(l, sp)| {
            let s = AccountStream::spawn(sp.ws_url());
            s.enable_slot_updates();
            (l.clone(), s)
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let slot_updates: serde_json::Map<String, serde_json::Value> = su
        .iter()
        .map(|(l, s)| (l.clone(), json!(s.slot_updates_supported())))
        .collect();

    let out = json!({
        "kind": "rpc_probe",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "host": std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_default(),
        "note": "read-only: no transaction sent. ws lag = slotNotification arrival behind the first provider to announce the same slot.",
        "ws_window_s": seconds,
        "slots_compared": compared,
        "http": http,
        "ws_slot_race": ws,
        "slots_updates_supported": slot_updates,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    let path = a.get("report").cloned().unwrap_or_else(|| {
        format!(
            "benchmarks/results/rpc-probe-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&out)?)?;
    println!("{}", json!({ "report": path }));
    Ok(())
}

/// Error text, trimmed; never contains a URL path or query (keys).
fn short(e: &str) -> String {
    let cleaned: String = e
        .split_whitespace()
        .map(|w| {
            if w.contains("://") {
                trader_market_data::rpc::url_host(w.trim_matches(|c: char| {
                    !c.is_ascii_alphanumeric() && c != ':' && c != '/' && c != '.'
                }))
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    cleaned.chars().take(100).collect()
}
