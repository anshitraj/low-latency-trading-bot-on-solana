//! Per-wallet hard limits. These are enforced server-side in the execution
//! engine immediately before signing; UI-side checks are only a convenience.

use crate::intent::{SenderKind, Side, TradeIntent};
use crate::money::{Bps, Lamports};
use crate::quote::Quote;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletPolicy {
    /// Max SOL spent on a single buy (input amount, excluding fees).
    pub max_order: Lamports,
    /// Max sum of buy inputs per UTC day.
    pub max_daily_turnover: Lamports,
    /// SOL that must remain after the worst-case debit (fees, ATA rent, future sells).
    pub min_sol_reserve: Lamports,
    pub max_slippage: Bps,
    pub max_priority_fee: Lamports,
    pub max_tip: Lamports,
    pub max_trades_per_day: u32,
    /// Stop trading after this much realized loss today (P1: fed by positions).
    pub daily_loss_limit: Lamports,
    /// User kill switch.
    pub paused: bool,
    /// Suspicious-activity lock set by risk/abuse systems; only an operator clears it.
    pub locked: bool,
    /// Must be explicitly enabled per wallet *and* globally for mainnet.
    pub mainnet_enabled: bool,
    pub allowed_senders: Vec<SenderKind>,
    /// Max price impact accepted for a trade.
    pub max_price_impact: Bps,
}

impl Default for WalletPolicy {
    fn default() -> Self {
        Self {
            max_order: Lamports(250_000_000),            // 0.25 SOL
            max_daily_turnover: Lamports(1_000_000_000), // 1 SOL
            min_sol_reserve: Lamports(10_000_000),       // 0.01 SOL
            max_slippage: Bps::new(1_500).unwrap(),      // 15%
            max_priority_fee: Lamports(200_000),         // 0.0002 SOL
            max_tip: Lamports(100_000),                  // 0.0001 SOL
            max_trades_per_day: 50,
            daily_loss_limit: Lamports(500_000_000),
            paused: false,
            locked: false,
            mainnet_enabled: false,
            allowed_senders: vec![SenderKind::Rpc, SenderKind::Mock],
            max_price_impact: Bps::new(2_500).unwrap(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub turnover_today: Lamports,
    pub trades_today: u32,
    pub realized_loss_today: Lamports,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PolicyViolation {
    #[error("trading is paused for this wallet")]
    Paused,
    #[error("wallet is locked pending review")]
    Locked,
    #[error("mainnet trading is not enabled for this wallet")]
    MainnetDisabled,
    #[error("order {requested} exceeds per-order max {max}")]
    OrderTooLarge { requested: Lamports, max: Lamports },
    #[error("daily turnover limit reached ({used} + {requested} > {max})")]
    DailyTurnover {
        used: Lamports,
        requested: Lamports,
        max: Lamports,
    },
    #[error("daily trade count limit reached ({max})")]
    DailyTradeCount { max: u32 },
    #[error("daily loss limit reached")]
    DailyLoss,
    #[error("slippage {requested_bps} bps exceeds max {max_bps} bps")]
    Slippage { requested_bps: u16, max_bps: u16 },
    #[error("priority fee cap {requested} exceeds policy max {max}")]
    PriorityFee { requested: Lamports, max: Lamports },
    #[error("tip {requested} exceeds policy max {max}")]
    Tip { requested: Lamports, max: Lamports },
    #[error("sender {sender:?} is not allowed for this wallet")]
    Sender { sender: SenderKind },
    #[error("insufficient SOL: need {needed} (incl. reserve), have {available}")]
    InsufficientSol {
        needed: Lamports,
        available: Lamports,
    },
    #[error("price impact {impact_bps} bps exceeds max {max_bps} bps")]
    PriceImpact { impact_bps: u32, max_bps: u16 },
    #[error("quote expired")]
    QuoteExpired,
    #[error("quote does not match intent: {reason}")]
    QuoteMismatch { reason: String },
}

impl WalletPolicy {
    /// Checks that only depend on the intent (run before quoting).
    pub fn check_intent(
        &self,
        intent: &TradeIntent,
        usage: &UsageSnapshot,
    ) -> Result<(), PolicyViolation> {
        if self.locked {
            return Err(PolicyViolation::Locked);
        }
        if self.paused {
            return Err(PolicyViolation::Paused);
        }
        if intent.network.is_mainnet() && !self.mainnet_enabled {
            return Err(PolicyViolation::MainnetDisabled);
        }
        if intent.slippage > self.max_slippage {
            return Err(PolicyViolation::Slippage {
                requested_bps: intent.slippage.get(),
                max_bps: self.max_slippage.get(),
            });
        }
        if intent.fees.max_priority_fee > self.max_priority_fee {
            return Err(PolicyViolation::PriorityFee {
                requested: intent.fees.max_priority_fee,
                max: self.max_priority_fee,
            });
        }
        if intent.fees.tip > self.max_tip {
            return Err(PolicyViolation::Tip {
                requested: intent.fees.tip,
                max: self.max_tip,
            });
        }
        for s in &intent.senders {
            if !self.allowed_senders.contains(s) {
                return Err(PolicyViolation::Sender { sender: *s });
            }
        }
        if usage.trades_today >= self.max_trades_per_day {
            return Err(PolicyViolation::DailyTradeCount {
                max: self.max_trades_per_day,
            });
        }
        if usage.realized_loss_today >= self.daily_loss_limit {
            return Err(PolicyViolation::DailyLoss);
        }
        if let (Side::Buy, crate::intent::AmountSpec::ExactSolIn { lamports }) =
            (intent.side, &intent.amount)
        {
            self.check_buy_size(*lamports, usage)?;
        }
        Ok(())
    }

    fn check_buy_size(
        &self,
        amount: Lamports,
        usage: &UsageSnapshot,
    ) -> Result<(), PolicyViolation> {
        if amount > self.max_order {
            return Err(PolicyViolation::OrderTooLarge {
                requested: amount,
                max: self.max_order,
            });
        }
        let after = usage.turnover_today.0.saturating_add(amount.0);
        if after > self.max_daily_turnover.0 {
            return Err(PolicyViolation::DailyTurnover {
                used: usage.turnover_today,
                requested: amount,
                max: self.max_daily_turnover,
            });
        }
        Ok(())
    }

    /// Checks against a concrete quote and current wallet balance (run right
    /// before signing, under the per-wallet lock).
    pub fn check_quote(
        &self,
        intent: &TradeIntent,
        quote: &Quote,
        sol_balance: Lamports,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), PolicyViolation> {
        if quote.is_expired(now) {
            return Err(PolicyViolation::QuoteExpired);
        }
        if quote.side != intent.side
            || quote.market.base_mint != intent.mint
            || quote.network != intent.network
        {
            return Err(PolicyViolation::QuoteMismatch {
                reason: "side/mint/network differ".into(),
            });
        }
        if let Some(impact) = quote.price_impact_bps {
            if impact > self.max_price_impact.get() as u32 {
                return Err(PolicyViolation::PriceImpact {
                    impact_bps: impact,
                    max_bps: self.max_price_impact.get(),
                });
            }
        }
        if quote.costs.priority_fee > intent.fees.max_priority_fee
            || quote.costs.priority_fee > self.max_priority_fee
        {
            return Err(PolicyViolation::PriorityFee {
                requested: quote.costs.priority_fee,
                max: self.max_priority_fee,
            });
        }
        if quote.costs.tip > self.max_tip {
            return Err(PolicyViolation::Tip {
                requested: quote.costs.tip,
                max: self.max_tip,
            });
        }
        if intent.side == Side::Buy {
            // max_in is what the program may take (slippage-inclusive); policy
            // caps apply to that worst case, not to the nominal amount.
            if quote.max_in > self.max_order.0 {
                return Err(PolicyViolation::OrderTooLarge {
                    requested: Lamports(quote.max_in),
                    max: self.max_order,
                });
            }
        }
        let needed = quote
            .max_total_debit
            .0
            .saturating_add(self.min_sol_reserve.0);
        if sol_balance.0 < needed {
            return Err(PolicyViolation::InsufficientSol {
                needed: Lamports(needed),
                available: sol_balance,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fees::CostEstimate;
    use crate::intent::{AmountSpec, FeePreference};
    use crate::network::Network;
    use crate::pubkey::Pubkey;
    use crate::quote::{DexKind, MarketRef, QuoteSource};
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn intent(amount: u64) -> TradeIntent {
        TradeIntent {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            wallet_id: Uuid::nil(),
            network: Network::Localnet,
            side: Side::Buy,
            mint: Pubkey::new_from_array([7; 32]),
            amount: AmountSpec::ExactSolIn {
                lamports: Lamports(amount),
            },
            slippage: Bps::new(100).unwrap(),
            fees: FeePreference {
                compute_unit_price_micro_lamports: 1000,
                max_priority_fee: Lamports(50_000),
                tip: Lamports(0),
            },
            senders: vec![SenderKind::Rpc],
            idempotency_key: "k".into(),
            requires_approval: false,
            created_at: Utc::now(),
        }
    }

    fn quote(i: &TradeIntent, max_in: u64) -> Quote {
        Quote {
            id: Uuid::nil(),
            network: i.network,
            market: MarketRef {
                dex: DexKind::Mock,
                market: Pubkey::new_from_array([1; 32]),
                base_mint: i.mint,
                quote_mint: Pubkey::default(),
            },
            side: i.side,
            in_amount: max_in,
            in_decimals: 9,
            expected_out: 1000,
            out_decimals: 6,
            min_out: 990,
            max_in,
            price_impact_bps: Some(50),
            costs: CostEstimate {
                priority_fee: Lamports(20_000),
                ..Default::default()
            },
            max_total_debit: Lamports(max_in + 2_100_000),
            context_slot: 1,
            state_age_ms: 10,
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(10),
            source: QuoteSource::Mock,
            warnings: vec![],
        }
    }

    #[test]
    fn intent_limits() {
        let p = WalletPolicy::default();
        let u = UsageSnapshot::default();
        assert!(p.check_intent(&intent(100_000_000), &u).is_ok());
        assert!(matches!(
            p.check_intent(&intent(300_000_000), &u),
            Err(PolicyViolation::OrderTooLarge { .. })
        ));
        let u2 = UsageSnapshot {
            turnover_today: Lamports(900_000_000),
            ..Default::default()
        };
        assert!(matches!(
            p.check_intent(&intent(200_000_000), &u2),
            Err(PolicyViolation::DailyTurnover { .. })
        ));
        let paused = WalletPolicy {
            paused: true,
            ..p.clone()
        };
        assert_eq!(
            paused.check_intent(&intent(1), &u),
            Err(PolicyViolation::Paused)
        );
        let mut m = intent(1);
        m.network = Network::Mainnet;
        assert_eq!(
            p.check_intent(&m, &u),
            Err(PolicyViolation::MainnetDisabled)
        );
        let mut s = intent(1);
        s.slippage = Bps::new(5_000).unwrap();
        assert!(matches!(
            p.check_intent(&s, &u),
            Err(PolicyViolation::Slippage { .. })
        ));
        let mut j = intent(1);
        j.senders = vec![SenderKind::Jito];
        assert!(matches!(
            p.check_intent(&j, &u),
            Err(PolicyViolation::Sender { .. })
        ));
        let mut t = intent(1);
        t.fees.tip = Lamports(1_000_000);
        assert!(matches!(
            p.check_intent(&t, &u),
            Err(PolicyViolation::Tip { .. })
        ));
        let busy = UsageSnapshot {
            trades_today: 50,
            ..Default::default()
        };
        assert!(matches!(
            p.check_intent(&intent(1), &busy),
            Err(PolicyViolation::DailyTradeCount { .. })
        ));
    }

    #[test]
    fn quote_limits() {
        let p = WalletPolicy::default();
        let i = intent(100_000_000);
        let q = quote(&i, 101_000_000);
        let now = Utc::now();
        assert!(p.check_quote(&i, &q, Lamports(1_000_000_000), now).is_ok());
        // reserve enforced
        assert!(matches!(
            p.check_quote(&i, &q, Lamports(110_000_000), now),
            Err(PolicyViolation::InsufficientSol { .. })
        ));
        // slippage-inclusive max_in over cap
        let q2 = quote(&i, 260_000_000);
        assert!(matches!(
            p.check_quote(&i, &q2, Lamports(10_000_000_000), now),
            Err(PolicyViolation::OrderTooLarge { .. })
        ));
        // expired
        assert_eq!(
            p.check_quote(&i, &q, Lamports(1_000_000_000), now + Duration::seconds(11)),
            Err(PolicyViolation::QuoteExpired)
        );
        // impact
        let mut q3 = quote(&i, 101_000_000);
        q3.price_impact_bps = Some(3_000);
        assert!(matches!(
            p.check_quote(&i, &q3, Lamports(1_000_000_000), now),
            Err(PolicyViolation::PriceImpact { .. })
        ));
        // mismatch
        let mut q4 = quote(&i, 101_000_000);
        q4.market.base_mint = Pubkey::new_from_array([9; 32]);
        assert!(matches!(
            p.check_quote(&i, &q4, Lamports(1_000_000_000), now),
            Err(PolicyViolation::QuoteMismatch { .. })
        ));
        // priority fee above user cap
        let mut q5 = quote(&i, 101_000_000);
        q5.costs.priority_fee = Lamports(60_000);
        assert!(matches!(
            p.check_quote(&i, &q5, Lamports(1_000_000_000), now),
            Err(PolicyViolation::PriorityFee { .. })
        ));
    }
}
