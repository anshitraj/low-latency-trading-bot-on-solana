//! Sender contract + router.
//!
//! Semantics (master prompt §5):
//! - Every sender receives the SAME signed bytes. The router never re-signs.
//! - `SendOutcome::Accepted` is an acknowledgement, NOT landing.
//! - `Ambiguous` (timeout / transport / 5xx) means "may have been forwarded":
//!   the order must be treated as possibly in flight.
//! - Only `Rejected` is definitive (e.g. preflight failure returned before
//!   forwarding). An order may be marked REJECTED only if *every* attempt was
//!   definitively rejected.

pub mod health;
pub mod limiter;
pub mod mock;
pub mod tips;

use async_trait::async_trait;
use futures::future::join_all;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trader_domain::SenderKind;
use uuid::Uuid;

pub use health::{BreakerState, HealthTracker, SenderHealth};
pub use limiter::TokenBucket;
pub use tips::{plan_tip, TipPlan, TipPlanError, TipPolicy};

#[derive(Clone, Debug)]
pub struct SendContext {
    pub order_id: Uuid,
    /// Base58 signature computed locally at signing time.
    pub signature: String,
    pub last_valid_block_height: u64,
    pub attempt: u32,
    /// True for rebroadcasts of an already-submitted tx.
    pub rebroadcast: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SendOutcome {
    /// Endpoint acknowledged. `returned_signature` must equal ctx.signature.
    Accepted {
        returned_signature: Option<String>,
    },
    /// Endpoint says it already has/processed this signature (counts as accepted).
    AlreadyProcessed,
    /// Definitive refusal before forwarding (e.g. preflight simulation error).
    Rejected {
        reason: String,
    },
    RateLimited,
    /// Timeout, transport or 5xx: may or may not have been forwarded.
    Ambiguous {
        reason: String,
    },
    /// Circuit open / not configured / local rate limit; nothing was sent.
    NotSent {
        reason: String,
    },
}

impl SendOutcome {
    pub fn possibly_forwarded(&self) -> bool {
        matches!(
            self,
            SendOutcome::Accepted { .. }
                | SendOutcome::AlreadyProcessed
                | SendOutcome::Ambiguous { .. }
        )
    }
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            SendOutcome::Accepted { .. } | SendOutcome::AlreadyProcessed
        )
    }
    pub fn code(&self) -> &'static str {
        match self {
            SendOutcome::Accepted { .. } => "accepted",
            SendOutcome::AlreadyProcessed => "already_processed",
            SendOutcome::Rejected { .. } => "rejected",
            SendOutcome::RateLimited => "rate_limited",
            SendOutcome::Ambiguous { .. } => "ambiguous",
            SendOutcome::NotSent { .. } => "not_sent",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SendAttempt {
    pub sender: SenderKind,
    pub endpoint: String,
    pub wall_start: chrono::DateTime<chrono::Utc>,
    /// Monotonic offset (µs) of request start relative to the router call.
    pub start_offset_us: u64,
    pub latency_us: u64,
    pub outcome: SendOutcome,
}

#[async_trait]
pub trait TransactionSender: Send + Sync {
    fn kind(&self) -> SenderKind;
    /// Host/region label only (never a URL with credentials).
    fn endpoint(&self) -> String;
    fn tip_policy(&self) -> TipPolicy;
    /// Submit pre-signed wire bytes. Must not panic; must classify every failure.
    async fn submit(&self, signed_tx: &[u8], ctx: &SendContext) -> SendOutcome;
    async fn health(&self) -> SenderHealth;
}

/// Wraps a sender with health tracking, breaker and rate limiter.
pub struct ManagedSender {
    pub inner: Arc<dyn TransactionSender>,
    pub health: HealthTracker,
    pub limiter: Option<TokenBucket>,
    pub timeout: Duration,
}

impl ManagedSender {
    pub fn new(
        inner: Arc<dyn TransactionSender>,
        timeout: Duration,
        limiter: Option<TokenBucket>,
    ) -> Self {
        Self {
            inner,
            health: HealthTracker::new(200, 5, Duration::from_secs(10)),
            limiter,
            timeout,
        }
    }

    async fn submit_tracked(&self, wire: &[u8], ctx: &SendContext, origin: Instant) -> SendAttempt {
        let wall_start = chrono::Utc::now();
        let start = Instant::now();
        let outcome = if !self.health.allow_request() {
            SendOutcome::NotSent {
                reason: "circuit open".into(),
            }
        } else if self
            .limiter
            .as_ref()
            .map(|l| !l.try_take())
            .unwrap_or(false)
        {
            SendOutcome::NotSent {
                reason: "local rate limit".into(),
            }
        } else {
            match tokio::time::timeout(self.timeout, self.inner.submit(wire, ctx)).await {
                Ok(o) => o,
                Err(_) => SendOutcome::Ambiguous {
                    reason: format!("router timeout {:?}", self.timeout),
                },
            }
        };
        // Signature mismatch from an endpoint is treated as ambiguous + unhealthy.
        let outcome = match outcome {
            SendOutcome::Accepted {
                returned_signature: Some(s),
            } if s != ctx.signature => SendOutcome::Ambiguous {
                reason: format!("endpoint returned different signature {s}"),
            },
            o => o,
        };
        let latency_us = start.elapsed().as_micros() as u64;
        if !matches!(outcome, SendOutcome::NotSent { .. }) {
            self.health.record(
                outcome.is_success() || matches!(outcome, SendOutcome::Rejected { .. }),
                latency_us,
            );
        }
        SendAttempt {
            sender: self.inner.kind(),
            endpoint: self.inner.endpoint(),
            wall_start,
            start_offset_us: start.saturating_duration_since(origin).as_micros() as u64,
            latency_us,
            outcome,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    #[error("no requested sender is configured/enabled: {0:?}")]
    NoSender(Vec<SenderKind>),
    #[error(transparent)]
    Tip(#[from] TipPlanError),
}

#[derive(Clone)]
pub struct RoutePlan {
    pub senders: Vec<Arc<ManagedSender>>,
    pub tip: Option<TipPlan>,
}

impl RoutePlan {
    pub fn kinds(&self) -> Vec<SenderKind> {
        self.senders.iter().map(|s| s.inner.kind()).collect()
    }
}

#[derive(Default, Clone)]
pub struct Router {
    senders: Vec<Arc<ManagedSender>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, s: ManagedSender) {
        self.senders.push(Arc::new(s));
    }
    pub fn get(&self, k: SenderKind) -> Option<Arc<ManagedSender>> {
        self.senders.iter().find(|s| s.inner.kind() == k).cloned()
    }
    pub fn all(&self) -> &[Arc<ManagedSender>] {
        &self.senders
    }

    /// Deterministic score: lower is better. Open breaker -> excluded. Missing
    /// stats -> neutral score (safe fallback when no data).
    fn score(s: &ManagedSender) -> Option<u64> {
        if s.health.state() == BreakerState::Open {
            return None;
        }
        let st = s.health.snapshot();
        let p50 = st.p50_ack_us.unwrap_or(50_000);
        let fail_penalty =
            (10_000u64.saturating_sub(st.success_rate_bps.unwrap_or(10_000) as u64)) * 100;
        Some(p50 + fail_penalty)
    }

    /// Choose senders for an order. Requested kinds are filtered to configured
    /// and healthy ones, then ordered by score; the tip plan is derived from
    /// the chosen set and must fit the user's cap.
    pub fn plan(
        &self,
        requested: &[SenderKind],
        desired_tip: u64,
        tip_cap: u64,
    ) -> Result<RoutePlan, RouteError> {
        let mut chosen: Vec<(u64, Arc<ManagedSender>)> = self
            .senders
            .iter()
            .filter(|s| requested.contains(&s.inner.kind()))
            .filter_map(|s| Self::score(s).map(|sc| (sc, s.clone())))
            .collect();
        chosen.sort_by_key(|(sc, s)| (*sc, s.inner.kind()));
        if chosen.is_empty() {
            return Err(RouteError::NoSender(requested.to_vec()));
        }
        let senders: Vec<Arc<ManagedSender>> = chosen.into_iter().map(|(_, s)| s).collect();
        let policies: Vec<(SenderKind, TipPolicy)> = senders
            .iter()
            .map(|s| (s.inner.kind(), s.inner.tip_policy()))
            .collect();
        let tip = plan_tip(&policies, desired_tip, tip_cap)?;
        Ok(RoutePlan { senders, tip })
    }

    /// Send the same bytes to every sender in the plan concurrently.
    pub async fn fan_out(plan: &RoutePlan, wire: &[u8], ctx: &SendContext) -> Vec<SendAttempt> {
        let origin = Instant::now();
        join_all(
            plan.senders
                .iter()
                .map(|s| s.submit_tracked(wire, ctx, origin)),
        )
        .await
    }
}

/// Summarize a set of attempts into the order-level decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanOutVerdict {
    /// At least one endpoint may have forwarded it -> SUBMITTED.
    PossiblyInFlight,
    /// Every attempt was a definitive rejection or not sent -> REJECTED is safe
    /// only if nothing was ever possibly forwarded for this signature before.
    NothingForwarded,
}

pub fn verdict(attempts: &[SendAttempt]) -> FanOutVerdict {
    if attempts.iter().any(|a| a.outcome.possibly_forwarded()) {
        FanOutVerdict::PossiblyInFlight
    } else {
        FanOutVerdict::NothingForwarded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockBehavior, MockSender};

    fn ctx() -> SendContext {
        SendContext {
            order_id: Uuid::nil(),
            signature: "SIG".into(),
            last_valid_block_height: 10,
            attempt: 0,
            rebroadcast: false,
        }
    }

    #[tokio::test]
    async fn fan_out_same_bytes_and_verdicts() {
        let a = Arc::new(MockSender::new(SenderKind::Rpc, MockBehavior::Accept));
        let b = Arc::new(MockSender::new(
            SenderKind::Jito,
            MockBehavior::Timeout(Duration::from_millis(200)),
        ));
        let mut r = Router::new();
        r.add(ManagedSender::new(
            a.clone(),
            Duration::from_millis(50),
            None,
        ));
        r.add(ManagedSender::new(
            b.clone(),
            Duration::from_millis(50),
            None,
        ));
        let plan = r.plan(&[SenderKind::Rpc, SenderKind::Jito], 0, 0).unwrap();
        let wire = vec![1u8, 2, 3];
        let at = Router::fan_out(&plan, &wire, &ctx()).await;
        assert_eq!(at.len(), 2);
        assert_eq!(a.received(), vec![wire.clone()]);
        assert_eq!(
            b.received(),
            vec![wire.clone()],
            "same bytes to every sender"
        );
        let jito = at.iter().find(|x| x.sender == SenderKind::Jito).unwrap();
        assert!(matches!(jito.outcome, SendOutcome::Ambiguous { .. }));
        assert_eq!(verdict(&at), FanOutVerdict::PossiblyInFlight);
    }

    #[tokio::test]
    async fn all_rejected_means_nothing_forwarded() {
        let a = Arc::new(MockSender::new(
            SenderKind::Rpc,
            MockBehavior::Reject("preflight: insufficient funds".into()),
        ));
        let mut r = Router::new();
        r.add(ManagedSender::new(a, Duration::from_millis(50), None));
        let plan = r.plan(&[SenderKind::Rpc], 0, 0).unwrap();
        let at = Router::fan_out(&plan, &[0], &ctx()).await;
        assert_eq!(verdict(&at), FanOutVerdict::NothingForwarded);
        // timeouts are never "nothing forwarded"
        let t = Arc::new(MockSender::new(SenderKind::Rpc, MockBehavior::Http5xx));
        let mut r = Router::new();
        r.add(ManagedSender::new(t, Duration::from_millis(50), None));
        let plan = r.plan(&[SenderKind::Rpc], 0, 0).unwrap();
        assert_eq!(
            verdict(&Router::fan_out(&plan, &[0], &ctx()).await),
            FanOutVerdict::PossiblyInFlight
        );
    }

    #[tokio::test]
    async fn signature_mismatch_is_ambiguous() {
        let a = Arc::new(MockSender::new(
            SenderKind::Rpc,
            MockBehavior::AcceptWithSignature("OTHER".into()),
        ));
        let mut r = Router::new();
        r.add(ManagedSender::new(a, Duration::from_millis(50), None));
        let plan = r.plan(&[SenderKind::Rpc], 0, 0).unwrap();
        let at = Router::fan_out(&plan, &[0], &ctx()).await;
        assert!(matches!(at[0].outcome, SendOutcome::Ambiguous { .. }));
    }

    #[tokio::test]
    async fn breaker_excludes_and_plan_falls_back() {
        let bad = Arc::new(MockSender::new(SenderKind::Jito, MockBehavior::Http5xx));
        let good = Arc::new(MockSender::new(SenderKind::Rpc, MockBehavior::Accept));
        let mut r = Router::new();
        r.add(ManagedSender::new(bad, Duration::from_millis(50), None));
        r.add(ManagedSender::new(good, Duration::from_millis(50), None));
        for _ in 0..5 {
            let plan = r.plan(&[SenderKind::Jito], 0, 0).unwrap();
            Router::fan_out(&plan, &[0], &ctx()).await;
        }
        assert!(matches!(
            r.plan(&[SenderKind::Jito], 0, 0),
            Err(RouteError::NoSender(_))
        ));
        let p = r.plan(&[SenderKind::Jito, SenderKind::Rpc], 0, 0).unwrap();
        assert_eq!(p.kinds(), vec![SenderKind::Rpc]);
        assert!(matches!(
            r.plan(&[SenderKind::Tpu], 0, 0),
            Err(RouteError::NoSender(_))
        ));
    }

    #[tokio::test]
    async fn rate_limited_locally_is_not_sent() {
        let a = Arc::new(MockSender::new(SenderKind::Jito, MockBehavior::Accept));
        let mut r = Router::new();
        r.add(ManagedSender::new(
            a.clone(),
            Duration::from_millis(50),
            Some(TokenBucket::new(1, Duration::from_secs(60))),
        ));
        let plan = r.plan(&[SenderKind::Jito], 0, 0).unwrap();
        let first = Router::fan_out(&plan, &[0], &ctx()).await;
        let second = Router::fan_out(&plan, &[0], &ctx()).await;
        assert!(first[0].outcome.is_success());
        assert!(matches!(second[0].outcome, SendOutcome::NotSent { .. }));
        assert_eq!(a.received().len(), 1);
    }
}
