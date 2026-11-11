use crate::fees::CostEstimate;
use crate::intent::Side;
use crate::money::{u64_string, Lamports};
use crate::network::Network;
use crate::pubkey::{b58, Pubkey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DexKind {
    PumpBondingCurve,
    PumpSwap,
    RaydiumCpmm,
    Jupiter,
    Mock,
}

impl DexKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DexKind::PumpBondingCurve => "pump_bonding_curve",
            DexKind::PumpSwap => "pump_swap",
            DexKind::RaydiumCpmm => "raydium_cpmm",
            DexKind::Jupiter => "jupiter",
            DexKind::Mock => "mock",
        }
    }
}

/// Where the numbers in a quote came from. Displayed to the user verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuoteSource {
    /// Computed locally from on-chain account state at `slot`.
    OnChain,
    /// Returned by an external routing API; not independently verified.
    ExternalApi { provider: String },
    /// Deterministic mock data. Never shown as real.
    Mock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketRef {
    pub dex: DexKind,
    /// Pool / bonding-curve account.
    #[serde(with = "b58")]
    pub market: Pubkey,
    #[serde(with = "b58")]
    pub base_mint: Pubkey,
    #[serde(with = "b58")]
    pub quote_mint: Pubkey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub id: Uuid,
    pub network: Network,
    pub market: MarketRef,
    pub side: Side,
    /// Exact input amount in base units of the input asset.
    #[serde(with = "u64_string")]
    pub in_amount: u64,
    pub in_decimals: u8,
    /// Expected output at the quoted state, in base units of the output asset.
    #[serde(with = "u64_string")]
    pub expected_out: u64,
    pub out_decimals: u8,
    /// Slippage-protected minimum output passed on-chain.
    #[serde(with = "u64_string")]
    pub min_out: u64,
    /// For DEX instructions that take a max-input instead (Pump buy): the max
    /// quote spent including slippage.
    #[serde(with = "u64_string")]
    pub max_in: u64,
    pub price_impact_bps: Option<u32>,
    pub costs: CostEstimate,
    /// Worst-case total SOL debit from the wallet if the trade executes
    /// (max_in for buys + overhead). Shown as "max spend".
    pub max_total_debit: Lamports,
    /// Slot of the account state used.
    pub context_slot: u64,
    /// Age of that state when the quote was computed.
    pub state_age_ms: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub source: QuoteSource,
    /// Free-text caveats (e.g. "token-2022 transfer fee applied").
    pub warnings: Vec<String>,
}

impl Quote {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}
