//! trader-domain: pure, dependency-light domain logic.
//!
//! Everything in this crate is deterministic and side-effect free so it can be
//! unit/property tested exhaustively. Amounts are always integers in base units
//! (lamports or raw token units). There is intentionally no `f64` money math.

pub mod fees;
pub mod intent;
pub mod money;
pub mod network;
pub mod policy;
pub mod pnl;
pub mod pubkey;
pub mod quote;
pub mod state;
pub mod trace;

pub use fees::{ComputeBudget, CostEstimate};
pub use intent::{AmountSpec, SenderKind, Side, TradeIntent};
pub use money::{Bps, Lamports, MoneyError, TokenAmount, LAMPORTS_PER_SOL};
pub use network::Network;
pub use policy::{PolicyViolation, UsageSnapshot, WalletPolicy};
pub use quote::{DexKind, MarketRef, Quote, QuoteSource};
pub use state::{user_status, Commitment, OrderState, TransitionError};
pub use trace::{LatencyTrace, Stage};
