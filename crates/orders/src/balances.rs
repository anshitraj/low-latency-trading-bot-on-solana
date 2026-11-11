//! RPC-backed balance source (one `getMultipleAccounts` round trip at
//! `processed` so a just-landed fill is visible).

use crate::engine::BalanceSource;
use crate::model::WalletBalances;
use async_trait::async_trait;
use solana_pubkey::Pubkey;
use trader_domain::{Commitment, Lamports};
use trader_market_data::token::{
    associated_token_address, parse_token_account, SPL_TOKEN_PROGRAM, TOKEN_2022_PROGRAM,
};
use trader_market_data::RpcClient;

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
        let mut keys = vec![
            *wallet,
            associated_token_address(wallet, mint, &SPL_TOKEN_PROGRAM),
            associated_token_address(wallet, mint, &TOKEN_2022_PROGRAM),
        ];
        keys.extend_from_slice(extra);
        let (slot, accts) = self
            .rpc
            .get_multiple_accounts(&keys, Commitment::Processed)
            .await
            .map_err(|e| e.to_string())?;
        let sol = accts[0].as_ref().map(|a| a.lamports).unwrap_or(0);
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
}
