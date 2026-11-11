//! Pre-signing transaction guard. Re-validates the *compiled* message
//! independently of whoever produced the instructions (our adapters or an
//! external router), so a bug or a malicious route cannot make the managed
//! signer authorize arbitrary transfers.

use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use trader_domain::Lamports;

pub const SYSTEM_PROGRAM: Pubkey = solana_pubkey::pubkey!("11111111111111111111111111111111");
pub const COMPUTE_BUDGET_PROGRAM: Pubkey = solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");
pub const ATA_PROGRAM: Pubkey = solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("fee payer is not the trading wallet")]
    WrongPayer,
    #[error("transaction requires {0} signatures; only the wallet may sign")]
    ExtraSigners(u8),
    #[error("address lookup tables are not allowed")]
    LookupTables,
    #[error("program {0} is not allowlisted")]
    ProgramNotAllowed(Pubkey),
    #[error("system instruction not allowed: {0}")]
    SystemInstruction(String),
    #[error("compute budget instruction not allowed: {0}")]
    ComputeBudget(String),
    #[error("priority fee {fee} exceeds cap {cap}")]
    PriorityFee { fee: Lamports, cap: Lamports },
    #[error("tip {amount} to {to} not allowed (cap {cap})")]
    Tip { to: Pubkey, amount: Lamports, cap: Lamports },
    #[error("ATA instruction not allowed: {0}")]
    Ata(String),
    #[error("program {program} rejected instruction: {reason}")]
    ProgramRule { program: Pubkey, reason: String },
    #[error("malformed message: {0}")]
    Malformed(String),
}

/// A compiled instruction with resolved keys.
pub struct IxView<'a> {
    pub program: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: &'a [u8],
}

/// Program-specific validation supplied by a DEX adapter.
pub trait ProgramRule: Send + Sync {
    fn check(&self, ix: &IxView<'_>, wallet: &Pubkey) -> Result<(), String>;
}

#[derive(Clone, Debug, Default)]
pub struct GuardConfig {
    pub max_priority_fee: Lamports,
    pub max_tip: Lamports,
    pub tip_accounts: Vec<Pubkey>,
}

#[derive(Clone, Default)]
pub struct TxGuard {
    programs: HashMap<Pubkey, (&'static str, Option<Arc<dyn ProgramRule>>)>,
}

impl TxGuard {
    pub fn allow_program(&mut self, id: Pubkey, name: &'static str) {
        self.programs.insert(id, (name, None));
    }
    pub fn allow_program_with_rule(&mut self, id: Pubkey, name: &'static str, rule: Arc<dyn ProgramRule>) {
        self.programs.insert(id, (name, Some(rule)));
    }
    pub fn allowed(&self) -> Vec<(Pubkey, &'static str)> {
        self.programs.iter().map(|(k, v)| (*k, v.0)).collect()
    }

    pub fn validate(&self, msg: &VersionedMessage, wallet: &Pubkey, cfg: &GuardConfig) -> Result<(), GuardError> {
        let keys = msg.static_account_keys();
        if keys.first() != Some(wallet) {
            return Err(GuardError::WrongPayer);
        }
        let header = msg.header();
        if header.num_required_signatures != 1 {
            return Err(GuardError::ExtraSigners(header.num_required_signatures));
        }
        if let VersionedMessage::V0(m) = msg {
            if !m.address_table_lookups.is_empty() {
                return Err(GuardError::LookupTables);
            }
        }
        let mut cu_limit: Option<u32> = None;
        let mut cu_price: Option<u64> = None;
        for cix in msg.instructions() {
            let program = *keys
                .get(cix.program_id_index as usize)
                .ok_or_else(|| GuardError::Malformed("program index".into()))?;
            let accounts = cix
                .accounts
                .iter()
                .map(|i| keys.get(*i as usize).copied().ok_or_else(|| GuardError::Malformed("account index".into())))
                .collect::<Result<Vec<_>, _>>()?;
            let ix = IxView { program, accounts, data: &cix.data };
            if program == COMPUTE_BUDGET_PROGRAM {
                check_compute_budget(&ix, &mut cu_limit, &mut cu_price)?;
            } else if program == SYSTEM_PROGRAM {
                check_system(&ix, wallet, cfg)?;
            } else if program == ATA_PROGRAM {
                check_ata(&ix, wallet)?;
            } else {
                match self.programs.get(&program) {
                    None => return Err(GuardError::ProgramNotAllowed(program)),
                    Some((_, Some(rule))) => rule
                        .check(&ix, wallet)
                        .map_err(|reason| GuardError::ProgramRule { program, reason })?,
                    Some((_, None)) => {}
                }
            }
        }
        let limit = cu_limit.unwrap_or(200_000) as u128;
        let fee = Lamports(((limit * cu_price.unwrap_or(0) as u128).div_ceil(1_000_000)) as u64);
        if fee > cfg.max_priority_fee {
            return Err(GuardError::PriorityFee { fee, cap: cfg.max_priority_fee });
        }
        Ok(())
    }
}

fn check_compute_budget(ix: &IxView<'_>, limit: &mut Option<u32>, price: &mut Option<u64>) -> Result<(), GuardError> {
    match ix.data.first() {
        Some(2) if ix.data.len() == 5 && limit.is_none() => {
            *limit = Some(u32::from_le_bytes(ix.data[1..5].try_into().unwrap()));
            Ok(())
        }
        Some(3) if ix.data.len() == 9 && price.is_none() => {
            *price = Some(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()));
            Ok(())
        }
        other => Err(GuardError::ComputeBudget(format!("discriminator {other:?} / duplicate"))),
    }
}

fn check_system(ix: &IxView<'_>, wallet: &Pubkey, cfg: &GuardConfig) -> Result<(), GuardError> {
    // SystemInstruction is bincode: u32 LE discriminant. Transfer = 2 { lamports: u64 }.
    if ix.data.len() != 12 || u32::from_le_bytes(ix.data[0..4].try_into().unwrap()) != 2 {
        return Err(GuardError::SystemInstruction("only tip transfers are allowed at top level".into()));
    }
    let lamports = Lamports(u64::from_le_bytes(ix.data[4..12].try_into().unwrap()));
    let (from, to) = match ix.accounts.as_slice() {
        [f, t] => (*f, *t),
        _ => return Err(GuardError::SystemInstruction("transfer accounts".into())),
    };
    if from != *wallet {
        return Err(GuardError::SystemInstruction("transfer source must be the wallet".into()));
    }
    if !cfg.tip_accounts.contains(&to) || lamports > cfg.max_tip {
        return Err(GuardError::Tip { to, amount: lamports, cap: cfg.max_tip });
    }
    Ok(())
}

fn check_ata(ix: &IxView<'_>, wallet: &Pubkey) -> Result<(), GuardError> {
    // CreateIdempotent = 1 (Create = 0 / empty data also accepted). Accounts:
    // [payer, ata, owner, mint, system, token_program]
    match ix.data {
        [] | [0] | [1] => {}
        _ => return Err(GuardError::Ata("only create/create_idempotent".into())),
    }
    match ix.accounts.as_slice() {
        [payer, _ata, owner, _mint, system, _tp] if payer == wallet && owner == wallet && *system == SYSTEM_PROGRAM => Ok(()),
        _ => Err(GuardError::Ata("ATA must be created for and paid by the wallet".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{compile, BuildParams, TipInstruction};
    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use trader_domain::{ComputeBudget, Network};

    fn params(payer: Pubkey, ixs: Vec<Instruction>, price: u64, tip: Option<TipInstruction>) -> BuildParams {
        BuildParams {
            network: Network::Localnet,
            payer,
            instructions: ixs,
            compute: ComputeBudget { unit_limit: 100_000, unit_price_micro_lamports: price },
            tip,
            blockhash: Hash::new_from_array([1; 32]),
            last_valid_block_height: 1,
        }
    }

    fn cfg(tip_acct: Pubkey) -> GuardConfig {
        GuardConfig { max_priority_fee: Lamports(10_000), max_tip: Lamports(5_000), tip_accounts: vec![tip_acct] }
    }

    struct DenyAll;
    impl ProgramRule for DenyAll {
        fn check(&self, _: &IxView<'_>, _: &Pubkey) -> Result<(), String> {
            Err("nope".into())
        }
    }

    #[test]
    fn rules() {
        let w = Pubkey::new_unique();
        let tip = Pubkey::new_unique();
        let good = Pubkey::new_unique();
        let bad = Pubkey::new_unique();
        let mut g = TxGuard::default();
        g.allow_program(good, "good");
        let ix = |p: Pubkey| Instruction::new_with_bytes(p, &[1], vec![AccountMeta::new(w, true)]);

        // allowlisted program, tip in range
        let m = compile(&params(w, vec![ix(good)], 1000, Some(TipInstruction { account: tip, lamports: Lamports(1000) }))).unwrap();
        assert_eq!(g.validate(&m, &w, &cfg(tip)), Ok(()));

        // unknown program
        let m = compile(&params(w, vec![ix(bad)], 1000, None)).unwrap();
        assert_eq!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::ProgramNotAllowed(bad)));

        // tip too large
        let m = compile(&params(w, vec![ix(good)], 0, Some(TipInstruction { account: tip, lamports: Lamports(6000) }))).unwrap();
        assert!(matches!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::Tip { .. })));

        // transfer to a non-tip account (drain attempt)
        let drain = solana_system_interface::instruction::transfer(&w, &bad, 1);
        let m = compile(&params(w, vec![drain], 0, None)).unwrap();
        assert!(matches!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::Tip { .. })));

        // priority fee over cap: 100k CU * 200_000 µL = 20_000 lamports
        let m = compile(&params(w, vec![ix(good)], 200_000, None)).unwrap();
        assert!(matches!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::PriorityFee { .. })));

        // wrong wallet
        let other = Pubkey::new_unique();
        assert_eq!(g.validate(&m, &other, &cfg(tip)), Err(GuardError::WrongPayer));

        // extra signer
        let two_sig = Instruction::new_with_bytes(good, &[1], vec![AccountMeta::new(w, true), AccountMeta::new(other, true)]);
        let m = compile(&params(w, vec![two_sig], 0, None)).unwrap();
        assert_eq!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::ExtraSigners(2)));

        // program rule rejection
        g.allow_program_with_rule(good, "good", Arc::new(DenyAll));
        let m = compile(&params(w, vec![ix(good)], 0, None)).unwrap();
        assert!(matches!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::ProgramRule { .. })));

        // ATA create for someone else
        let mint = Pubkey::new_unique();
        let ata_for_other = Instruction::new_with_bytes(
            ATA_PROGRAM,
            &[1],
            vec![
                AccountMeta::new(w, true),
                AccountMeta::new(Pubkey::new_unique(), false),
                AccountMeta::new_readonly(other, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new_readonly(Pubkey::new_unique(), false),
            ],
        );
        let m = compile(&params(w, vec![ata_for_other], 0, None)).unwrap();
        assert!(matches!(g.validate(&m, &w, &cfg(tip)), Err(GuardError::Ata(_))));
    }
}
