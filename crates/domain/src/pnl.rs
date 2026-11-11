//! Position accounting: **weighted-average cost basis** (documented in
//! docs/DATABASE.md). All amounts integer; SOL in lamports, tokens raw.
//!
//! - Buy: qty += tokens_received; cost += lamports_debited (swap input +
//!   protocol fee + network fee + tip; ATA rent tracked separately as
//!   refundable, not a cost).
//! - Sell: cost_removed = cost * qty_sold / qty (all cost when selling all);
//!   realized += lamports_credited_net - cost_removed.
//! - Unrealized needs an executable price source; without one it is `None`
//!   (never a fabricated number).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub qty_raw: u128,
    pub cost_basis_lamports: u128,
    pub realized_pnl_lamports: i128,
    pub fees_paid_lamports: u128,
    pub rent_locked_lamports: u128,
    pub gross_bought_lamports: u128,
    pub gross_sold_lamports: u128,
    pub buys: u32,
    pub sells: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PnlError {
    #[error("sell quantity {sell} exceeds position {have}")]
    Oversell { sell: u128, have: u128 },
    #[error("zero quantity fill")]
    ZeroQty,
}

/// One on-chain-verified fill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub qty_raw: u128,
    /// For buys: total lamports that left the wallet for this trade, excluding rent.
    /// For sells: net lamports that arrived (after network fee / tip).
    pub lamports: u128,
    pub fees_lamports: u128,
    pub rent_lamports: u128,
}

impl Position {
    pub fn apply_buy(&mut self, f: &Fill) -> Result<(), PnlError> {
        if f.qty_raw == 0 {
            return Err(PnlError::ZeroQty);
        }
        self.qty_raw += f.qty_raw;
        self.cost_basis_lamports += f.lamports;
        self.gross_bought_lamports += f.lamports;
        self.fees_paid_lamports += f.fees_lamports;
        self.rent_locked_lamports += f.rent_lamports;
        self.buys += 1;
        Ok(())
    }

    /// Returns realized PnL of this sell.
    pub fn apply_sell(&mut self, f: &Fill) -> Result<i128, PnlError> {
        if f.qty_raw == 0 {
            return Err(PnlError::ZeroQty);
        }
        if f.qty_raw > self.qty_raw {
            return Err(PnlError::Oversell { sell: f.qty_raw, have: self.qty_raw });
        }
        let cost_removed = if f.qty_raw == self.qty_raw {
            self.cost_basis_lamports
        } else {
            self.cost_basis_lamports * f.qty_raw / self.qty_raw
        };
        self.qty_raw -= f.qty_raw;
        self.cost_basis_lamports -= cost_removed;
        let realized = f.lamports as i128 - cost_removed as i128;
        self.realized_pnl_lamports += realized;
        self.gross_sold_lamports += f.lamports;
        self.fees_paid_lamports += f.fees_lamports;
        self.sells += 1;
        Ok(realized)
    }

    /// Average cost in lamports per `10^decimals` raw units (i.e. per whole token), floor.
    pub fn avg_cost_per_token(&self, decimals: u8) -> Option<u128> {
        if self.qty_raw == 0 {
            return None;
        }
        Some(self.cost_basis_lamports * 10u128.pow(decimals as u32) / self.qty_raw)
    }

    /// Unrealized PnL given an *executable* estimate of what selling the full
    /// position would return now (e.g. a sell quote). None if no estimate.
    pub fn unrealized(&self, est_full_exit_lamports: Option<u128>) -> Option<i128> {
        let v = est_full_exit_lamports?;
        Some(v as i128 - self.cost_basis_lamports as i128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fill(q: u128, l: u128) -> Fill {
        Fill { qty_raw: q, lamports: l, fees_lamports: 5_000, rent_lamports: 0 }
    }

    #[test]
    fn weighted_average_example() {
        let mut p = Position::default();
        p.apply_buy(&fill(1_000, 100_000)).unwrap();
        p.apply_buy(&fill(1_000, 300_000)).unwrap();
        assert_eq!(p.avg_cost_per_token(0), Some(200));
        let r = p.apply_sell(&fill(500, 150_000)).unwrap();
        assert_eq!(r, 150_000 - 100_000);
        assert_eq!(p.qty_raw, 1_500);
        assert_eq!(p.cost_basis_lamports, 300_000);
        let r2 = p.apply_sell(&fill(1_500, 200_000)).unwrap();
        assert_eq!(r2, 200_000 - 300_000);
        assert_eq!(p.qty_raw, 0);
        assert_eq!(p.cost_basis_lamports, 0);
        assert_eq!(p.realized_pnl_lamports, 50_000 - 100_000);
        assert_eq!(p.fees_paid_lamports, 20_000);
        assert_eq!(p.unrealized(None), None);
    }

    #[test]
    fn oversell_rejected() {
        let mut p = Position::default();
        p.apply_buy(&fill(10, 10)).unwrap();
        assert!(p.apply_sell(&fill(11, 1)).is_err());
        assert!(p.apply_buy(&fill(0, 1)).is_err());
    }

    proptest! {
        /// Selling everything (in any number of partial sells) removes exactly
        /// the total cost: realized == total proceeds - total cost. No drift.
        #[test]
        fn prop_no_cost_drift(buys in proptest::collection::vec((1u64..1_000_000, 1u64..1_000_000_000), 1..8),
                              splits in proptest::collection::vec(1u64..100, 1..8),
                              proceeds in 0u64..1_000_000_000) {
            let mut p = Position::default();
            let mut total_cost = 0u128;
            for (q, l) in &buys {
                p.apply_buy(&Fill { qty_raw: *q as u128, lamports: *l as u128, fees_lamports: 0, rent_lamports: 0 }).unwrap();
                total_cost += *l as u128;
            }
            let mut total_proceeds = 0i128;
            for s in &splits {
                if p.qty_raw == 0 { break; }
                let q = (p.qty_raw * *s as u128 / 100).max(1);
                p.apply_sell(&Fill { qty_raw: q, lamports: proceeds as u128, fees_lamports: 0, rent_lamports: 0 }).unwrap();
                total_proceeds += proceeds as i128;
            }
            if p.qty_raw > 0 {
                let q = p.qty_raw;
                p.apply_sell(&Fill { qty_raw: q, lamports: proceeds as u128, fees_lamports: 0, rent_lamports: 0 }).unwrap();
                total_proceeds += proceeds as i128;
            }
            prop_assert_eq!(p.cost_basis_lamports, 0);
            prop_assert_eq!(p.realized_pnl_lamports, total_proceeds - total_cost as i128);
        }
    }
}
