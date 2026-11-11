//! Deterministic mock sender for tests and `mock` mode. Labeled MOCK in every
//! output; can never reach a real network.

use crate::{SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use trader_domain::SenderKind;

#[derive(Clone, Debug)]
pub enum MockBehavior {
    Accept,
    AcceptWithSignature(String),
    Reject(String),
    RateLimited,
    Http5xx,
    Timeout(Duration),
    Delay(Duration),
}

type Hook = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct MockSender {
    kind: SenderKind,
    behavior: Mutex<MockBehavior>,
    received: Mutex<Vec<Vec<u8>>>,
    on_receive: Mutex<Option<Hook>>,
}

impl MockSender {
    pub fn new(kind: SenderKind, behavior: MockBehavior) -> Self {
        Self {
            kind,
            behavior: Mutex::new(behavior),
            received: Mutex::new(vec![]),
            on_receive: Mutex::new(None),
        }
    }
    pub fn set_behavior(&self, b: MockBehavior) {
        *self.behavior.lock() = b;
    }
    pub fn received(&self) -> Vec<Vec<u8>> {
        self.received.lock().clone()
    }
    /// Called with the wire bytes whenever the mock "forwards" (e.g. into a mock ledger).
    pub fn on_receive(&self, f: Hook) {
        *self.on_receive.lock() = Some(f);
    }
}

#[async_trait]
impl TransactionSender for MockSender {
    fn kind(&self) -> SenderKind {
        self.kind
    }
    fn endpoint(&self) -> String {
        format!("MOCK-{}", self.kind.as_str())
    }
    fn tip_policy(&self) -> TipPolicy {
        TipPolicy::None
    }
    async fn submit(&self, wire: &[u8], ctx: &SendContext) -> SendOutcome {
        self.received.lock().push(wire.to_vec());
        let b = self.behavior.lock().clone();
        let forward = || {
            if let Some(h) = self.on_receive.lock().clone() {
                h(wire)
            }
        };
        match b {
            MockBehavior::Accept => {
                forward();
                SendOutcome::Accepted {
                    returned_signature: Some(ctx.signature.clone()),
                }
            }
            MockBehavior::AcceptWithSignature(s) => SendOutcome::Accepted {
                returned_signature: Some(s),
            },
            MockBehavior::Reject(r) => SendOutcome::Rejected { reason: r },
            MockBehavior::RateLimited => SendOutcome::RateLimited,
            MockBehavior::Http5xx => {
                // A 5xx may still have forwarded (ambiguous): model that pessimistically.
                forward();
                SendOutcome::Ambiguous {
                    reason: "http 502".into(),
                }
            }
            MockBehavior::Timeout(d) => {
                forward();
                tokio::time::sleep(d).await;
                SendOutcome::Ambiguous {
                    reason: "timeout".into(),
                }
            }
            MockBehavior::Delay(d) => {
                tokio::time::sleep(d).await;
                forward();
                SendOutcome::Accepted {
                    returned_signature: Some(ctx.signature.clone()),
                }
            }
        }
    }
    async fn health(&self) -> SenderHealth {
        SenderHealth {
            configured: true,
            note: Some("MOCK".into()),
            ..Default::default()
        }
    }
}
