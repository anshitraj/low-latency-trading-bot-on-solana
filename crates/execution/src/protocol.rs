//! Protocol (DEX) adapter interface. Each adapter owns its market-state cache
//! and must fail closed when state is missing/stale or the market is outside
//! what it explicitly understands.

use crate::guard::ProgramRule;
use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use std::sync::Arc;
use std::time::Duration;
use trader_domain::{DexKind, Lamports, MarketRef, Side};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("market not supported by {adapter}: {reason}")]
    Unsupported { adapter: &'static str, reason: String },
    #[error("market state unavailable: {0}")]
    StateUnavailable(String),
    #[error("market state stale: {0}")]
    Stale(String),
    #[error("quote failed: {0}")]
    Quote(String),
    #[error("trading disabled on this market: {0}")]
    TradingDisabled(String),
    #[error("rpc error: {0}")]
    Rpc(String),
}

#[derive(Clone, Debug)]
pub struct QuoteRequest {
    pub mint: Pubkey,
    pub side: Side,
    /// Buy: lamports in. Sell: raw token units in.
    pub amount_in: u64,
    pub slippage_bps: u16,
    pub max_state_age: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolQuote {
    pub market: MarketRef,
    pub side: Side,
    pub amount_in: u64,
    /// Amount the program actually charges (may be < amount_in on a partial fill).
    pub input_used: u64,
    pub expected_out: u64,
    pub min_out: u64,
    pub max_in: u64,
    pub base_decimals: u8,
    pub base_token_program: Pubkey,
    pub protocol_fee_lamports: Option<Lamports>,
    pub price_impact_bps: Option<u32>,
    pub context_slot: u64,
    pub state_age_ms: u64,
    /// Accounts this trade may create and their rent (refundable on close).
    pub rent_lamports: Lamports,
    /// Estimated compute units (measured + margin; see adapter docs).
    pub compute_units: u32,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SwapBuild {
    pub instructions: Vec<Instruction>,
}

#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    fn kind(&self) -> DexKind;
    fn name(&self) -> &'static str;

    /// Program IDs (and per-program rules) the guard must allow for this adapter.
    fn program_rules(&self) -> Vec<(Pubkey, &'static str, Option<Arc<dyn ProgramRule>>)>;

    /// Resolve which market (if any) this adapter can trade for `mint`, fetching
    /// and caching its state. `Ok(None)` = not this adapter's market.
    async fn resolve_market(&self, mint: &Pubkey) -> Result<Option<MarketRef>, ProtocolError>;

    /// Refresh cached state for a market (background task or on-demand).
    async fn refresh(&self, mint: &Pubkey) -> Result<(), ProtocolError>;

    /// Exact-in quote from cached state only (no I/O). Fails closed on stale state.
    fn quote_exact_in(&self, req: &QuoteRequest) -> Result<ProtocolQuote, ProtocolError>;

    /// Exact-out is optional; default unsupported.
    fn quote_exact_out(&self, _req: &QuoteRequest) -> Result<ProtocolQuote, ProtocolError> {
        Err(ProtocolError::Unsupported { adapter: self.name(), reason: "exact-out not supported".into() })
    }

    /// Accounts this protocol may create *at the user's expense* when trading
    /// `mint` (e.g. Pump's per-user volume accumulator), with their rent. The
    /// engine checks existence in the same RPC round trip as balances and adds
    /// the rent of missing ones to the quoted max spend.
    fn user_rent_candidates(&self, _user: &Pubkey, _mint: &Pubkey, _side: Side) -> Vec<(Pubkey, Lamports)> {
        vec![]
    }

    /// Human-facing market facts from cached state (liquidity etc.), with provenance.
    fn describe(&self, _mint: &Pubkey) -> Option<serde_json::Value> {
        None
    }

    /// Build swap instructions for a quote (no I/O). Thresholds come from the quote.
    fn build_swap(&self, quote: &ProtocolQuote, user: &Pubkey) -> Result<SwapBuild, ProtocolError>;
}
