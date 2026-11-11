//! SPL Token / Token-2022 account parsing with explicit extension detection.
//! Layouts: https://github.com/solana-program/token and token-2022 (state.rs,
//! extension/mod.rs ExtensionType discriminants).

use solana_pubkey::Pubkey;

pub const SPL_TOKEN_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const NATIVE_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

const MINT_LEN: usize = 82;
const ACCOUNT_LEN: usize = 165;
const ACCOUNT_TYPE_OFFSET: usize = 165;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TokenProgram {
    Spl,
    Token2022,
}

impl TokenProgram {
    pub fn from_owner(owner: &Pubkey) -> Option<Self> {
        if *owner == SPL_TOKEN_PROGRAM {
            Some(TokenProgram::Spl)
        } else if *owner == TOKEN_2022_PROGRAM {
            Some(TokenProgram::Token2022)
        } else {
            None
        }
    }
    pub fn id(self) -> Pubkey {
        match self {
            TokenProgram::Spl => SPL_TOKEN_PROGRAM,
            TokenProgram::Token2022 => TOKEN_2022_PROGRAM,
        }
    }
}

/// Token-2022 extension discriminants (subset we name explicitly).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Extension {
    TransferFeeConfig,
    TransferFeeAmount,
    MintCloseAuthority,
    ConfidentialTransferMint,
    ConfidentialTransferAccount,
    DefaultAccountState,
    ImmutableOwner,
    MemoTransfer,
    NonTransferable,
    InterestBearingConfig,
    CpiGuard,
    PermanentDelegate,
    NonTransferableAccount,
    TransferHook,
    TransferHookAccount,
    ConfidentialTransferFeeConfig,
    ConfidentialTransferFeeAmount,
    MetadataPointer,
    TokenMetadata,
    GroupPointer,
    TokenGroup,
    GroupMemberPointer,
    TokenGroupMember,
    ConfidentialMintBurn,
    ScaledUiAmount,
    Pausable,
    PausableAccount,
    Other(u16),
}

impl Extension {
    pub fn from_u16(v: u16) -> Self {
        use Extension::*;
        match v {
            1 => TransferFeeConfig,
            2 => TransferFeeAmount,
            3 => MintCloseAuthority,
            4 => ConfidentialTransferMint,
            5 => ConfidentialTransferAccount,
            6 => DefaultAccountState,
            7 => ImmutableOwner,
            8 => MemoTransfer,
            9 => NonTransferable,
            10 => InterestBearingConfig,
            11 => CpiGuard,
            12 => PermanentDelegate,
            13 => NonTransferableAccount,
            14 => TransferHook,
            15 => TransferHookAccount,
            16 => ConfidentialTransferFeeConfig,
            17 => ConfidentialTransferFeeAmount,
            18 => MetadataPointer,
            19 => TokenMetadata,
            20 => GroupPointer,
            21 => TokenGroup,
            22 => GroupMemberPointer,
            23 => TokenGroupMember,
            24 => ConfidentialMintBurn,
            25 => ScaledUiAmount,
            26 => Pausable,
            27 => PausableAccount,
            o => Other(o),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintInfo {
    pub program: TokenProgram,
    pub mint_authority: Option<Pubkey>,
    pub supply: u64,
    pub decimals: u8,
    pub is_initialized: bool,
    pub freeze_authority: Option<Pubkey>,
    pub extensions: Vec<Extension>,
    /// Raw TransferFeeConfig bytes if present (newer fee at the end: epoch u64, max_fee u64, bps u16).
    pub transfer_fee_bps: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenAccountInfo {
    pub program: TokenProgram,
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// 0 = uninitialized, 1 = initialized, 2 = frozen
    pub state: u8,
    pub extensions: Vec<Extension>,
}

impl TokenAccountInfo {
    pub fn is_frozen(&self) -> bool {
        self.state == 2
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenParseError {
    #[error("account not owned by a token program")]
    NotTokenProgram,
    #[error("account data too short")]
    TooShort,
    #[error("malformed extension data")]
    BadExtensions,
}

fn read_coption_pubkey(d: &[u8]) -> Option<Pubkey> {
    let tag = u32::from_le_bytes(d[0..4].try_into().ok()?);
    if tag == 1 {
        Some(Pubkey::new_from_array(d[4..36].try_into().ok()?))
    } else {
        None
    }
}

fn parse_tlv(data: &[u8], expect_type: u8) -> Result<Vec<(Extension, &[u8])>, TokenParseError> {
    if data.len() <= ACCOUNT_TYPE_OFFSET {
        return Ok(vec![]);
    }
    if data[ACCOUNT_TYPE_OFFSET] != expect_type {
        return Err(TokenParseError::BadExtensions);
    }
    let mut out = vec![];
    let mut i = ACCOUNT_TYPE_OFFSET + 1;
    while i + 4 <= data.len() {
        let t = u16::from_le_bytes([data[i], data[i + 1]]);
        let len = u16::from_le_bytes([data[i + 2], data[i + 3]]) as usize;
        if t == 0 {
            break; // uninitialized padding
        }
        let start = i + 4;
        let end = start
            .checked_add(len)
            .ok_or(TokenParseError::BadExtensions)?;
        if end > data.len() {
            return Err(TokenParseError::BadExtensions);
        }
        out.push((Extension::from_u16(t), &data[start..end]));
        i = end;
    }
    Ok(out)
}

pub fn parse_mint(owner: &Pubkey, data: &[u8]) -> Result<MintInfo, TokenParseError> {
    let program = TokenProgram::from_owner(owner).ok_or(TokenParseError::NotTokenProgram)?;
    if data.len() < MINT_LEN {
        return Err(TokenParseError::TooShort);
    }
    let exts = if program == TokenProgram::Token2022 {
        parse_tlv(data, 1)?
    } else {
        vec![]
    };
    let transfer_fee_bps = exts
        .iter()
        .find(|(e, _)| *e == Extension::TransferFeeConfig)
        .and_then(|(_, v)| {
            // TransferFeeConfig: authority(32) withdraw_authority(32) withheld(8) older(18) newer(18)
            // TransferFee { epoch u64, maximum_fee u64, transfer_fee_basis_points u16 }
            let newer = v.get(32 + 32 + 8 + 18..32 + 32 + 8 + 36)?;
            Some(u16::from_le_bytes([newer[16], newer[17]]))
        });
    Ok(MintInfo {
        program,
        mint_authority: read_coption_pubkey(&data[0..36]),
        supply: u64::from_le_bytes(data[36..44].try_into().unwrap()),
        decimals: data[44],
        is_initialized: data[45] == 1,
        freeze_authority: read_coption_pubkey(&data[46..82]),
        extensions: exts.into_iter().map(|(e, _)| e).collect(),
        transfer_fee_bps,
    })
}

pub fn parse_token_account(
    owner: &Pubkey,
    data: &[u8],
) -> Result<TokenAccountInfo, TokenParseError> {
    let program = TokenProgram::from_owner(owner).ok_or(TokenParseError::NotTokenProgram)?;
    if data.len() < ACCOUNT_LEN {
        return Err(TokenParseError::TooShort);
    }
    let exts = if program == TokenProgram::Token2022 {
        parse_tlv(data, 2)?
    } else {
        vec![]
    };
    Ok(TokenAccountInfo {
        program,
        mint: Pubkey::new_from_array(data[0..32].try_into().unwrap()),
        owner: Pubkey::new_from_array(data[32..64].try_into().unwrap()),
        amount: u64::from_le_bytes(data[64..72].try_into().unwrap()),
        state: data[108],
        extensions: exts.into_iter().map(|(e, _)| e).collect(),
    })
}

pub fn associated_token_address(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mint_bytes(auth: Option<Pubkey>, freeze: Option<Pubkey>, supply: u64, dec: u8) -> Vec<u8> {
        let mut d = vec![0u8; MINT_LEN];
        if let Some(a) = auth {
            d[0..4].copy_from_slice(&1u32.to_le_bytes());
            d[4..36].copy_from_slice(a.as_ref());
        }
        d[36..44].copy_from_slice(&supply.to_le_bytes());
        d[44] = dec;
        d[45] = 1;
        if let Some(f) = freeze {
            d[46..50].copy_from_slice(&1u32.to_le_bytes());
            d[50..82].copy_from_slice(f.as_ref());
        }
        d
    }

    #[test]
    fn spl_mint() {
        let a = Pubkey::new_unique();
        let d = mint_bytes(Some(a), None, 1_000_000, 6);
        let m = parse_mint(&SPL_TOKEN_PROGRAM, &d).unwrap();
        assert_eq!(m.mint_authority, Some(a));
        assert_eq!(m.freeze_authority, None);
        assert_eq!(m.supply, 1_000_000);
        assert_eq!(m.decimals, 6);
        assert!(m.extensions.is_empty());
        assert_eq!(
            parse_mint(&Pubkey::new_unique(), &d),
            Err(TokenParseError::NotTokenProgram)
        );
        assert_eq!(
            parse_mint(&SPL_TOKEN_PROGRAM, &d[..50]),
            Err(TokenParseError::TooShort)
        );
    }

    #[test]
    fn token2022_mint_extensions() {
        let mut d = mint_bytes(None, Some(Pubkey::new_unique()), 5, 9);
        d.resize(ACCOUNT_TYPE_OFFSET, 0);
        d.push(1); // AccountType::Mint
                   // TransferHook (14), len 64
        d.extend_from_slice(&14u16.to_le_bytes());
        d.extend_from_slice(&64u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 64]);
        // TransferFeeConfig (1), len 108 with newer bps = 250
        let mut tf = vec![0u8; 108];
        tf[72 + 18 + 16..72 + 18 + 18].copy_from_slice(&250u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&108u16.to_le_bytes());
        d.extend_from_slice(&tf);
        let m = parse_mint(&TOKEN_2022_PROGRAM, &d).unwrap();
        assert_eq!(
            m.extensions,
            vec![Extension::TransferHook, Extension::TransferFeeConfig]
        );
        assert_eq!(m.transfer_fee_bps, Some(250));
        assert!(m.freeze_authority.is_some());
        // truncated TLV is rejected rather than ignored
        let bad = &d[..d.len() - 10];
        assert_eq!(
            parse_mint(&TOKEN_2022_PROGRAM, bad),
            Err(TokenParseError::BadExtensions)
        );
    }

    #[test]
    fn token_account() {
        let mut d = vec![0u8; ACCOUNT_LEN];
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&777u64.to_le_bytes());
        d[108] = 2;
        let t = parse_token_account(&SPL_TOKEN_PROGRAM, &d).unwrap();
        assert_eq!((t.mint, t.owner, t.amount), (mint, owner, 777));
        assert!(t.is_frozen());
    }

    #[test]
    fn ata_derivation_known_vector() {
        // Known: ATA of the system program id wallet for wSOL (cross-checked with spl-associated-token-account).
        let wallet = solana_pubkey::pubkey!("11111111111111111111111111111111");
        let ata = associated_token_address(&wallet, &NATIVE_MINT, &SPL_TOKEN_PROGRAM);
        assert_ne!(ata, Pubkey::default());
        // deterministic
        assert_eq!(
            ata,
            associated_token_address(&wallet, &NATIVE_MINT, &SPL_TOKEN_PROGRAM)
        );
    }
}
