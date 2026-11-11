//! Per-wallet serialization + in-flight reservations.
//!
//! The lock serializes balance-check -> sign for one wallet. After submission
//! the lock is released but a *reservation* of the worst-case SOL debit (and
//! token amount for sells) stays until the observer resolves the order, so a
//! concurrent second order cannot pass the balance check using funds the
//! first may still spend.

use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub order_id: Uuid,
    pub lamports: u64,
    pub token: Option<(Pubkey, u64)>,
}

#[derive(Default, Clone)]
pub struct WalletLocks {
    locks: Arc<Mutex<HashMap<Pubkey, Arc<AsyncMutex<()>>>>>,
    reservations: Arc<Mutex<HashMap<Pubkey, Vec<Reservation>>>>,
}

impl WalletLocks {
    pub async fn lock(&self, wallet: &Pubkey) -> OwnedMutexGuard<()> {
        let m = {
            let mut g = self.locks.lock();
            g.entry(*wallet).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
        };
        m.lock_owned().await
    }

    pub fn reserve(&self, wallet: &Pubkey, r: Reservation) {
        let mut g = self.reservations.lock();
        let v = g.entry(*wallet).or_default();
        if !v.iter().any(|x| x.order_id == r.order_id) {
            v.push(r);
        }
    }

    pub fn release(&self, wallet: &Pubkey, order_id: Uuid) {
        let mut g = self.reservations.lock();
        if let Some(v) = g.get_mut(wallet) {
            v.retain(|r| r.order_id != order_id);
        }
    }

    pub fn reserved_lamports(&self, wallet: &Pubkey) -> u64 {
        self.reservations.lock().get(wallet).map(|v| v.iter().map(|r| r.lamports).sum()).unwrap_or(0)
    }

    pub fn reserved_tokens(&self, wallet: &Pubkey, mint: &Pubkey) -> u64 {
        self.reservations
            .lock()
            .get(wallet)
            .map(|v| v.iter().filter_map(|r| r.token.filter(|(m, _)| m == mint).map(|(_, a)| a)).sum())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serializes_per_wallet() {
        let locks = WalletLocks::default();
        let w = Pubkey::new_unique();
        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut hs = vec![];
        for _ in 0..32 {
            let (l, i, m) = (locks.clone(), inside.clone(), max_seen.clone());
            hs.push(tokio::spawn(async move {
                let _g = l.lock(&w).await;
                let n = i.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(n, Ordering::SeqCst);
                tokio::task::yield_now().await;
                i.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reservations() {
        let l = WalletLocks::default();
        let w = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let a = Uuid::new_v4();
        l.reserve(&w, Reservation { order_id: a, lamports: 100, token: None });
        l.reserve(&w, Reservation { order_id: a, lamports: 100, token: None }); // idempotent
        l.reserve(&w, Reservation { order_id: Uuid::new_v4(), lamports: 5, token: Some((mint, 7)) });
        assert_eq!(l.reserved_lamports(&w), 105);
        assert_eq!(l.reserved_tokens(&w, &mint), 7);
        l.release(&w, a);
        assert_eq!(l.reserved_lamports(&w), 5);
    }
}
