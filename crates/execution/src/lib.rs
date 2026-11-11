//! Transaction construction for the hot path.
//!
//! Hot path (warm cache): ProtocolAdapter::quote (in-memory) ->
//! ProtocolAdapter::build_swap (in-memory) -> builder::build_and_sign
//! (compile + guard + sign + serialize). No network or DB I/O happens inside
//! these functions; blockhash comes from `BlockhashService`'s cache.

pub mod blockhash;
pub mod builder;
pub mod guard;
pub mod locks;
pub mod protocol;
pub mod risk;

pub use blockhash::{BlockhashEntry, BlockhashError, BlockhashService};
pub use builder::{build_and_sign, verify_wire, BuildError, BuildParams, SignedTx, TipInstruction};
pub use guard::{GuardConfig, GuardError, IxView, ProgramRule, TxGuard};
pub use locks::{Reservation, WalletLocks};
pub use protocol::{ProtocolAdapter, ProtocolError, ProtocolQuote, QuoteRequest, SwapBuild};
pub use risk::{screen_mint, RiskFlag, RiskLevel, RiskReport};
