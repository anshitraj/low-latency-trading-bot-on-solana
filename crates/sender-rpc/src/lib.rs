//! JSON-RPC `sendTransaction` sender. Two modes:
//! - `RpcSender::standard`: any Solana RPC (free/public or self-hosted).
//!   `maxRetries=0` because the order engine owns rebroadcasting.
//! - `RpcSender::helius_sender`: Helius Sender (https://www.helius.dev/docs/sending-transactions/sender,
//!   accessed 2026-11-11): requires `skipPreflight=true`, a tip to a designated
//!   tip account (>= 0.001 SOL for "Sender Max"; >= 0.000005 SOL with
//!   `?swqos_only=true`), keyless limited to 1 req/s.

use async_trait::async_trait;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use trader_domain::{Commitment, SenderKind};
use trader_market_data::{RpcClient, RpcError};
use trader_router::{SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender};

/// Designated Helius Sender tip accounts (mainnet-beta), copied from the
/// Helius Sender docs (accessed 2026-11-11). Re-verify before enabling.
pub const HELIUS_TIP_ACCOUNTS: [&str; 10] = [
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
    "2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD",
    "2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ",
    "wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF",
    "3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT",
    "4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey",
    "4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or",
];
pub const HELIUS_MAX_MIN_TIP: u64 = 1_000_000; // 0.001 SOL
pub const HELIUS_SWQOS_MIN_TIP: u64 = 5_000; // 0.000005 SOL

pub struct RpcSender {
    kind: SenderKind,
    rpc: RpcClient,
    skip_preflight: bool,
    tip: TipPolicy,
    label: String,
}

impl RpcSender {
    /// Standard RPC sender. `skip_preflight` should stay false unless the
    /// per-protocol policy explicitly allows it (see docs/EXECUTION_ENGINE.md).
    pub fn standard(rpc: RpcClient, skip_preflight: bool) -> Self {
        let label = format!("rpc:{}", rpc.host());
        Self {
            kind: SenderKind::Rpc,
            rpc,
            skip_preflight,
            tip: TipPolicy::None,
            label,
        }
    }

    pub fn helius_sender(rpc: RpcClient, swqos_only: bool) -> Self {
        let accounts = HELIUS_TIP_ACCOUNTS
            .iter()
            .map(|s| Pubkey::from_str(s).expect("valid tip account"))
            .collect();
        let min = if swqos_only {
            HELIUS_SWQOS_MIN_TIP
        } else {
            HELIUS_MAX_MIN_TIP
        };
        let label = format!("helius-sender:{}", rpc.host());
        Self {
            kind: SenderKind::Helius,
            rpc,
            skip_preflight: true,
            tip: TipPolicy::Required {
                min_lamports: min,
                accounts,
            },
            label,
        }
    }
}

/// Map an RPC error from `sendTransaction` to a send outcome.
pub fn classify(e: &RpcError) -> SendOutcome {
    match e {
        RpcError::Rpc {
            code,
            message,
            data,
        } => {
            let text = format!(
                "{message} {}",
                data.as_ref().map(|d| d.to_string()).unwrap_or_default()
            );
            if text.contains("AlreadyProcessed") || text.contains("already been processed") {
                return SendOutcome::AlreadyProcessed;
            }
            match *code {
                // -32002 preflight simulation failed; -32003 signature verification failed;
                // -32602 invalid params (malformed tx). The node did not forward.
                -32002 | -32003 | -32602 => SendOutcome::Rejected {
                    reason: truncate(&text),
                },
                429 => SendOutcome::RateLimited,
                // -32005 node unhealthy/behind; unknown codes: treat as ambiguous.
                _ => SendOutcome::Ambiguous {
                    reason: truncate(&text),
                },
            }
        }
        e if e.is_rate_limited() => SendOutcome::RateLimited,
        RpcError::Decode(m) => SendOutcome::Ambiguous {
            reason: truncate(m),
        },
        e => SendOutcome::Ambiguous {
            reason: truncate(&e.to_string()),
        },
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

#[async_trait]
impl TransactionSender for RpcSender {
    fn kind(&self) -> SenderKind {
        self.kind
    }
    fn endpoint(&self) -> String {
        self.label.clone()
    }
    fn tip_policy(&self) -> TipPolicy {
        self.tip.clone()
    }
    async fn submit(&self, wire: &[u8], _ctx: &SendContext) -> SendOutcome {
        match self
            .rpc
            .send_transaction(wire, self.skip_preflight, Some(0), Commitment::Confirmed)
            .await
        {
            Ok(sig) => SendOutcome::Accepted {
                returned_signature: Some(sig),
            },
            Err(e) => classify(&e),
        }
    }
    async fn health(&self) -> SenderHealth {
        let note = match self.rpc.get_health().await {
            Ok(s) => Some(s),
            Err(e) => Some(format!("unhealthy: {e}")),
        };
        SenderHealth {
            configured: true,
            note,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use trader_market_data::RpcConfig;

    #[test]
    fn tip_accounts_parse() {
        for a in HELIUS_TIP_ACCOUNTS {
            assert!(Pubkey::from_str(a).is_ok(), "{a}");
        }
    }

    #[test]
    fn classification() {
        let rej = RpcError::Rpc {
            code: -32002,
            message: "Transaction simulation failed: insufficient funds".into(),
            data: None,
        };
        assert!(matches!(classify(&rej), SendOutcome::Rejected { .. }));
        let ap = RpcError::Rpc {
            code: -32002,
            message: "x".into(),
            data: Some(json!({"err":"AlreadyProcessed"})),
        };
        assert_eq!(classify(&ap), SendOutcome::AlreadyProcessed);
        let behind = RpcError::Rpc {
            code: -32005,
            message: "Node is behind".into(),
            data: None,
        };
        assert!(matches!(classify(&behind), SendOutcome::Ambiguous { .. }));
        assert!(matches!(
            classify(&RpcError::Timeout(std::time::Duration::from_secs(1))),
            SendOutcome::Ambiguous { .. }
        ));
        assert_eq!(
            classify(&RpcError::Http {
                status: 429,
                body: "".into()
            }),
            SendOutcome::RateLimited
        );
        assert!(matches!(
            classify(&RpcError::Http {
                status: 503,
                body: "".into()
            }),
            SendOutcome::Ambiguous { .. }
        ));
    }

    #[tokio::test]
    async fn sends_base64_with_max_retries_zero() {
        let app = Router::new().route(
            "/",
            post(|Json(b): Json<Value>| async move {
                assert_eq!(b["method"], "sendTransaction");
                assert_eq!(b["params"][1]["encoding"], "base64");
                assert_eq!(b["params"][1]["maxRetries"], 0);
                assert_eq!(b["params"][1]["skipPreflight"], true);
                assert_eq!(b["params"][0], "AQID");
                Json(json!({"jsonrpc":"2.0","id":1,"result":"SIG"}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let s = RpcSender::helius_sender(RpcClient::new(RpcConfig::new(url)).unwrap(), true);
        let ctx = SendContext {
            order_id: Default::default(),
            signature: "SIG".into(),
            last_valid_block_height: 1,
            attempt: 0,
            rebroadcast: false,
        };
        assert_eq!(
            s.submit(&[1, 2, 3], &ctx).await,
            SendOutcome::Accepted {
                returned_signature: Some("SIG".into())
            }
        );
        assert!(matches!(
            s.tip_policy(),
            TipPolicy::Required {
                min_lamports: HELIUS_SWQOS_MIN_TIP,
                ..
            }
        ));
    }
}
