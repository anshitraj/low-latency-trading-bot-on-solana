//! Transaction cost math (base fee, priority fee, tips, rent).
//!
//! Source: https://solana.com/docs/core/fees (accessed 2026-11-11)
//! - base fee: 5000 lamports per signature
//! - prioritization fee = ceil(compute_unit_limit * compute_unit_price_micro_lamports / 1_000_000)
//! - rent-exempt minimum = (128 + data_len) * 3480 lamports/byte-year * 2 years

use crate::money::{Lamports, MoneyError};
use serde::{Deserialize, Serialize};

pub const BASE_FEE_LAMPORTS_PER_SIGNATURE: u64 = 5_000;
pub const MICRO_LAMPORTS_PER_LAMPORT: u128 = 1_000_000;
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
pub const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
pub const LAMPORTS_PER_BYTE_YEAR: u64 = 3_480;
pub const EXEMPTION_THRESHOLD_YEARS: u64 = 2;
/// SPL Token account size.
pub const TOKEN_ACCOUNT_LEN: u64 = 165;
/// Token-2022 account with the ImmutableOwner extension (what the ATA program creates).
pub const TOKEN_2022_ATA_LEN: u64 = 170;

pub fn rent_exempt_minimum(data_len: u64) -> Lamports {
    Lamports((ACCOUNT_STORAGE_OVERHEAD + data_len) * LAMPORTS_PER_BYTE_YEAR * EXEMPTION_THRESHOLD_YEARS)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeBudget {
    pub unit_limit: u32,
    pub unit_price_micro_lamports: u64,
}

impl ComputeBudget {
    pub fn priority_fee(&self) -> Lamports {
        let v = (self.unit_limit as u128 * self.unit_price_micro_lamports as u128)
            .div_ceil(MICRO_LAMPORTS_PER_LAMPORT);
        Lamports(u64::try_from(v).unwrap_or(u64::MAX))
    }

    /// Choose the largest CU price whose resulting priority fee stays within `max_fee`.
    pub fn capped_price(unit_limit: u32, desired_price: u64, max_fee: Lamports) -> u64 {
        if unit_limit == 0 {
            return 0;
        }
        let max_price = (max_fee.0 as u128 * MICRO_LAMPORTS_PER_LAMPORT) / unit_limit as u128;
        desired_price.min(u64::try_from(max_price).unwrap_or(u64::MAX))
    }
}

/// Full, user-visible cost breakdown for a trade. All values in lamports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CostEstimate {
    pub base_fee: Lamports,
    pub priority_fee: Lamports,
    pub tip: Lamports,
    /// Rent for accounts this tx may create (e.g. the user's token ATA). Refundable on close.
    pub rent_deposit: Lamports,
    /// DEX protocol + creator fees charged in SOL, if the DEX charges them in the quote asset.
    pub protocol_fee: Lamports,
}

impl CostEstimate {
    /// Network-side costs paid regardless of trade size or execution success
    /// (a failed tx still pays base + priority; tip depends on sender semantics).
    pub fn network_fees(&self) -> Result<Lamports, MoneyError> {
        self.base_fee.checked_add(self.priority_fee)?.checked_add(self.tip)
    }
    pub fn total_overhead(&self) -> Result<Lamports, MoneyError> {
        self.network_fees()?.checked_add(self.rent_deposit)
    }
}

pub fn base_fee(num_signatures: u64) -> Lamports {
    Lamports(BASE_FEE_LAMPORTS_PER_SIGNATURE * num_signatures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rent_matches_known_values() {
        assert_eq!(rent_exempt_minimum(TOKEN_ACCOUNT_LEN), Lamports(2_039_280));
        assert_eq!(rent_exempt_minimum(0), Lamports(890_880));
        assert_eq!(rent_exempt_minimum(TOKEN_2022_ATA_LEN), Lamports(2_074_080));
    }

    #[test]
    fn priority_fee_ceil() {
        let cb = ComputeBudget { unit_limit: 200_000, unit_price_micro_lamports: 1 };
        assert_eq!(cb.priority_fee(), Lamports(1)); // 0.2 lamports -> ceil 1
        let cb = ComputeBudget { unit_limit: 200_000, unit_price_micro_lamports: 10_000 };
        assert_eq!(cb.priority_fee(), Lamports(2_000));
        let cb = ComputeBudget { unit_limit: 0, unit_price_micro_lamports: 10_000 };
        assert_eq!(cb.priority_fee(), Lamports(0));
        let cb = ComputeBudget { unit_limit: MAX_COMPUTE_UNIT_LIMIT, unit_price_micro_lamports: u64::MAX };
        assert!(cb.priority_fee().0 > 0);
    }

    #[test]
    fn capped_price_respects_cap() {
        let p = ComputeBudget::capped_price(200_000, 1_000_000, Lamports(10_000));
        let cb = ComputeBudget { unit_limit: 200_000, unit_price_micro_lamports: p };
        assert!(cb.priority_fee() <= Lamports(10_000));
        assert_eq!(p, 50_000);
        assert_eq!(ComputeBudget::capped_price(200_000, 10, Lamports(10_000)), 10);
    }

    #[test]
    fn totals() {
        let c = CostEstimate {
            base_fee: base_fee(1),
            priority_fee: Lamports(2_000),
            tip: Lamports(1_000),
            rent_deposit: Lamports(2_039_280),
            protocol_fee: Lamports(0),
        };
        assert_eq!(c.network_fees().unwrap(), Lamports(8_000));
        assert_eq!(c.total_overhead().unwrap(), Lamports(2_047_280));
    }
}
