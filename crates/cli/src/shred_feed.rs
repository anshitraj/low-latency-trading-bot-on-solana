//! Collector for `tools/shred-observer`: receives [64-byte signature][u64 LE
//! slot] records over UDP and remembers when each signature was FIRST seen
//! (local monotonic clock), for ~2 minutes. Read-only; never sends.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RECORD: usize = 72;
const KEEP: Duration = Duration::from_secs(120);

/// signature -> (first local arrival, slot)
type SeenMap = HashMap<[u8; 64], (Instant, u64)>;

#[derive(Clone)]
pub struct ShredFeed {
    seen: Arc<Mutex<SeenMap>>,
    records: Arc<AtomicU64>,
}

impl ShredFeed {
    pub fn spawn(bind: &str) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(bind)?;
        let me = Self {
            seen: Default::default(),
            records: Default::default(),
        };
        let (seen, records) = (me.seen.clone(), me.records.clone());
        std::thread::Builder::new()
            .name("shred-feed".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                let mut last_prune = Instant::now();
                loop {
                    let Ok((n, _)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    let now = Instant::now();
                    let mut g = seen.lock();
                    for rec in buf[..n].chunks_exact(RECORD) {
                        let mut k = [0u8; 64];
                        k.copy_from_slice(&rec[..64]);
                        let slot = u64::from_le_bytes(rec[64..72].try_into().unwrap());
                        g.entry(k).or_insert((now, slot));
                        records.fetch_add(1, Ordering::Relaxed);
                    }
                    if last_prune.elapsed() > Duration::from_secs(5) {
                        g.retain(|_, (t, _)| t.elapsed() < KEEP);
                        last_prune = Instant::now();
                    }
                }
            })?;
        Ok(me)
    }

    /// First time the feed reported `signature` (base58), and its slot.
    pub fn seen(&self, signature: &str) -> Option<(Instant, u64)> {
        let raw = bs58::decode(signature).into_vec().ok()?;
        let k: [u8; 64] = raw.try_into().ok()?;
        self.seen.lock().get(&k).copied()
    }

    pub fn records(&self) -> u64 {
        self.records.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_first_sighting_only() {
        // Pick a free port, then bind the feed to it.
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let feed = ShredFeed::spawn(&format!("127.0.0.1:{port}")).unwrap();
        let sig = [7u8; 64];
        let mut rec = sig.to_vec();
        rec.extend_from_slice(&42u64.to_le_bytes());
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(&rec, ("127.0.0.1", port)).unwrap();
        let b58 = bs58::encode(sig).into_string();
        let t0 = Instant::now();
        let first = loop {
            if let Some(v) = feed.seen(&b58) {
                break v;
            }
            assert!(t0.elapsed() < Duration::from_secs(2), "record not received");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(first.1, 42);
        std::thread::sleep(Duration::from_millis(20));
        let mut rec2 = sig.to_vec();
        rec2.extend_from_slice(&43u64.to_le_bytes());
        tx.send_to(&rec2, ("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(feed.seen(&b58).unwrap(), first, "later sightings ignored");
        assert_eq!(feed.records(), 2);
        assert!(feed.seen("notbase58!!").is_none());
    }
}
