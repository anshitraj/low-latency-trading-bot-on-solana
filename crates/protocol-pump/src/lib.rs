//! Pump bonding-curve adapter.
//!
//! Built on the official `pump-rust-client` 0.5.0 (MIT) which is generated
//! from the current IDLs in https://github.com/pump-fun/pump-public-docs and
//! implements the on-chain fee resolution (pump-fees `FeeConfig` tiers by
//! market cap, per-coin creator fee). We do NOT re-implement fee math.
//!
//! Instructions: `buy_exact_quote_in_v3` (spend exact SOL, min tokens out) and
//! `sell_v3` (exact tokens in, min SOL out) — the 17-account v3 interface.
//!
//! Supported: SOL-quoted, non-mayhem curves that are not `complete`.
//! Fail-closed (Unsupported): migrated curves (PumpSwap adapter not enabled),
//! non-SOL quote mints, mayhem-mode coins, legacy cashback coins (v3 refuses
//! them), and mints failing the risk screen.

use async_trait::async_trait;
use parking_lot::RwLock;
use pump_rust_client::accounts::{decode_bonding_curve, decode_fee_config, decode_global};
use pump_rust_client::math::fees::compute_bonding_curve_fee_bps;
use pump_rust_client::state::{BondingCurve, FeeConfig, Global};
use pump_rust_client::{constants, pda, PumpSdk, TransferFee};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use trader_domain::{money::price_impact_bps, DexKind, Lamports, MarketRef, Side};
use trader_execution::guard::{IxView, ProgramRule};
use trader_execution::protocol::{ProtocolAdapter, ProtocolError, ProtocolQuote, QuoteRequest, SwapBuild};
use trader_execution::risk::screen_mint;
use trader_market_data::token::{associated_token_address, parse_mint, parse_token_account, MintInfo, SPL_TOKEN_PROGRAM, TOKEN_2022_PROGRAM};
use trader_market_data::{RpcClient, TokenBalanceChange};
use trader_domain::Commitment;

pub const PUMP_PROGRAM_ID: Pubkey = constants::pump::PROGRAM_ID;
pub const PUMP_FEE_PROGRAM_ID: Pubkey = constants::FEE_PROGRAM_ID;

/// Anchor discriminators from idls/pump.json (pump-rust-client 0.5.0).
pub const DISC_BUY_V3: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
pub const DISC_BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
pub const DISC_SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
/// Position of the `user` signer in v3 trade accounts (IDL order).
pub const V3_USER_INDEX: usize = 8;
/// Compute units: measured on localnet (see docs/BENCHMARKS.md) plus margin.
pub const DEFAULT_COMPUTE_UNITS: u32 = 150_000;
/// Data length of the pump `UserVolumeAccumulator` account (observed on localnet).
pub const USER_VOLUME_ACCUMULATOR_LEN: u64 = 137;
/// Small reference trade used to estimate spot price for impact display.
const SPOT_PROBE_LAMPORTS: u64 = 1_000_000;

#[derive(Clone)]
pub struct CurveSnapshot {
    pub global: Global,
    pub fee_config: FeeConfig,
    pub curve: BondingCurve,
    pub bonding_curve: Pubkey,
    pub mint: MintInfo,
    pub base_ata_amount: u64,
    pub slot: u64,
    pub fetched_at: Instant,
}

pub struct PumpAdapter {
    rpc: Option<RpcClient>,
    sdk: PumpSdk,
    cache: Arc<RwLock<HashMap<Pubkey, CurveSnapshot>>>,
    compute_units: u32,
}

impl PumpAdapter {
    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc: Some(rpc), sdk: PumpSdk::new(), cache: Default::default(), compute_units: DEFAULT_COMPUTE_UNITS }
    }

    /// Offline adapter (fixtures / benchmarks): state is injected with `insert_snapshot`.
    pub fn offline() -> Self {
        Self { rpc: None, sdk: PumpSdk::new(), cache: Default::default(), compute_units: DEFAULT_COMPUTE_UNITS }
    }

    pub fn with_compute_units(mut self, cu: u32) -> Self {
        self.compute_units = cu;
        self
    }

    pub fn insert_snapshot(&self, mint: Pubkey, s: CurveSnapshot) {
        self.cache.write().insert(mint, s);
    }

    pub fn snapshot(&self, mint: &Pubkey) -> Option<CurveSnapshot> {
        self.cache.read().get(mint).cloned()
    }

    /// Decode a full snapshot from raw account data (also used by fixture tests).
    #[allow(clippy::too_many_arguments)]
    pub fn decode_snapshot(
        mint: &Pubkey,
        global: (&Pubkey, &[u8]),
        fee_config: (&Pubkey, &[u8]),
        curve: (&Pubkey, &[u8]),
        mint_acct: (&Pubkey, &[u8]),
        base_ata_amount: u64,
        slot: u64,
    ) -> Result<CurveSnapshot, ProtocolError> {
        let bad = |m: &str| ProtocolError::StateUnavailable(m.to_string());
        if *global.0 != PUMP_PROGRAM_ID {
            return Err(bad("global not owned by pump program"));
        }
        if *fee_config.0 != PUMP_FEE_PROGRAM_ID {
            return Err(bad("fee_config not owned by pump fee program"));
        }
        if *curve.0 != PUMP_PROGRAM_ID {
            return Err(bad("bonding curve not owned by pump program"));
        }
        let global = decode_global(global.1).map_err(|e| bad(&format!("global: {e}")))?;
        let fee_config = decode_fee_config(fee_config.1).map_err(|e| bad(&format!("fee_config: {e}")))?;
        let curve_acc = decode_bonding_curve(curve.1).map_err(|e| bad(&format!("bonding curve: {e}")))?;
        let mint_info = parse_mint(mint_acct.0, mint_acct.1).map_err(|e| bad(&format!("mint: {e}")))?;
        Ok(CurveSnapshot {
            global,
            fee_config,
            curve: curve_acc,
            bonding_curve: pda::pump::bonding_curve(mint).0,
            mint: mint_info,
            base_ata_amount,
            slot,
            fetched_at: Instant::now(),
        })
    }

    pub async fn fetch(&self, mint: &Pubkey) -> Result<Option<CurveSnapshot>, ProtocolError> {
        let rpc = self.rpc.as_ref().ok_or_else(|| ProtocolError::StateUnavailable("offline adapter".into()))?;
        let bc = pda::pump::bonding_curve(mint).0;
        let keys = [
            pda::pump::global().0,
            pda::pump::fee_config().0,
            bc,
            *mint,
            associated_token_address(&bc, mint, &SPL_TOKEN_PROGRAM),
            associated_token_address(&bc, mint, &TOKEN_2022_PROGRAM),
        ];
        let (slot, accts) = rpc
            .get_multiple_accounts(&keys, Commitment::Processed)
            .await
            .map_err(|e| ProtocolError::Rpc(e.to_string()))?;
        let curve = match &accts[2] {
            None => return Ok(None), // not a pump coin
            Some(a) => a,
        };
        let global = accts[0].as_ref().ok_or_else(|| ProtocolError::StateUnavailable("pump global missing".into()))?;
        let fee = accts[1].as_ref().ok_or_else(|| ProtocolError::StateUnavailable("pump fee_config missing".into()))?;
        let mint_acct = accts[3].as_ref().ok_or_else(|| ProtocolError::StateUnavailable("mint missing".into()))?;
        let ata_idx = if mint_acct.owner == TOKEN_2022_PROGRAM { 5 } else { 4 };
        let base_ata_amount = match &accts[ata_idx] {
            Some(a) => parse_token_account(&a.owner, &a.data).map(|t| t.amount).unwrap_or(0),
            None => 0,
        };
        let snap = Self::decode_snapshot(
            mint,
            (&global.owner, &global.data),
            (&fee.owner, &fee.data),
            (&curve.owner, &curve.data),
            (&mint_acct.owner, &mint_acct.data),
            base_ata_amount,
            slot,
        )?;
        Ok(Some(snap))
    }

    fn fresh(&self, mint: &Pubkey, req: &QuoteRequest) -> Result<CurveSnapshot, ProtocolError> {
        let s = self
            .cache
            .read()
            .get(mint)
            .cloned()
            .ok_or_else(|| ProtocolError::StateUnavailable("no cached pump state".into()))?;
        let age = s.fetched_at.elapsed();
        if age > req.max_state_age {
            return Err(ProtocolError::Stale(format!("{} ms > {} ms", age.as_millis(), req.max_state_age.as_millis())));
        }
        Ok(s)
    }

    fn check_supported(s: &CurveSnapshot) -> Result<(), ProtocolError> {
        let unsupported = |r: &str| Err(ProtocolError::Unsupported { adapter: "pump", reason: r.to_string() });
        if s.curve.complete {
            return unsupported("curve complete (migrated to PumpSwap); PumpSwap adapter not enabled");
        }
        if s.curve.quote_mint != Pubkey::default() && s.curve.quote_mint != constants::NATIVE_MINT {
            return unsupported("non-SOL quote mint");
        }
        if s.curve.is_mayhem_mode {
            return unsupported("mayhem-mode coin");
        }
        if s.curve.is_cashback_coin {
            // pump-rust-client fixtures: "v3 / pool v2 trades refuse cashback coins".
            return unsupported("legacy cashback coin: v3 trades refuse it and the legacy v2 path is not enabled");
        }
        let risk = screen_mint(&s.mint, None);
        if risk.blocked() {
            let reasons: Vec<String> = risk.flags.iter().filter(|f| f.level == trader_execution::RiskLevel::Block).map(|f| f.message.clone()).collect();
            return unsupported(&format!("risk screen blocked: {}", reasons.join("; ")));
        }
        Ok(())
    }

    fn fee_bps(&self, s: &CurveSnapshot) -> Option<u64> {
        compute_bonding_curve_fee_bps(
            &s.global,
            &s.fee_config,
            &constants::NATIVE_MINT,
            s.curve.creator_fee_bps,
            s.mint.supply,
            s.curve.virtual_quote_reserves,
            s.curve.virtual_token_reserves,
        )
        .ok()
        .map(|f| f.protocol_fee_bps + f.creator_fee_bps)
    }

    fn buyback_recipient(global: &Global, user: &Pubkey) -> Option<Pubkey> {
        let mut c: Vec<Pubkey> = global.buyback_fee_recipients.iter().copied().filter(|p| *p != Pubkey::default()).collect();
        c.sort();
        c.dedup();
        if c.is_empty() {
            return None;
        }
        // Deterministic spread across recipients (keeps tx bytes reproducible).
        let idx = user.to_bytes()[0] as usize % c.len();
        Some(c[idx])
    }

    /// Decode actual fill from confirmed tx balances (token delta + SOL delta).
    pub fn decode_fill(tx: &trader_market_data::TransactionDetails, user: &Pubkey, mint: &Pubkey) -> (i128, Option<i128>) {
        let token = tx.token_delta(&user.to_string(), &mint.to_string());
        let sol = tx.lamport_delta(&user.to_string());
        (token, sol)
    }

    pub fn token_changes(tx: &trader_market_data::TransactionDetails) -> &[TokenBalanceChange] {
        &tx.token_balances
    }
}

struct PumpRule;

impl ProgramRule for PumpRule {
    fn check(&self, ix: &IxView<'_>, wallet: &Pubkey) -> Result<(), String> {
        let disc: [u8; 8] = ix.data.get(..8).and_then(|d| d.try_into().ok()).ok_or("short data")?;
        if disc != DISC_BUY_EXACT_QUOTE_IN_V3 && disc != DISC_SELL_V3 && disc != DISC_BUY_V3 {
            return Err("only buy_v3 / buy_exact_quote_in_v3 / sell_v3 are allowed".into());
        }
        if ix.accounts.get(V3_USER_INDEX) != Some(wallet) {
            return Err("pump trade user must be the signing wallet".into());
        }
        Ok(())
    }
}

#[async_trait]
impl ProtocolAdapter for PumpAdapter {
    fn kind(&self) -> DexKind {
        DexKind::PumpBondingCurve
    }
    fn name(&self) -> &'static str {
        "pump"
    }
    fn program_rules(&self) -> Vec<(Pubkey, &'static str, Option<Arc<dyn ProgramRule>>)> {
        vec![(PUMP_PROGRAM_ID, "pump", Some(Arc::new(PumpRule)))]
    }

    async fn resolve_market(&self, mint: &Pubkey) -> Result<Option<MarketRef>, ProtocolError> {
        if self.rpc.is_none() {
            // Offline (fixtures/benchmarks): only injected snapshots exist.
            return Ok(self.cache.read().get(mint).map(|s| MarketRef { dex: DexKind::PumpBondingCurve, market: s.bonding_curve, base_mint: *mint, quote_mint: constants::NATIVE_MINT }));
        }
        match self.fetch(mint).await? {
            None => Ok(None),
            Some(s) => {
                let m = MarketRef { dex: DexKind::PumpBondingCurve, market: s.bonding_curve, base_mint: *mint, quote_mint: constants::NATIVE_MINT };
                self.cache.write().insert(*mint, s);
                Ok(Some(m))
            }
        }
    }

    async fn refresh(&self, mint: &Pubkey) -> Result<(), ProtocolError> {
        match self.fetch(mint).await? {
            None => Err(ProtocolError::StateUnavailable("bonding curve not found".into())),
            Some(s) => {
                let mut w = self.cache.write();
                if w.get(mint).map(|o| o.slot <= s.slot).unwrap_or(true) {
                    w.insert(*mint, s);
                }
                Ok(())
            }
        }
    }

    fn quote_exact_in(&self, req: &QuoteRequest) -> Result<ProtocolQuote, ProtocolError> {
        let s = self.fresh(&req.mint, req)?;
        Self::check_supported(&s)?;
        if req.amount_in == 0 {
            return Err(ProtocolError::Quote("amount must be > 0".into()));
        }
        let q = |e: pump_rust_client::math::QuoteError| ProtocolError::Quote(format!("{e:?}"));
        let (quote, spot) = match req.side {
            Side::Buy => {
                let qt = self
                    .sdk
                    .buy_quote_bonding_curve_v3_sol_in(&s.global, &s.fee_config, &s.curve, s.mint.supply, TransferFee::NONE, s.base_ata_amount, req.amount_in, req.slippage_bps)
                    .map_err(q)?;
                let probe = req.amount_in.min(SPOT_PROBE_LAMPORTS);
                let sp = self
                    .sdk
                    .buy_quote_bonding_curve_v3_sol_in(&s.global, &s.fee_config, &s.curve, s.mint.supply, TransferFee::NONE, s.base_ata_amount, probe, 0)
                    .ok()
                    .map(|p| (probe, p.amount));
                (qt, sp)
            }
            Side::Sell => {
                let qt = self
                    .sdk
                    .sell_quote_bonding_curve(&s.global, &s.fee_config, &s.curve, s.mint.supply, TransferFee::NONE, req.amount_in, req.slippage_bps)
                    .map_err(q)?;
                let probe = (req.amount_in / 1000).max(1);
                let sp = self
                    .sdk
                    .sell_quote_bonding_curve(&s.global, &s.fee_config, &s.curve, s.mint.supply, TransferFee::NONE, probe, 0)
                    .ok()
                    .map(|p| (probe, p.amount));
                (qt, sp)
            }
        };
        if quote.amount == 0 {
            return Err(ProtocolError::Quote("quote returns zero output".into()));
        }
        // Impact: compare out/in against the probe's out/in.
        let impact = spot.and_then(|(pin, pout)| {
            let spot_scaled = pout as u128 * req.amount_in as u128; // spot output at full size
            price_impact_bps(spot_scaled / pin.max(1) as u128, quote.amount as u128)
        });
        let fee_bps = self.fee_bps(&s);
        let protocol_fee = match (req.side, fee_bps) {
            (Side::Buy, Some(b)) => Some(Lamports(((req.amount_in as u128 * b as u128).div_ceil(10_000)) as u64)),
            (Side::Sell, Some(b)) => Some(Lamports(((quote.amount as u128 * b as u128).div_ceil(10_000)) as u64)),
            _ => None,
        };
        let mut warnings = vec![];
        if quote.input_amount_used < req.amount_in && req.side == Side::Buy {
            warnings.push("partial fill: curve completes; remainder buys from post-migration pool".into());
        }
        if fee_bps.is_some() {
            warnings.push("protocol/creator fee shown is an estimate from the pump-fees tier table".into());
        }
        Ok(ProtocolQuote {
            market: MarketRef { dex: DexKind::PumpBondingCurve, market: s.bonding_curve, base_mint: req.mint, quote_mint: constants::NATIVE_MINT },
            side: req.side,
            amount_in: req.amount_in,
            input_used: quote.input_amount_used,
            expected_out: quote.amount,
            min_out: quote.min_out,
            max_in: quote.max_input,
            base_decimals: s.mint.decimals,
            base_token_program: s.mint.program.id(),
            protocol_fee_lamports: protocol_fee,
            price_impact_bps: impact,
            context_slot: s.slot,
            state_age_ms: s.fetched_at.elapsed().as_millis() as u64,
            rent_lamports: Lamports(0),
            compute_units: self.compute_units,
            warnings,
        })
    }

    fn user_rent_candidates(&self, user: &Pubkey, _mint: &Pubkey, _side: Side) -> Vec<(Pubkey, Lamports)> {
        // Measured on localnet with pump.so (pump-rust-client 0.5.0): the first
        // v3 trade creates `user_volume_accumulator` (owner pump, 137 bytes,
        // 1_844_400 lamports) paid by the user.
        vec![(pda::pump::user_volume_accumulator(user).0, trader_domain::fees::rent_exempt_minimum(USER_VOLUME_ACCUMULATOR_LEN))]
    }

    fn build_swap(&self, quote: &ProtocolQuote, user: &Pubkey) -> Result<SwapBuild, ProtocolError> {
        let s = self
            .cache
            .read()
            .get(&quote.market.base_mint)
            .cloned()
            .ok_or_else(|| ProtocolError::StateUnavailable("no cached pump state".into()))?;
        let buyback = Self::buyback_recipient(&s.global, user)
            .ok_or_else(|| ProtocolError::StateUnavailable("global has no buyback fee recipients".into()))?;
        let ixs: Vec<Instruction> = match quote.side {
            Side::Buy => self.sdk.buy_exact_quote_in_v3_instructions(
                quote.market.base_mint,
                constants::NATIVE_MINT,
                quote.base_token_program,
                constants::SPL_TOKEN_PROGRAM_ID,
                *user,
                buyback,
                quote.amount_in,
                quote.min_out,
            ),
            Side::Sell => self.sdk.sell_v3_instructions(
                quote.market.base_mint,
                constants::NATIVE_MINT,
                quote.base_token_program,
                constants::SPL_TOKEN_PROGRAM_ID,
                *user,
                buyback,
                quote.amount_in,
                quote.min_out,
            ),
        };
        Ok(SwapBuild { instructions: ixs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_match_sdk() {
        use anchor_lang::Discriminator;
        use pump_rust_client::pump::client::args;
        assert_eq!(args::BuyV3::DISCRIMINATOR, DISC_BUY_V3);
        assert_eq!(args::BuyExactQuoteInV3::DISCRIMINATOR, DISC_BUY_EXACT_QUOTE_IN_V3);
        assert_eq!(args::SellV3::DISCRIMINATOR, DISC_SELL_V3);
    }

    #[test]
    fn builder_user_index_and_rule() {
        let sdk = PumpSdk::new();
        let user = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ixs = sdk.buy_exact_quote_in_v3_instructions(mint, constants::NATIVE_MINT, TOKEN_2022_PROGRAM, SPL_TOKEN_PROGRAM, user, Pubkey::new_unique(), 1_000, 1);
        let trade = ixs.iter().find(|i| i.program_id == PUMP_PROGRAM_ID).expect("pump ix");
        assert_eq!(trade.accounts[V3_USER_INDEX].pubkey, user);
        assert!(trade.accounts[V3_USER_INDEX].is_signer);
        assert_eq!(trade.accounts.len(), 17);
        assert_eq!(&trade.data[..8], &DISC_BUY_EXACT_QUOTE_IN_V3);
        // other ixs are ATA creates for the user
        for i in ixs.iter().filter(|i| i.program_id != PUMP_PROGRAM_ID) {
            assert_eq!(i.program_id, trader_market_data::token::ATA_PROGRAM);
        }
        let view = IxView { program: PUMP_PROGRAM_ID, accounts: trade.accounts.iter().map(|a| a.pubkey).collect(), data: &trade.data };
        assert!(PumpRule.check(&view, &user).is_ok());
        assert!(PumpRule.check(&view, &Pubkey::new_unique()).is_err());
        let mut bad = trade.data.clone();
        bad[0] ^= 1;
        let view = IxView { program: PUMP_PROGRAM_ID, accounts: trade.accounts.iter().map(|a| a.pubkey).collect(), data: &bad };
        assert!(PumpRule.check(&view, &user).is_err());
    }

    #[test]
    fn rent_candidates_include_volume_accumulator() {
        let a = PumpAdapter::offline();
        let u = Pubkey::new_unique();
        let c = a.user_rent_candidates(&u, &Pubkey::new_unique(), Side::Buy);
        assert_eq!(c, vec![(pda::pump::user_volume_accumulator(&u).0, Lamports(1_844_400))]);
    }

    #[test]
    fn offline_quote_without_state_fails_closed() {
        let a = PumpAdapter::offline();
        let r = QuoteRequest { mint: Pubkey::new_unique(), side: Side::Buy, amount_in: 1, slippage_bps: 100, max_state_age: std::time::Duration::from_secs(1) };
        assert!(matches!(a.quote_exact_in(&r), Err(ProtocolError::StateUnavailable(_))));
    }
}
