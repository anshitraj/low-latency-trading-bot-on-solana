//! Simple token bucket for outbound per-endpoint rate limits (published
//! provider limits are configured, not assumed).

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct TokenBucket {
    inner: Arc<Mutex<(f64, Instant)>>,
    capacity: f64,
    refill_per_sec: f64,
}

impl TokenBucket {
    /// `capacity` requests per `per` duration.
    pub fn new(capacity: u32, per: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new((capacity as f64, Instant::now()))),
            capacity: capacity as f64,
            refill_per_sec: capacity as f64 / per.as_secs_f64(),
        }
    }
    pub fn try_take(&self) -> bool {
        let mut g = self.inner.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(g.1).as_secs_f64();
        g.0 = (g.0 + elapsed * self.refill_per_sec).min(self.capacity);
        g.1 = now;
        if g.0 >= 1.0 {
            g.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket() {
        let b = TokenBucket::new(2, Duration::from_millis(100));
        assert!(b.try_take());
        assert!(b.try_take());
        assert!(!b.try_take());
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.try_take());
    }
}
