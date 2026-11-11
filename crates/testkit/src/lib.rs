//! MOCK infrastructure for deterministic tests and `TRADER_MODE=mock`.
//! Everything here is labeled MOCK and cannot touch a real cluster.

pub mod chain;
pub mod dex;

pub use chain::{ChainConfig, Fault, MockChain};
pub use dex::{MockDex, MOCK_DEX_PROGRAM};
