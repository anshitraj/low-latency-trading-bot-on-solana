//! Direct TPU/QUIC sender.
//!
//! Status: **NOT IMPLEMENTED in the default build.** The intended backend is
//! Triton's maintained `yellowstone-jet-tpu-client` 0.8.1 (Apache-2.0,
//! rust-version 1.91), which manages QUIC connection pools, leader schedule
//! tracking and validator contact discovery. It targets the Solana 3.x crate
//! line while this workspace is pinned to 2.x (see docs/DECISIONS); wire bytes
//! are version-agnostic, so the integration boundary is `&[u8]`.
//!
//! Important: an unstaked client sending QUIC packets to leaders gets no
//! stake-weighted QoS. Without stake or a SWQoS peering agreement this path
//! is best-effort and may be throttled by validators.
//!
//! Until the `jet` feature is implemented and tested, this sender reports
//! itself unavailable and the router excludes it (`NotSent`).

use async_trait::async_trait;
use trader_domain::SenderKind;
use trader_router::{SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender};

pub struct TpuSender;

#[async_trait]
impl TransactionSender for TpuSender {
    fn kind(&self) -> SenderKind {
        SenderKind::Tpu
    }
    fn endpoint(&self) -> String {
        "tpu:not-built".into()
    }
    fn tip_policy(&self) -> TipPolicy {
        TipPolicy::None
    }
    async fn submit(&self, _wire: &[u8], _ctx: &SendContext) -> SendOutcome {
        SendOutcome::NotSent { reason: "TPU sender not built (enable feature `jet`; not implemented yet)".into() }
    }
    async fn health(&self) -> SenderHealth {
        SenderHealth { configured: false, note: Some("not implemented".into()), ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reports_not_sent() {
        let ctx = SendContext { order_id: Default::default(), signature: "S".into(), last_valid_block_height: 0, attempt: 0, rebroadcast: false };
        assert!(matches!(TpuSender.submit(&[1], &ctx).await, SendOutcome::NotSent { .. }));
        assert!(!TpuSender.health().await.configured);
    }
}
