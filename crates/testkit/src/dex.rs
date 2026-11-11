//! MOCK constant-product DEX. Instruction layout (program MOCK_DEX_PROGRAM):
//! data = [side u8 (0 buy,1 sell)] [amount_in u64 LE] [min_out u64 LE]
//! accounts = [user (signer, writable), mint (readonly)]
//! Fee: 100 bps of input. Executed by `MockChain`.

use async_trait::async_trait;
use parking_lot::RwLock;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use trader_domain::money::{min_out_after_slippage, price_impact_bps};
use trader_domain::{Bps, DexKind, Lamports, MarketRef, Side};
use trader_execution::guard::{IxView, ProgramRule};
use trader_execution::protocol::{ProtocolAdapter, ProtocolError, ProtocolQuote, QuoteRequest, SwapBuild};
use trader_market_data::token::{NATIVE_MINT, SPL_TOKEN_PROGRAM};

pub const MOCK_DEX_PROGRAM: Pubkey = solana_pubkey::pubkey!("MockDex111111111111111111111111111111111111");
pub const FEE_BPS: u64 = 100;

#[derive(Clone, Copy, Debug)]
pub struct Pool {
    pub sol_reserve: u64,
    pub token_reserve: u64,
    pub decimals: u8,
}

/// out = reserve_out * in_after_fee / (reserve_in + in_after_fee)
pub fn swap_out(pool: &Pool, side: Side, amount_in: u64) -> u64 {
    let in_after_fee = amount_in as u128 * (10_000 - FEE_BPS) as u128 / 10_000;
    let (rin, rout) = match side {
        Side::Buy => (pool.sol_reserve as u128, pool.token_reserve as u128),
        Side::Sell => (pool.token_reserve as u128, pool.sol_reserve as u128),
    };
    if rin == 0 || rout == 0 {
        return 0;
    }
    (rout * in_after_fee / (rin + in_after_fee)) as u64
}

pub fn encode(side: Side, amount_in: u64, min_out: u64) -> Vec<u8> {
    let mut d = Vec::with_capacity(17);
    d.push(if side == Side::Buy { 0 } else { 1 });
    d.extend_from_slice(&amount_in.to_le_bytes());
    d.extend_from_slice(&min_out.to_le_bytes());
    d
}

pub fn decode(data: &[u8]) -> Option<(Side, u64, u64)> {
    if data.len() != 17 {
        return None;
    }
    let side = match data[0] {
        0 => Side::Buy,
        1 => Side::Sell,
        _ => return None,
    };
    Some((side, u64::from_le_bytes(data[1..9].try_into().ok()?), u64::from_le_bytes(data[9..17].try_into().ok()?)))
}

/// Adapter side: reads pool state from a shared map the MockChain also mutates
/// (this models a perfectly fresh state subscription).
#[derive(Clone, Default)]
pub struct MockDex {
    pub pools: Arc<RwLock<HashMap<Pubkey, (Pool, u64, Instant)>>>,
    pub compute_units: u32,
}

impl MockDex {
    pub fn new() -> Self {
        Self { pools: Default::default(), compute_units: 60_000 }
    }
    pub fn add_pool(&self, mint: Pubkey, pool: Pool) {
        self.pools.write().insert(mint, (pool, 1, Instant::now()));
    }
    pub fn pool(&self, mint: &Pubkey) -> Option<Pool> {
        self.pools.read().get(mint).map(|p| p.0)
    }
    pub fn set_stale(&self, mint: &Pubkey, age: std::time::Duration) {
        if let Some(p) = self.pools.write().get_mut(mint) {
            p.2 = Instant::now() - age;
        }
    }
}

struct MockRule;
impl ProgramRule for MockRule {
    fn check(&self, ix: &IxView<'_>, wallet: &Pubkey) -> Result<(), String> {
        decode(ix.data).ok_or("bad mock dex data")?;
        if ix.accounts.first() != Some(wallet) {
            return Err("user must be wallet".into());
        }
        Ok(())
    }
}

#[async_trait]
impl ProtocolAdapter for MockDex {
    fn kind(&self) -> DexKind {
        DexKind::Mock
    }
    fn name(&self) -> &'static str {
        "mock-dex"
    }
    fn program_rules(&self) -> Vec<(Pubkey, &'static str, Option<Arc<dyn ProgramRule>>)> {
        vec![(MOCK_DEX_PROGRAM, "mock-dex", Some(Arc::new(MockRule)))]
    }
    async fn resolve_market(&self, mint: &Pubkey) -> Result<Option<MarketRef>, ProtocolError> {
        Ok(self.pools.read().get(mint).map(|_| MarketRef { dex: DexKind::Mock, market: *mint, base_mint: *mint, quote_mint: NATIVE_MINT }))
    }
    async fn refresh(&self, mint: &Pubkey) -> Result<(), ProtocolError> {
        let mut g = self.pools.write();
        let p = g.get_mut(mint).ok_or_else(|| ProtocolError::StateUnavailable("no pool".into()))?;
        p.2 = Instant::now();
        Ok(())
    }
    fn quote_exact_in(&self, req: &QuoteRequest) -> Result<ProtocolQuote, ProtocolError> {
        let (pool, slot, at) = *self.pools.read().get(&req.mint).ok_or_else(|| ProtocolError::StateUnavailable("no pool".into()))?;
        if at.elapsed() > req.max_state_age {
            return Err(ProtocolError::Stale(format!("{} ms", at.elapsed().as_millis())));
        }
        if pool.sol_reserve == 0 || pool.token_reserve == 0 {
            return Err(ProtocolError::Quote("zero liquidity".into()));
        }
        let out = swap_out(&pool, req.side, req.amount_in);
        if out == 0 {
            return Err(ProtocolError::Quote("zero output".into()));
        }
        let probe = (req.amount_in / 1000).max(1);
        let spot = swap_out(&pool, req.side, probe) as u128 * req.amount_in as u128 / probe as u128;
        let fee = req.amount_in as u128 * FEE_BPS as u128 / 10_000;
        Ok(ProtocolQuote {
            market: MarketRef { dex: DexKind::Mock, market: req.mint, base_mint: req.mint, quote_mint: NATIVE_MINT },
            side: req.side,
            amount_in: req.amount_in,
            input_used: req.amount_in,
            expected_out: out,
            min_out: min_out_after_slippage(out, Bps::new(req.slippage_bps as u32).unwrap()),
            max_in: req.amount_in,
            base_decimals: pool.decimals,
            base_token_program: SPL_TOKEN_PROGRAM,
            protocol_fee_lamports: Some(Lamports(if req.side == Side::Buy { fee as u64 } else { (out as u128 * FEE_BPS as u128 / 10_000) as u64 })),
            price_impact_bps: price_impact_bps(spot, out as u128),
            context_slot: slot,
            state_age_ms: at.elapsed().as_millis() as u64,
            rent_lamports: Lamports(0),
            compute_units: self.compute_units,
            warnings: vec!["MOCK market".into()],
        })
    }
    fn describe(&self, mint: &Pubkey) -> Option<serde_json::Value> {
        let (p, slot, at) = *self.pools.read().get(mint)?;
        Some(serde_json::json!({"dex": "MOCK", "liquidity_sol_lamports": p.sol_reserve.to_string(), "token_reserve": p.token_reserve.to_string(), "decimals": p.decimals, "source": "MOCK pool (not real market data)", "slot": slot, "state_age_ms": at.elapsed().as_millis() as u64}))
    }

    fn build_swap(&self, q: &ProtocolQuote, user: &Pubkey) -> Result<SwapBuild, ProtocolError> {
        Ok(SwapBuild {
            instructions: vec![Instruction::new_with_bytes(
                MOCK_DEX_PROGRAM,
                &encode(q.side, q.amount_in, q.min_out),
                vec![AccountMeta::new(*user, true), AccountMeta::new_readonly(q.market.base_mint, false)],
            )],
        })
    }
}
