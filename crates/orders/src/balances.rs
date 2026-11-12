//! Balance sources.
//! - [`RpcBalances`]: one `getMultipleAccounts` round trip at `processed` so a
//!   just-landed fill is visible.
//! - [`StreamBalances`]: same accounts served from a live WebSocket stream
//!   (no round trip); falls back to RPC whenever the stream is not live or an
//!   account is not seeded yet, and warms the stream in the background.

use crate::engine::BalanceSource;
use crate::model::WalletBalances;
use async_trait::async_trait;
use solana_pubkey::Pubkey;
use std::time::Duration;
use trader_domain::{Commitment, Lamports};
use trader_market_data::token::{
    associated_token_address, parse_token_account, SPL_TOKEN_PROGRAM, TOKEN_2022_PROGRAM,
};
use trader_market_data::{AccountData, AccountStream, RpcClient};

fn balance_keys(wallet: &Pubkey, mint: &Pubkey, extra: &[Pubkey]) -> Vec<Pubkey> {
    let mut keys = vec![
        *wallet,
        associated_token_address(wallet, mint, &SPL_TOKEN_PROGRAM),
        associated_token_address(wallet, mint, &TOKEN_2022_PROGRAM),
    ];
    keys.extend_from_slice(extra);
    keys
}

/// Shared interpretation of `[wallet, spl_ata, t22_ata, extra...]`.
fn interpret(
    wallet: &Pubkey,
    mint: &Pubkey,
    slot: u64,
    accts: &[Option<&AccountData>],
) -> Result<WalletBalances, String> {
    let sol = accts[0].map(|a| a.lamports).unwrap_or(0);
    let mut token_raw = None;
    let mut exists = false;
    for a in accts[1..3].iter().flatten() {
        if let Ok(t) = parse_token_account(&a.owner, &a.data) {
            if t.mint == *mint && t.owner == *wallet {
                exists = true;
                if t.is_frozen() {
                    return Err("token account is frozen".into());
                }
                token_raw = Some(token_raw.unwrap_or(0) + t.amount);
            }
        }
    }
    let extra_exists = accts[3..].iter().map(|a| a.is_some()).collect();
    Ok(WalletBalances {
        sol: Lamports(sol),
        token_raw,
        token_account_exists: exists,
        slot,
        extra_exists,
    })
}

pub struct RpcBalances {
    pub rpc: RpcClient,
}

#[async_trait]
impl BalanceSource for RpcBalances {
    async fn balances(
        &self,
        wallet: &Pubkey,
        mint: &Pubkey,
        extra: &[Pubkey],
    ) -> Result<WalletBalances, String> {
        let keys = balance_keys(wallet, mint, extra);
        let (slot, accts) = self
            .rpc
            .get_multiple_accounts(&keys, Commitment::Processed)
            .await
            .map_err(|e| e.to_string())?;
        let refs: Vec<Option<&AccountData>> = accts.iter().map(|a| a.as_ref()).collect();
        interpret(wallet, mint, slot, &refs)
    }
}

pub struct StreamBalances {
    pub rpc: RpcClient,
    pub stream: AccountStream,
    /// Max time since the last slot notification for the stream to count as live.
    pub max_silence: Duration,
}

impl StreamBalances {
    pub fn new(rpc: RpcClient, stream: AccountStream) -> Self {
        Self {
            rpc,
            stream,
            max_silence: Duration::from_millis(1_500),
        }
    }

    /// Subscribe + seed the wallet's balance accounts for `mint` (awaits the warm-up).
    pub async fn prewarm(&self, wallet: &Pubkey, mint: &Pubkey, extra: &[Pubkey]) -> bool {
        let keys = balance_keys(wallet, mint, extra);
        trader_market_data::ws::warm(&self.stream, &self.rpc, &keys, Duration::from_secs(3))
            .await
            .unwrap_or(false)
    }

    /// Served from the stream without I/O, if live and fully seeded.
    pub fn from_stream(
        &self,
        wallet: &Pubkey,
        mint: &Pubkey,
        extra: &[Pubkey],
    ) -> Option<Result<WalletBalances, String>> {
        if !self.stream.is_live(self.max_silence) {
            return None;
        }
        let keys = balance_keys(wallet, mint, extra);
        let entries = self.stream.get_all(&keys)?;
        let slot = entries.iter().map(|e| e.slot).max().unwrap_or(0);
        let refs: Vec<Option<&AccountData>> = entries.iter().map(|e| e.account.as_ref()).collect();
        Some(interpret(wallet, mint, slot, &refs))
    }
}

#[async_trait]
impl BalanceSource for StreamBalances {
    async fn balances(
        &self,
        wallet: &Pubkey,
        mint: &Pubkey,
        extra: &[Pubkey],
    ) -> Result<WalletBalances, String> {
        if let Some(r) = self.from_stream(wallet, mint, extra) {
            return r;
        }
        // Cold or stream not live: answer this request via RPC, warm in background.
        let keys = balance_keys(wallet, mint, extra);
        if !keys.iter().all(|k| self.stream.is_acked(k)) {
            let (stream, rpc, keys) = (self.stream.clone(), self.rpc.clone(), keys.clone());
            tokio::spawn(async move {
                let _ = trader_market_data::ws::warm(&stream, &rpc, &keys, Duration::from_secs(3))
                    .await;
            });
        }
        let (slot, accts) = self
            .rpc
            .get_multiple_accounts(&keys, Commitment::Processed)
            .await
            .map_err(|e| e.to_string())?;
        let refs: Vec<Option<&AccountData>> = accts.iter().map(|a| a.as_ref()).collect();
        interpret(wallet, mint, slot, &refs)
    }
}
