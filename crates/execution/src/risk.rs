//! Token risk screening from verifiable on-chain data. This is a *screen*, not
//! a guarantee: it cannot detect every rug or honeypot, and the UI must never
//! render a "safe" badge from it.

use serde::Serialize;
use trader_market_data::token::{Extension, MintInfo, TokenProgram};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Info,
    Warn,
    /// Trading is refused (fail closed).
    Block,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RiskFlag {
    pub code: &'static str,
    pub level: RiskLevel,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RiskReport {
    pub flags: Vec<RiskFlag>,
    /// Top-10 holder share in bps, if reliably available (excludes nothing; the
    /// bonding curve / pool vault is usually the largest holder).
    pub top10_holder_bps: Option<u32>,
    pub source: &'static str,
}

impl RiskReport {
    pub fn blocked(&self) -> bool {
        self.flags.iter().any(|f| f.level == RiskLevel::Block)
    }
    fn push(&mut self, code: &'static str, level: RiskLevel, message: impl Into<String>) {
        self.flags.push(RiskFlag { code, level, message: message.into() });
    }
}

/// Extensions explicitly understood as harmless for swap math / transfers.
fn extension_policy(e: Extension) -> (RiskLevel, &'static str) {
    use Extension::*;
    match e {
        MetadataPointer | TokenMetadata | GroupPointer | TokenGroup | GroupMemberPointer | TokenGroupMember | ImmutableOwner => {
            (RiskLevel::Info, "metadata/group extension")
        }
        MintCloseAuthority => (RiskLevel::Warn, "mint close authority set"),
        InterestBearingConfig | ScaledUiAmount => (RiskLevel::Warn, "UI amount differs from raw amount"),
        TransferFeeConfig => (RiskLevel::Block, "transfer fee on base token is not supported by our quote math"),
        TransferHook => (RiskLevel::Block, "transfer hook can run arbitrary code on transfer"),
        PermanentDelegate => (RiskLevel::Block, "permanent delegate can move or burn holder tokens"),
        NonTransferable => (RiskLevel::Block, "token is non-transferable"),
        DefaultAccountState => (RiskLevel::Block, "default account state may freeze new accounts"),
        Pausable => (RiskLevel::Block, "transfers can be paused by an authority"),
        ConfidentialTransferMint | ConfidentialTransferFeeConfig | ConfidentialMintBurn => {
            (RiskLevel::Block, "confidential transfer extensions not supported")
        }
        _ => (RiskLevel::Block, "unrecognized token extension"),
    }
}

pub fn screen_mint(mint: &MintInfo, top10_holder_bps: Option<u32>) -> RiskReport {
    let mut r = RiskReport { top10_holder_bps, source: "on-chain mint account", ..Default::default() };
    if !mint.is_initialized {
        r.push("uninitialized_mint", RiskLevel::Block, "mint is not initialized");
    }
    if mint.freeze_authority.is_some() {
        r.push("freeze_authority", RiskLevel::Block, "freeze authority is set: your tokens could be frozen");
    }
    if mint.mint_authority.is_some() {
        r.push("mint_authority", RiskLevel::Warn, "mint authority is set: supply can be inflated");
    }
    if mint.program == TokenProgram::Token2022 {
        r.push("token_2022", RiskLevel::Info, "Token-2022 mint");
        for e in &mint.extensions {
            let (lvl, msg) = extension_policy(*e);
            if lvl > RiskLevel::Info {
                r.push("token_extension", lvl, format!("{e:?}: {msg}"));
            }
        }
    }
    match top10_holder_bps {
        Some(b) if b >= 8_000 => r.push("holder_concentration", RiskLevel::Warn, format!("top-10 holders own {}%", b / 100)),
        None => r.push("holder_concentration_unknown", RiskLevel::Info, "holder concentration unknown"),
        _ => {}
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;

    fn mint(program: TokenProgram, exts: Vec<Extension>) -> MintInfo {
        MintInfo {
            program,
            mint_authority: None,
            supply: 1,
            decimals: 6,
            is_initialized: true,
            freeze_authority: None,
            extensions: exts,
            transfer_fee_bps: None,
        }
    }

    #[test]
    fn blocks_dangerous_extensions() {
        for e in [Extension::TransferHook, Extension::PermanentDelegate, Extension::TransferFeeConfig, Extension::NonTransferable, Extension::Other(99)] {
            assert!(screen_mint(&mint(TokenProgram::Token2022, vec![e]), None).blocked(), "{e:?}");
        }
        let ok = screen_mint(&mint(TokenProgram::Token2022, vec![Extension::MetadataPointer, Extension::TokenMetadata]), Some(1000));
        assert!(!ok.blocked());
    }

    #[test]
    fn authorities() {
        let mut m = mint(TokenProgram::Spl, vec![]);
        m.freeze_authority = Some(Pubkey::new_unique());
        assert!(screen_mint(&m, None).blocked());
        let mut m = mint(TokenProgram::Spl, vec![]);
        m.mint_authority = Some(Pubkey::new_unique());
        let r = screen_mint(&m, Some(9_000));
        assert!(!r.blocked());
        assert!(r.flags.iter().any(|f| f.code == "mint_authority"));
        assert!(r.flags.iter().any(|f| f.code == "holder_concentration"));
    }
}
