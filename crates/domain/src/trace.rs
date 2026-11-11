//! Latency tracing with a monotonic clock. Durations are only meaningful
//! between stamps taken on the same host. Wall-clock start is recorded
//! separately for cross-host correlation (requires NTP-synced hosts).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    WebhookReceived,
    AuthOk,
    IntentAccepted,
    QuoteReady,
    TxBuilt,
    Signed,
    SendStart,
    SenderAck,
    FirstObservedProcessed,
    Confirmed,
    Finalized,
}

impl Stage {
    pub const ALL: [Stage; 11] = [
        Stage::WebhookReceived,
        Stage::AuthOk,
        Stage::IntentAccepted,
        Stage::QuoteReady,
        Stage::TxBuilt,
        Stage::Signed,
        Stage::SendStart,
        Stage::SenderAck,
        Stage::FirstObservedProcessed,
        Stage::Confirmed,
        Stage::Finalized,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::WebhookReceived => "t_webhook_received",
            Stage::AuthOk => "t_auth_ok",
            Stage::IntentAccepted => "t_intent_accepted",
            Stage::QuoteReady => "t_quote_ready",
            Stage::TxBuilt => "t_tx_built",
            Stage::Signed => "t_signed",
            Stage::SendStart => "t_send_start",
            Stage::SenderAck => "t_sender_ack",
            Stage::FirstObservedProcessed => "t_first_observed_processed",
            Stage::Confirmed => "t_confirmed",
            Stage::Finalized => "t_finalized",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LatencyTrace {
    pub origin: Instant,
    pub wall_start: DateTime<Utc>,
    stamps: Vec<(Stage, Instant)>,
}

impl Default for LatencyTrace {
    fn default() -> Self {
        Self::start()
    }
}

impl LatencyTrace {
    pub fn start() -> Self {
        Self {
            origin: Instant::now(),
            wall_start: Utc::now(),
            stamps: Vec::with_capacity(12),
        }
    }
    pub fn starting_at(origin: Instant, wall_start: DateTime<Utc>) -> Self {
        Self {
            origin,
            wall_start,
            stamps: Vec::with_capacity(12),
        }
    }
    /// Records the first occurrence of a stage only.
    pub fn mark(&mut self, stage: Stage) {
        self.mark_at(stage, Instant::now());
    }
    pub fn mark_at(&mut self, stage: Stage, at: Instant) {
        if !self.stamps.iter().any(|(s, _)| *s == stage) {
            self.stamps.push((stage, at));
        }
    }
    /// Microseconds since origin, if stamped.
    pub fn us(&self, stage: Stage) -> Option<u64> {
        self.stamps
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, t)| t.saturating_duration_since(self.origin).as_micros() as u64)
    }
    pub fn between_us(&self, a: Stage, b: Stage) -> Option<u64> {
        Some(self.us(b)?.saturating_sub(self.us(a)?))
    }
    pub fn to_map(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert("wall_start".into(), self.wall_start.to_rfc3339().into());
        for s in Stage::ALL {
            if let Some(us) = self.us(s) {
                m.insert(format!("{}_us", s.as_str()), us.into());
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marks_first_only_and_orders() {
        let mut t = LatencyTrace::start();
        t.mark(Stage::AuthOk);
        std::thread::sleep(std::time::Duration::from_millis(2));
        t.mark(Stage::Signed);
        let first = t.us(Stage::Signed).unwrap();
        t.mark(Stage::Signed);
        assert_eq!(t.us(Stage::Signed).unwrap(), first);
        assert!(t.between_us(Stage::AuthOk, Stage::Signed).unwrap() >= 1_000);
        assert!(t.us(Stage::Finalized).is_none());
        assert!(t.to_map().contains_key("t_signed_us"));
    }
}
