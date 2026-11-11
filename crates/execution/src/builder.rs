//! Build -> guard -> sign-once. The output `SignedTx` is immutable; every
//! sender receives the exact same bytes, so racing senders can never produce
//! two different transactions (and thus never two swaps).

use crate::guard::{GuardConfig, GuardError, TxGuard};
use trader_wallets::{SignError, TxSigner};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::{v0, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use trader_domain::{ComputeBudget, Lamports, Network};

/// Max serialized transaction size (bytes) accepted by the network.
pub const PACKET_DATA_SIZE: usize = 1232;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("message compile failed: {0}")]
    Compile(String),
    #[error("transaction too large: {0} > {PACKET_DATA_SIZE} bytes")]
    TooLarge(usize),
    #[error("guard rejected transaction: {0}")]
    Guard(#[from] GuardError),
    #[error("signing failed: {0}")]
    Sign(#[from] SignError),
    #[error("serialization failed: {0}")]
    Serialize(String),
}

#[derive(Clone, Debug)]
pub struct TipInstruction {
    pub account: Pubkey,
    pub lamports: Lamports,
}

#[derive(Clone, Debug)]
pub struct BuildParams {
    pub network: Network,
    pub payer: Pubkey,
    pub instructions: Vec<Instruction>,
    pub compute: ComputeBudget,
    pub tip: Option<TipInstruction>,
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
}

#[derive(Clone, Debug)]
pub struct SignedTx {
    pub signature: Signature,
    pub wire: Vec<u8>,
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
    pub network: Network,
    pub compute: ComputeBudget,
    pub tip: Option<TipInstruction>,
}

impl SignedTx {
    pub fn signature_b58(&self) -> String {
        self.signature.to_string()
    }
    pub fn wire_b64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&self.wire)
    }
}

pub fn assemble_instructions(p: &BuildParams) -> Vec<Instruction> {
    let mut ixs = Vec::with_capacity(p.instructions.len() + 3);
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(p.compute.unit_limit));
    if p.compute.unit_price_micro_lamports > 0 {
        ixs.push(ComputeBudgetInstruction::set_compute_unit_price(p.compute.unit_price_micro_lamports));
    }
    ixs.extend(p.instructions.iter().cloned());
    if let Some(t) = &p.tip {
        // Tip last: if the swap fails the whole tx fails atomically and no tip
        // is transferred (base + priority fee are still charged).
        ixs.push(solana_system_interface::instruction::transfer(&p.payer, &t.account, t.lamports.0));
    }
    ixs
}

pub fn compile(p: &BuildParams) -> Result<VersionedMessage, BuildError> {
    let ixs = assemble_instructions(p);
    let msg = v0::Message::try_compile(&p.payer, &ixs, &[], p.blockhash).map_err(|e| BuildError::Compile(e.to_string()))?;
    Ok(VersionedMessage::V0(msg))
}

/// Compile, validate with the guard, sign exactly once, serialize.
pub async fn build_and_sign(
    p: &BuildParams,
    guard: &TxGuard,
    guard_cfg: &GuardConfig,
    signer: &dyn TxSigner,
) -> Result<SignedTx, BuildError> {
    if signer.pubkey() != p.payer {
        return Err(BuildError::Guard(GuardError::WrongPayer));
    }
    let message = compile(p)?;
    guard.validate(&message, &p.payer, guard_cfg)?;
    let msg_bytes = message.serialize();
    let signature = signer.sign_message(p.network, &msg_bytes).await?;
    let tx = VersionedTransaction { signatures: vec![signature], message };
    let wire = bincode::serialize(&tx).map_err(|e| BuildError::Serialize(e.to_string()))?;
    if wire.len() > PACKET_DATA_SIZE {
        return Err(BuildError::TooLarge(wire.len()));
    }
    Ok(SignedTx {
        signature,
        wire,
        blockhash: p.blockhash,
        last_valid_block_height: p.last_valid_block_height,
        network: p.network,
        compute: p.compute,
        tip: p.tip.clone(),
    })
}

/// Decode a wire tx and verify its first signature against its message. Used
/// by tests and by the mock RPC to prove what was sent is what was signed.
pub fn verify_wire(wire: &[u8]) -> Result<(VersionedTransaction, Signature), String> {
    let tx: VersionedTransaction = bincode::deserialize(wire).map_err(|e| e.to_string())?;
    let ok = tx.verify_with_results();
    if ok.is_empty() || !ok.iter().all(|b| *b) {
        return Err("signature verification failed".into());
    }
    let sig = tx.signatures[0];
    Ok((tx, sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::TxGuard;
    use trader_wallets::{MockSigner, TxSigner};

    fn memo_like_ix(program: Pubkey, payer: Pubkey) -> Instruction {
        Instruction::new_with_bytes(program, &[1, 2, 3], vec![solana_instruction::AccountMeta::new(payer, true)])
    }

    #[tokio::test]
    async fn sign_once_and_verify() {
        let signer = MockSigner::from_seed([3; 32]);
        let program = Pubkey::new_unique();
        let mut guard = TxGuard::default();
        guard.allow_program(program, "test");
        let p = BuildParams {
            network: Network::Localnet,
            payer: signer.pubkey(),
            instructions: vec![memo_like_ix(program, signer.pubkey())],
            compute: ComputeBudget { unit_limit: 50_000, unit_price_micro_lamports: 1_000 },
            tip: None,
            blockhash: Hash::new_from_array([5; 32]),
            last_valid_block_height: 100,
        };
        let cfg = GuardConfig { max_priority_fee: Lamports(1_000_000), max_tip: Lamports(0), tip_accounts: vec![] };
        let tx = build_and_sign(&p, &guard, &cfg, &signer).await.unwrap();
        let (decoded, sig) = verify_wire(&tx.wire).unwrap();
        assert_eq!(sig, tx.signature);
        assert_eq!(decoded.message.recent_blockhash(), &p.blockhash);
        // Same params, ed25519 is deterministic => identical bytes (no hidden nondeterminism).
        let tx2 = build_and_sign(&p, &guard, &cfg, &signer).await.unwrap();
        assert_eq!(tx.wire, tx2.wire);
        // tampering breaks verification
        let mut bad = tx.wire.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(verify_wire(&bad).is_err());
    }

    #[tokio::test]
    async fn wrong_payer_and_network_rejected() {
        let signer = MockSigner::from_seed([3; 32]);
        let guard = TxGuard::default();
        let cfg = GuardConfig::default();
        let mut p = BuildParams {
            network: Network::Mainnet,
            payer: signer.pubkey(),
            instructions: vec![],
            compute: ComputeBudget { unit_limit: 1000, unit_price_micro_lamports: 0 },
            tip: None,
            blockhash: Hash::default(),
            last_valid_block_height: 1,
        };
        assert!(matches!(build_and_sign(&p, &guard, &cfg, &signer).await, Err(BuildError::Sign(_))));
        p.network = Network::Localnet;
        p.payer = Pubkey::new_unique();
        assert!(matches!(build_and_sign(&p, &guard, &cfg, &signer).await, Err(BuildError::Guard(GuardError::WrongPayer))));
    }
}
