//! Slot-stamped, bounded, freshness-checked cache. Quotes fail closed when
//! the entry is missing or older than the caller's max age.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Stamped<V> {
    pub value: V,
    pub slot: u64,
    pub fetched_at: Instant,
}

impl<V> Stamped<V> {
    pub fn age(&self) -> Duration {
        self.fetched_at.elapsed()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Freshness {
    #[error("no cached state")]
    Missing,
    #[error("cached state is stale ({age_ms} ms > {max_ms} ms)")]
    Stale { age_ms: u64, max_ms: u64 },
}

#[derive(Clone)]
pub struct SlotCache<K, V> {
    inner: Arc<RwLock<HashMap<K, Stamped<V>>>>,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> SlotCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            capacity,
        }
    }

    /// Insert unless an entry with a *newer* slot already exists (prevents an
    /// out-of-order RPC response from regressing state).
    pub fn put(&self, k: K, value: V, slot: u64) -> bool {
        let mut m = self.inner.write();
        if let Some(existing) = m.get(&k) {
            if existing.slot > slot {
                return false;
            }
        }
        if m.len() >= self.capacity && !m.contains_key(&k) {
            // Evict the oldest entry by fetch time. O(n) but n is bounded and
            // eviction is off the hot path (refresh tasks).
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, v)| v.fetched_at)
                .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(
            k,
            Stamped {
                value,
                slot,
                fetched_at: Instant::now(),
            },
        );
        true
    }

    pub fn get(&self, k: &K) -> Option<Stamped<V>> {
        self.inner.read().get(k).cloned()
    }

    pub fn get_fresh(&self, k: &K, max_age: Duration) -> Result<Stamped<V>, Freshness> {
        let s = self.get(k).ok_or(Freshness::Missing)?;
        let age = s.age();
        if age > max_age {
            return Err(Freshness::Stale {
                age_ms: age.as_millis() as u64,
                max_ms: max_age.as_millis() as u64,
            });
        }
        Ok(s)
    }

    pub fn invalidate(&self, k: &K) {
        self.inner.write().remove(k);
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn keys(&self) -> Vec<K> {
        self.inner.read().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_and_ordering() {
        let c: SlotCache<u32, &str> = SlotCache::new(2);
        assert_eq!(
            c.get_fresh(&1, Duration::from_secs(1)).unwrap_err(),
            Freshness::Missing
        );
        assert!(c.put(1, "a", 10));
        assert!(!c.put(1, "old", 9), "older slot must not overwrite");
        assert_eq!(c.get(&1).unwrap().value, "a");
        assert!(c.put(1, "b", 10));
        std::thread::sleep(Duration::from_millis(15));
        assert!(matches!(
            c.get_fresh(&1, Duration::from_millis(5)),
            Err(Freshness::Stale { .. })
        ));
        assert!(c.get_fresh(&1, Duration::from_secs(5)).is_ok());
        c.put(2, "x", 1);
        c.put(3, "y", 1);
        assert_eq!(c.len(), 2, "bounded");
        assert!(c.get(&1).is_none(), "oldest evicted");
    }
}
