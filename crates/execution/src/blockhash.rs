//! Background blockhash refresher. The hot path reads the cached entry and
//! fails closed if it is older than the caller's limit.
//!
//! Per https://solana.com/docs/tools/transaction-landing (accessed
//! 2026-11-11): fetch with `confirmed` commitment (a `processed` blockhash may
//! belong to a fork that gets skipped) and track `lastValidBlockHeight`.

use parking_lot::RwLock;
use solana_hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trader_domain::Commitment;
use trader_market_data::RpcClient;

#[derive(Clone, Copy, Debug)]
pub struct BlockhashEntry {
    pub hash: Hash,
    pub last_valid_block_height: u64,
    pub context_slot: u64,
    pub fetched_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockhashError {
    #[error("no blockhash fetched yet")]
    Missing,
    #[error("blockhash is stale ({0} ms)")]
    Stale(u64),
}

#[derive(Clone)]
pub struct BlockhashService {
    inner: Arc<RwLock<Option<BlockhashEntry>>>,
    block_height: Arc<RwLock<Option<(u64, Instant)>>>,
}

impl Default for BlockhashService {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockhashService {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            block_height: Arc::new(RwLock::new(None)),
        }
    }

    /// Manually set (tests / benchmarks).
    pub fn set(&self, e: BlockhashEntry) {
        *self.inner.write() = Some(e);
    }

    pub fn get(&self, max_age: Duration) -> Result<BlockhashEntry, BlockhashError> {
        let e = (*self.inner.read()).ok_or(BlockhashError::Missing)?;
        let age = e.fetched_at.elapsed();
        if age > max_age {
            return Err(BlockhashError::Stale(age.as_millis() as u64));
        }
        Ok(e)
    }

    pub fn last_block_height(&self) -> Option<(u64, Instant)> {
        *self.block_height.read()
    }

    pub async fn refresh_once(
        &self,
        rpc: &RpcClient,
    ) -> Result<BlockhashEntry, trader_market_data::RpcError> {
        let (hash, lvbh, slot) = rpc.get_latest_blockhash(Commitment::Confirmed).await?;
        let e = BlockhashEntry {
            hash,
            last_valid_block_height: lvbh,
            context_slot: slot,
            fetched_at: Instant::now(),
        };
        let mut w = self.inner.write();
        // Never regress to an older context slot from a lagging response.
        if w.map(|old| old.context_slot <= slot).unwrap_or(true) {
            *w = Some(e);
        }
        Ok(e)
    }

    /// Spawn the refresher. A blockhash is valid for 150 blocks (~31 s at the
    /// ~210 ms mainnet slots measured 2026-11-12); ~400 ms keeps it within ~2 slots.
    pub fn spawn(&self, rpc: RpcClient, interval: Duration) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                if let Err(e) = this.refresh_once(&rpc).await {
                    tracing::warn!(error = %e, "blockhash refresh failed");
                }
                if n.is_multiple_of(4) {
                    if let Ok(h) = rpc.get_block_height(Commitment::Confirmed).await {
                        *this.block_height.write() = Some((h, Instant::now()));
                    }
                }
                n = n.wrapping_add(1);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_fails_closed() {
        let s = BlockhashService::new();
        assert_eq!(
            s.get(Duration::from_secs(1)).unwrap_err(),
            BlockhashError::Missing
        );
        s.set(BlockhashEntry {
            hash: Hash::default(),
            last_valid_block_height: 10,
            context_slot: 1,
            fetched_at: Instant::now() - Duration::from_secs(5),
        });
        assert!(matches!(
            s.get(Duration::from_secs(2)),
            Err(BlockhashError::Stale(_))
        ));
        assert!(s.get(Duration::from_secs(10)).is_ok());
    }
}
