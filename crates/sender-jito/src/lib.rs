//! Jito Block Engine `sendTransaction` sender
//! (https://docs.jito.wtf/lowlatencytxnsend/, accessed 2026-11-11):
//! - POST {block_engine}/api/v1/transactions, JSON-RPC `sendTransaction`,
//!   base64 encoding (base58 deprecated).
//! - Jito always sets skip_preflight=true; optional `bundleOnly=true` query
//!   gives revert protection (sent as a single-tx bundle).
//! - Minimum tip 1000 lamports (bundles). Tip accounts from `getTipAccounts`
//!   on /api/v1/bundles; pick one at random.
//! - Default sends no longer need an auth key; optional UUID via `x-jito-auth`.
//! - Exceeding limits returns 429. Limits are configured, not assumed.
//! Bundles (`sendBundle`) are not used: one-tx orders don't need them.

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;
use trader_domain::SenderKind;
use trader_router::{SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender};

pub const JITO_MIN_TIP_LAMPORTS: u64 = 1_000;

pub struct JitoSender {
    http: reqwest::Client,
    base: String,
    bundle_only: bool,
    auth_uuid: Option<String>,
    tip_accounts: Vec<Pubkey>,
}

impl JitoSender {
    pub fn new(base: impl Into<String>, bundle_only: bool, auth_uuid: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self { http, base: base.into().trim_end_matches('/').to_string(), bundle_only, auth_uuid, tip_accounts: vec![] }
    }

    pub fn with_tip_accounts(mut self, accounts: Vec<Pubkey>) -> Self {
        self.tip_accounts = accounts;
        self
    }

    async fn rpc(&self, path: &str, method: &str, params: Value) -> Result<(u16, Value), String> {
        let mut req = self.http.post(format!("{}{}", self.base, path)).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(u) = &self.auth_uuid {
            req = req.header("x-jito-auth", u);
        }
        let resp = req.send().await.map_err(|e| if e.is_timeout() { "timeout".to_string() } else { "transport error".to_string() })?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        Ok((status, v))
    }

    /// Fetch tip accounts via `getTipAccounts`.
    pub async fn fetch_tip_accounts(&self) -> Result<Vec<Pubkey>, String> {
        let (status, v) = self.rpc("/api/v1/bundles", "getTipAccounts", json!([])).await?;
        if status != 200 {
            return Err(format!("http {status}"));
        }
        v["result"]
            .as_array()
            .ok_or("missing result")?
            .iter()
            .map(|a| a.as_str().and_then(|s| Pubkey::from_str(s).ok()).ok_or_else(|| "bad tip account".to_string()))
            .collect()
    }
}

pub fn classify(status: u16, v: &Value) -> SendOutcome {
    if status == 429 {
        return SendOutcome::RateLimited;
    }
    if (500..600).contains(&status) {
        return SendOutcome::Ambiguous { reason: format!("http {status}") };
    }
    if let Some(sig) = v.get("result").and_then(|r| r.as_str()) {
        return SendOutcome::Accepted { returned_signature: Some(sig.to_string()) };
    }
    if let Some(e) = v.get("error") {
        let msg: String = e.get("message").and_then(|m| m.as_str()).unwrap_or("error").chars().take(300).collect();
        if msg.to_ascii_lowercase().contains("rate limit") {
            return SendOutcome::RateLimited;
        }
        // JSON-RPC errors from the block engine are returned before forwarding.
        return SendOutcome::Rejected { reason: msg };
    }
    SendOutcome::Ambiguous { reason: format!("http {status} unexpected body") }
}

#[async_trait]
impl TransactionSender for JitoSender {
    fn kind(&self) -> SenderKind {
        SenderKind::Jito
    }
    fn endpoint(&self) -> String {
        format!("jito:{}", trader_market_data::rpc::url_host(&self.base))
    }
    fn tip_policy(&self) -> TipPolicy {
        if self.bundle_only {
            TipPolicy::Required { min_lamports: JITO_MIN_TIP_LAMPORTS, accounts: self.tip_accounts.clone() }
        } else {
            TipPolicy::Optional { min_lamports: JITO_MIN_TIP_LAMPORTS, accounts: self.tip_accounts.clone() }
        }
    }
    async fn submit(&self, wire: &[u8], _ctx: &SendContext) -> SendOutcome {
        let b64 = base64::engine::general_purpose::STANDARD.encode(wire);
        let path = if self.bundle_only { "/api/v1/transactions?bundleOnly=true" } else { "/api/v1/transactions" };
        match self.rpc(path, "sendTransaction", json!([b64, {"encoding": "base64"}])).await {
            Ok((status, v)) => classify(status, &v),
            Err(e) => SendOutcome::Ambiguous { reason: e },
        }
    }
    async fn health(&self) -> SenderHealth {
        SenderHealth {
            configured: true,
            note: Some(format!("tip accounts loaded: {}", self.tip_accounts.len())),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::post, Json, Router};
    use std::collections::HashMap;

    #[test]
    fn classify_responses() {
        assert_eq!(classify(429, &Value::Null), SendOutcome::RateLimited);
        assert!(matches!(classify(503, &Value::Null), SendOutcome::Ambiguous { .. }));
        assert_eq!(classify(200, &json!({"result":"S"})), SendOutcome::Accepted { returned_signature: Some("S".into()) });
        assert!(matches!(classify(200, &json!({"error":{"message":"bad tx"}})), SendOutcome::Rejected { .. }));
        assert_eq!(classify(200, &json!({"error":{"message":"Rate limit exceeded"}})), SendOutcome::RateLimited);
    }

    #[tokio::test]
    async fn submit_and_tip_accounts_against_mock_block_engine() {
        let tip = Pubkey::new_unique().to_string();
        let tip2 = tip.clone();
        let app = Router::new()
            .route(
                "/api/v1/transactions",
                post(|Query(q): Query<HashMap<String, String>>, Json(b): Json<Value>| async move {
                    assert_eq!(b["method"], "sendTransaction");
                    assert_eq!(b["params"][1]["encoding"], "base64");
                    assert_eq!(q.get("bundleOnly").map(String::as_str), Some("true"));
                    Json(json!({"jsonrpc":"2.0","id":1,"result":"SIG"}))
                }),
            )
            .route("/api/v1/bundles", post(move || { let t = tip2.clone(); async move { Json(json!({"jsonrpc":"2.0","id":1,"result":[t]})) } }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let s = JitoSender::new(url, true, None, Duration::from_secs(2));
        let accts = s.fetch_tip_accounts().await.unwrap();
        assert_eq!(accts[0].to_string(), tip);
        let s = s.with_tip_accounts(accts);
        assert!(matches!(s.tip_policy(), TipPolicy::Required { min_lamports: 1000, .. }));
        let ctx = SendContext { order_id: Default::default(), signature: "SIG".into(), last_valid_block_height: 1, attempt: 0, rebroadcast: false };
        assert_eq!(s.submit(&[9, 9], &ctx).await, SendOutcome::Accepted { returned_signature: Some("SIG".into()) });
    }
}
