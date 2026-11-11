//! Rolling health stats + circuit breaker per sender.

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SenderHealth {
    pub configured: bool,
    pub breaker: Option<BreakerState>,
    pub samples: usize,
    pub success_rate_bps: Option<u32>,
    pub p50_ack_us: Option<u64>,
    pub p95_ack_us: Option<u64>,
    pub p99_ack_us: Option<u64>,
    pub consecutive_failures: u32,
    pub note: Option<String>,
}

struct Inner {
    window: VecDeque<(bool, u64)>,
    cap: usize,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_inflight: bool,
}

#[derive(Clone)]
pub struct HealthTracker {
    inner: Arc<Mutex<Inner>>,
    threshold: u32,
    cooldown: Duration,
}

pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    // nearest-rank
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

impl HealthTracker {
    pub fn new(window: usize, threshold: u32, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                window: VecDeque::with_capacity(window),
                cap: window,
                consecutive_failures: 0,
                opened_at: None,
                half_open_inflight: false,
            })),
            threshold,
            cooldown,
        }
    }

    pub fn state(&self) -> BreakerState {
        let g = self.inner.lock();
        match g.opened_at {
            None => BreakerState::Closed,
            Some(t) if t.elapsed() >= self.cooldown => BreakerState::HalfOpen,
            Some(_) => BreakerState::Open,
        }
    }

    /// Whether a request may go out now (one trial request in half-open).
    pub fn allow_request(&self) -> bool {
        let mut g = self.inner.lock();
        match g.opened_at {
            None => true,
            Some(t) if t.elapsed() >= self.cooldown && !g.half_open_inflight => {
                g.half_open_inflight = true;
                true
            }
            _ => false,
        }
    }

    /// `ok` = endpoint behaved correctly (accepted, or gave a definitive answer).
    pub fn record(&self, ok: bool, latency_us: u64) {
        let mut g = self.inner.lock();
        if g.window.len() == g.cap {
            g.window.pop_front();
        }
        g.window.push_back((ok, latency_us));
        g.half_open_inflight = false;
        if ok {
            g.consecutive_failures = 0;
            g.opened_at = None;
        } else {
            g.consecutive_failures += 1;
            if g.consecutive_failures >= self.threshold {
                g.opened_at = Some(Instant::now());
            }
        }
    }

    pub fn snapshot(&self) -> SenderHealth {
        let g = self.inner.lock();
        let mut lat: Vec<u64> = g.window.iter().filter(|(ok, _)| *ok).map(|(_, l)| *l).collect();
        lat.sort_unstable();
        let n = g.window.len();
        let oks = g.window.iter().filter(|(ok, _)| *ok).count();
        drop(g);
        let state = self.state();
        let g = self.inner.lock();
        SenderHealth {
            configured: true,
            breaker: Some(state),
            samples: n,
            success_rate_bps: if n == 0 { None } else { Some((oks * 10_000 / n) as u32) },
            p50_ack_us: percentile(&lat, 50.0),
            p95_ack_us: percentile(&lat, 95.0),
            p99_ack_us: percentile(&lat, 99.0),
            consecutive_failures: g.consecutive_failures,
            note: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn breaker_cycle() {
        let h = HealthTracker::new(10, 3, Duration::from_millis(20));
        for _ in 0..3 {
            h.record(false, 1);
        }
        assert_eq!(h.state(), BreakerState::Open);
        assert!(!h.allow_request());
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(h.state(), BreakerState::HalfOpen);
        assert!(h.allow_request());
        assert!(!h.allow_request(), "single trial in half-open");
        h.record(true, 5);
        assert_eq!(h.state(), BreakerState::Closed);
    }

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 95.0), Some(95));
        assert_eq!(percentile(&v, 99.0), Some(99));
        assert_eq!(percentile(&[7], 99.0), Some(7));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
