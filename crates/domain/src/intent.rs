use crate::money::{u64_string, Bps, Lamports};
use crate::network::Network;
use crate::pubkey::{b58, Pubkey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// How much to trade. Each variant is exact-input; exact-output is only offered
/// where the adapter supports it (see DexAdapter::quote_exact_out).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AmountSpec {
    /// Spend exactly this much SOL (before fees) on a buy.
    ExactSolIn { lamports: Lamports },
    /// Sell a percentage (bps) of the wallet's *spendable* token balance at quote time.
    SellPercent { bps: Bps },
    /// Sell exactly this many raw token units.
    ExactTokenIn {
        #[serde(with = "u64_string")]
        raw: u64,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    Rpc,
    Jito,
    Helius,
    Tpu,
    Mock,
}

impl SenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderKind::Rpc => "rpc",
            SenderKind::Jito => "jito",
            SenderKind::Helius => "helius",
            SenderKind::Tpu => "tpu",
            SenderKind::Mock => "mock",
        }
    }
}

/// Fee preferences the user requested; always capped by WalletPolicy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeePreference {
    pub compute_unit_price_micro_lamports: u64,
    pub max_priority_fee: Lamports,
    pub tip: Lamports,
}

impl Default for FeePreference {
    fn default() -> Self {
        Self {
            compute_unit_price_micro_lamports: 10_000,
            max_priority_fee: Lamports(100_000),
            tip: Lamports(0),
        }
    }
}

/// A validated, authenticated request to trade. Created exactly once per
/// (user_id, idempotency_key); any retry with the same key returns the original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeIntent {
    pub id: Uuid,
    pub user_id: Uuid,
    pub wallet_id: Uuid,
    pub network: Network,
    pub side: Side,
    #[serde(with = "b58")]
    pub mint: Pubkey,
    pub amount: AmountSpec,
    pub slippage: Bps,
    pub fees: FeePreference,
    pub senders: Vec<SenderKind>,
    pub idempotency_key: String,
    pub requires_approval: bool,
    pub created_at: DateTime<Utc>,
}

pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

pub fn validate_idempotency_key(k: &str) -> Result<(), &'static str> {
    if k.is_empty() || k.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err("idempotency key must be 1..=128 bytes");
    }
    if !k
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
    {
        return Err("idempotency key has invalid characters");
    }
    Ok(())
}

impl TradeIntent {
    pub fn validate_shape(&self) -> Result<(), String> {
        validate_idempotency_key(&self.idempotency_key).map_err(str::to_string)?;
        match (&self.side, &self.amount) {
            (Side::Buy, AmountSpec::ExactSolIn { lamports }) if lamports.0 > 0 => {}
            (Side::Buy, AmountSpec::ExactSolIn { .. }) => {
                return Err("buy amount must be > 0".into())
            }
            (Side::Buy, _) => return Err("buy requires exact_sol_in".into()),
            (Side::Sell, AmountSpec::SellPercent { bps }) if bps.get() > 0 => {}
            (Side::Sell, AmountSpec::SellPercent { .. }) => {
                return Err("sell percent must be > 0".into())
            }
            (Side::Sell, AmountSpec::ExactTokenIn { raw }) if *raw > 0 => {}
            (Side::Sell, AmountSpec::ExactTokenIn { .. }) => {
                return Err("sell amount must be > 0".into())
            }
            (Side::Sell, AmountSpec::ExactSolIn { .. }) => {
                return Err("sell requires sell_percent or exact_token_in".into())
            }
        }
        if self.senders.is_empty() {
            return Err("at least one sender required".into());
        }
        let mut s = self.senders.clone();
        s.sort();
        s.dedup();
        if s.len() != self.senders.len() {
            return Err("duplicate sender".into());
        }
        if self.network == Network::Mainnet && self.senders.contains(&SenderKind::Mock) {
            return Err("mock sender not allowed on mainnet".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> TradeIntent {
        TradeIntent {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            wallet_id: Uuid::nil(),
            network: Network::Localnet,
            side: Side::Buy,
            mint: Pubkey::new_unique(),
            amount: AmountSpec::ExactSolIn {
                lamports: Lamports(50_000_000),
            },
            slippage: Bps::new(100).unwrap(),
            fees: FeePreference::default(),
            senders: vec![SenderKind::Rpc],
            idempotency_key: "tg:cb:abc123".into(),
            requires_approval: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn shape_validation() {
        let i = sample();
        assert!(i.validate_shape().is_ok());

        let mut bad = sample();
        bad.side = Side::Sell;
        assert!(bad.validate_shape().is_err());

        let mut bad = sample();
        bad.amount = AmountSpec::ExactSolIn {
            lamports: Lamports(0),
        };
        assert!(bad.validate_shape().is_err());

        let mut bad = sample();
        bad.idempotency_key = "has space".into();
        assert!(bad.validate_shape().is_err());

        let mut bad = sample();
        bad.senders = vec![SenderKind::Rpc, SenderKind::Rpc];
        assert!(bad.validate_shape().is_err());

        let mut bad = sample();
        bad.network = Network::Mainnet;
        bad.senders = vec![SenderKind::Mock];
        assert!(bad.validate_shape().is_err());

        let mut ok = sample();
        ok.side = Side::Sell;
        ok.amount = AmountSpec::SellPercent {
            bps: Bps::new(10_000).unwrap(),
        };
        assert!(ok.validate_shape().is_ok());
    }

    #[test]
    fn json_roundtrip_uses_strings() {
        let i = sample();
        let j = serde_json::to_value(&i).unwrap();
        assert_eq!(j["amount"]["lamports"], "50000000");
        assert!(j["mint"].is_string());
        let back: TradeIntent = serde_json::from_value(j).unwrap();
        assert_eq!(back, i);
    }
}
