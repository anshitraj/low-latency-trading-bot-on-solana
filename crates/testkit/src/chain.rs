//! MOCK Solana JSON-RPC server backed by an in-memory ledger.
//!
//! Implements the subset of methods the engine/observer use, verifies
//! signatures on every sent transaction, enforces blockhash validity,
//! dedupes by signature, charges fees, executes MockDex + SystemProgram
//! transfers atomically (fee still charged on failure), runs preflight
//! simulation unless skipPreflight, and supports fault injection.

use crate::dex::{decode, swap_out, MockDex, MOCK_DEX_PROGRAM};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use trader_domain::Side;
use trader_market_data::token::{associated_token_address, SPL_TOKEN_PROGRAM};

const SYSTEM: Pubkey = solana_pubkey::pubkey!("11111111111111111111111111111111");
const COMPUTE_BUDGET: Pubkey =
    solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");
const ATA: Pubkey = solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const MOCK_GENESIS: &str = "MockGenesis1111111111111111111111111111111";

#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
    /// HTTP 429, not forwarded.
    RateLimit,
    /// HTTP 500 but the tx WAS forwarded (worst-case ambiguity).
    Http500Forwarded,
    /// HTTP 500, not forwarded.
    Http500NotForwarded,
    /// Respond after a delay (forwarded immediately).
    DelayAck(Duration),
    /// Ack with signature but never land.
    Drop,
    /// Land with a program error.
    FailOnchain,
    /// Land, disappear one slot later (fork), then land again 2 slots later.
    Reorg,
}

#[derive(Clone, Debug)]
pub struct ChainConfig {
    pub slot_ms: u64,
    pub land_delay_slots: u64,
    pub confirm_after: u64,
    pub finalize_after: u64,
    /// Blocks a blockhash stays valid (mainnet: 150).
    pub blockhash_validity: u64,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            slot_ms: 20,
            land_delay_slots: 1,
            confirm_after: 2,
            finalize_after: 4,
            blockhash_validity: 150,
        }
    }
}

#[derive(Clone, Debug)]
struct TxRecord {
    slot: u64,
    err: Option<Value>,
    fee: u64,
    keys: Vec<String>,
    pre: Vec<u64>,
    post: Vec<u64>,
    token_pre: Vec<(String, String, u64, u8)>,
    token_post: Vec<(String, String, u64, u8)>,
    cu: u64,
    hidden: bool,
}

struct Pending {
    sig: String,
    wire: Vec<u8>,
    land_at: u64,
    force_fail: bool,
    reorg: bool,
}

#[derive(Default)]
struct Ledger {
    slot: u64,
    lamports: HashMap<Pubkey, u64>,
    tokens: HashMap<(Pubkey, Pubkey), u64>,
    ata_index: HashMap<Pubkey, (Pubkey, Pubkey)>,
    decimals: HashMap<Pubkey, u8>,
    blockhashes: HashMap<Hash, u64>,
    txs: HashMap<String, TxRecord>,
    pending: Vec<Pending>,
    reorg_relands: Vec<(u64, String)>,
    faults: VecDeque<Fault>,
    sends: u64,
    landed_sigs: Vec<String>,
}

#[derive(Clone)]
pub struct MockChain {
    inner: Arc<Mutex<Ledger>>,
    pub cfg: ChainConfig,
    pub dex: MockDex,
    pub url: String,
}

fn hash_for_slot(slot: u64) -> Hash {
    let d = Sha256::digest(slot.to_le_bytes());
    Hash::new_from_array(d.into())
}

impl MockChain {
    /// Starts the server on 127.0.0.1:<random> and the slot ticker.
    pub async fn start(cfg: ChainConfig, dex: MockDex) -> Self {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let mut led = Ledger {
            slot: 1,
            ..Default::default()
        };
        led.blockhashes.insert(hash_for_slot(1), 1);
        let chain = Self {
            inner: Arc::new(Mutex::new(led)),
            cfg,
            dex,
            url,
        };
        let app = Router::new()
            .route("/", post(handle))
            .with_state(chain.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let c2 = chain.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(c2.cfg.slot_ms));
            loop {
                t.tick().await;
                c2.tick();
            }
        });
        chain
    }

    pub fn fund(&self, owner: &Pubkey, lamports: u64) {
        *self.inner.lock().lamports.entry(*owner).or_default() += lamports;
    }
    pub fn balance(&self, owner: &Pubkey) -> u64 {
        self.inner.lock().lamports.get(owner).copied().unwrap_or(0)
    }
    pub fn set_token(&self, owner: &Pubkey, mint: &Pubkey, amount: u64, decimals: u8) {
        let mut g = self.inner.lock();
        g.tokens.insert((*owner, *mint), amount);
        g.decimals.insert(*mint, decimals);
        g.ata_index.insert(
            associated_token_address(owner, mint, &SPL_TOKEN_PROGRAM),
            (*owner, *mint),
        );
    }
    pub fn token(&self, owner: &Pubkey, mint: &Pubkey) -> u64 {
        self.inner
            .lock()
            .tokens
            .get(&(*owner, *mint))
            .copied()
            .unwrap_or(0)
    }
    pub fn push_fault(&self, f: Fault) {
        self.inner.lock().faults.push_back(f);
    }
    pub fn sends(&self) -> u64 {
        self.inner.lock().sends
    }
    /// Signatures that executed successfully (each at most once by construction).
    pub fn landed_ok(&self) -> Vec<String> {
        let g = self.inner.lock();
        g.landed_sigs
            .iter()
            .filter(|s| g.txs.get(*s).map(|t| t.err.is_none()).unwrap_or(false))
            .cloned()
            .collect()
    }
    pub fn slot(&self) -> u64 {
        self.inner.lock().slot
    }

    fn tick(&self) {
        let mut g = self.inner.lock();
        g.slot += 1;
        let slot = g.slot;
        g.blockhashes.insert(hash_for_slot(slot), slot);
        let validity = self.cfg.blockhash_validity;
        g.blockhashes.retain(|_, s| *s + validity + 50 >= slot);
        // fork: hide then re-land
        let relands: Vec<String> = g
            .reorg_relands
            .iter()
            .filter(|(s, _)| *s <= slot)
            .map(|(_, sig)| sig.clone())
            .collect();
        g.reorg_relands.retain(|(s, _)| *s > slot);
        for sig in relands {
            if let Some(t) = g.txs.get_mut(&sig) {
                t.hidden = false;
                t.slot = slot;
            }
        }
        let due: Vec<Pending> = {
            let (due, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut g.pending)
                .into_iter()
                .partition(|p| p.land_at <= slot);
            g.pending = rest;
            due
        };
        for p in due {
            if g.txs.contains_key(&p.sig) {
                continue;
            }
            let Ok(tx) = bincode::deserialize::<VersionedTransaction>(&p.wire) else {
                continue;
            };
            let bh_slot = g.blockhashes.get(tx.message.recent_blockhash()).copied();
            match bh_slot {
                Some(s) if s + validity >= slot => {}
                _ => continue, // expired blockhash: never lands
            }
            if let Some(mut rec) = execute(&mut g, &self.dex, &tx, slot, p.force_fail, true) {
                if p.reorg {
                    rec.hidden = false;
                    g.reorg_relands.push((slot + 3, p.sig.clone()));
                }
                g.txs.insert(p.sig.clone(), rec);
                g.landed_sigs.push(p.sig.clone());
                if p.reorg {
                    // becomes hidden next slot
                    let sig = p.sig.clone();
                    let chain = self.clone();
                    let ms = self.cfg.slot_ms;
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        if let Some(t) = chain.inner.lock().txs.get_mut(&sig) {
                            t.hidden = true;
                        }
                    });
                }
            }
        }
    }
}

fn parse_compute(tx: &VersionedTransaction) -> (u64, u64) {
    let keys = tx.message.static_account_keys();
    let (mut limit, mut price) = (200_000u64, 0u64);
    for ix in tx.message.instructions() {
        if keys[ix.program_id_index as usize] == COMPUTE_BUDGET {
            match ix.data.first() {
                Some(2) if ix.data.len() == 5 => {
                    limit = u32::from_le_bytes(ix.data[1..5].try_into().unwrap()) as u64
                }
                Some(3) if ix.data.len() == 9 => {
                    price = u64::from_le_bytes(ix.data[1..9].try_into().unwrap())
                }
                _ => {}
            }
        }
    }
    (limit, price)
}

/// Execute against the ledger. `commit=false` = preflight simulation.
/// Returns None if the fee payer cannot pay the fee (tx not included).
fn execute(
    g: &mut Ledger,
    dex: &MockDex,
    tx: &VersionedTransaction,
    slot: u64,
    force_fail: bool,
    commit: bool,
) -> Option<TxRecord> {
    let keys: Vec<Pubkey> = tx.message.static_account_keys().to_vec();
    let payer = keys[0];
    let (limit, price) = parse_compute(tx);
    let fee = 5_000 * tx.signatures.len() as u64
        + (limit as u128 * price as u128).div_ceil(1_000_000) as u64;
    if g.lamports.get(&payer).copied().unwrap_or(0) < fee {
        return None;
    }
    let pre: Vec<u64> = keys
        .iter()
        .map(|k| g.lamports.get(k).copied().unwrap_or(0))
        .collect();
    let mut lam = g.lamports.clone();
    let mut tok = g.tokens.clone();
    let mut pools = dex.pools.read().clone();
    *lam.get_mut(&payer).unwrap() -= fee;
    let after_fee = lam.clone();
    let mut err: Option<Value> = None;
    let mut touched_mints: Vec<Pubkey> = vec![];
    for (i, ix) in tx.message.instructions().iter().enumerate() {
        let prog = keys[ix.program_id_index as usize];
        let acct = |n: usize| keys[ix.accounts[n] as usize];
        let fail = |code: u32| Some(json!({"InstructionError": [i, {"Custom": code}]}));
        if prog == COMPUTE_BUDGET || prog == ATA {
            continue;
        } else if prog == SYSTEM {
            if ix.data.len() == 12 && u32::from_le_bytes(ix.data[0..4].try_into().unwrap()) == 2 {
                let amt = u64::from_le_bytes(ix.data[4..12].try_into().unwrap());
                let (from, to) = (acct(0), acct(1));
                let bal = lam.get(&from).copied().unwrap_or(0);
                if bal < amt {
                    err = Some(json!({"InstructionError": [i, {"Custom": 1}]}));
                    break;
                }
                *lam.get_mut(&from).unwrap() -= amt;
                *lam.entry(to).or_default() += amt;
            } else {
                err = fail(9999);
                break;
            }
        } else if prog == MOCK_DEX_PROGRAM {
            let Some((side, amount_in, min_out)) = decode(&ix.data) else {
                err = fail(6000);
                break;
            };
            let (user, mint) = (acct(0), acct(1));
            touched_mints.push(mint);
            let Some(entry) = pools.get_mut(&mint) else {
                err = fail(6002);
                break;
            };
            let out = swap_out(&entry.0, side, amount_in);
            if force_fail || out < min_out {
                err = fail(6001); // slippage exceeded
                break;
            }
            match side {
                Side::Buy => {
                    let bal = lam.get(&user).copied().unwrap_or(0);
                    if bal < amount_in {
                        err = fail(1);
                        break;
                    }
                    *lam.get_mut(&user).unwrap() -= amount_in;
                    *tok.entry((user, mint)).or_default() += out;
                    entry.0.sol_reserve += amount_in;
                    entry.0.token_reserve -= out;
                }
                Side::Sell => {
                    let have = tok.get(&(user, mint)).copied().unwrap_or(0);
                    if have < amount_in {
                        err = fail(2);
                        break;
                    }
                    *tok.get_mut(&(user, mint)).unwrap() -= amount_in;
                    *lam.entry(user).or_default() += out;
                    entry.0.token_reserve += amount_in;
                    entry.0.sol_reserve -= out;
                }
            }
            entry.1 = slot;
            entry.2 = std::time::Instant::now();
        } else {
            err = Some(json!({"InstructionError": [i, "IncorrectProgramId"]}));
            break;
        }
    }
    let user = payer;
    let token_pre: Vec<(String, String, u64, u8)> = touched_mints
        .iter()
        .map(|m| {
            (
                user.to_string(),
                m.to_string(),
                g.tokens.get(&(user, *m)).copied().unwrap_or(0),
                g.decimals.get(m).copied().unwrap_or(6),
            )
        })
        .collect();
    if err.is_some() {
        lam = after_fee; // revert instruction effects, keep fee
        tok = g.tokens.clone();
        pools = dex.pools.read().clone();
    }
    let post: Vec<u64> = keys
        .iter()
        .map(|k| lam.get(k).copied().unwrap_or(0))
        .collect();
    let token_post: Vec<(String, String, u64, u8)> = touched_mints
        .iter()
        .map(|m| {
            (
                user.to_string(),
                m.to_string(),
                tok.get(&(user, *m)).copied().unwrap_or(0),
                g.decimals.get(m).copied().unwrap_or(6),
            )
        })
        .collect();
    if commit {
        g.lamports = lam;
        for m in &touched_mints {
            g.ata_index.insert(
                associated_token_address(&user, m, &SPL_TOKEN_PROGRAM),
                (user, *m),
            );
        }
        g.tokens = tok;
        *dex.pools.write() = pools;
    }
    Some(TxRecord {
        slot,
        err,
        fee,
        keys: keys.iter().map(|k| k.to_string()).collect(),
        pre,
        post,
        token_pre,
        token_post,
        cu: 30_000,
        hidden: false,
    })
}

fn ok(v: Value) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":1,"result":v})),
    )
}
fn rpc_err(code: i64, msg: &str, data: Value) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":code,"message":msg,"data":data}})),
    )
}

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}

async fn handle(State(c): State<MockChain>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let method = body["method"].as_str().unwrap_or("").to_string();
    let params = body["params"].clone();
    let b64 = base64::engine::general_purpose::STANDARD;
    match method.as_str() {
        "getGenesisHash" => ok(json!(MOCK_GENESIS)),
        "getHealth" => ok(json!("ok")),
        "getSlot" => ok(json!(c.slot())),
        "getBlockHeight" => {
            let s = c.slot();
            let commitment = params[0]["commitment"].as_str().unwrap_or("finalized");
            let h = match commitment {
                "finalized" => s.saturating_sub(c.cfg.finalize_after),
                "confirmed" => s.saturating_sub(c.cfg.confirm_after),
                _ => s,
            };
            ok(json!(h))
        }
        "getLatestBlockhash" => {
            let s = c.slot();
            ok(
                json!({"context":{"slot":s},"value":{"blockhash": hash_for_slot(s).to_string(), "lastValidBlockHeight": s + c.cfg.blockhash_validity}}),
            )
        }
        "getBalance" => {
            let k: Pubkey = params[0]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default();
            ok(json!({"context":{"slot":c.slot()},"value": c.balance(&k)}))
        }
        "getMultipleAccounts" => {
            let g = c.inner.lock();
            let vals: Vec<Value> = params[0]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|k| {
                    let k: Pubkey = k.as_str().and_then(|s| s.parse().ok()).unwrap_or_default();
                    if let Some((owner, mint)) = g.ata_index.get(&k) {
                        let amt = g.tokens.get(&(*owner, *mint)).copied().unwrap_or(0);
                        json!({"lamports": 2_039_280u64, "owner": SPL_TOKEN_PROGRAM.to_string(), "data": [b64.encode(token_account_data(mint, owner, amt)), "base64"], "executable": false, "rentEpoch": 0})
                    } else if let Some(l) = g.lamports.get(&k) {
                        json!({"lamports": l, "owner": SYSTEM.to_string(), "data": ["", "base64"], "executable": false, "rentEpoch": 0})
                    } else {
                        Value::Null
                    }
                })
                .collect();
            ok(json!({"context":{"slot":g.slot},"value":vals}))
        }
        "sendTransaction" => {
            let wire = match params[0].as_str().and_then(|s| b64.decode(s).ok()) {
                Some(w) => w,
                None => return rpc_err(-32602, "invalid base64", Value::Null),
            };
            let skip_preflight = params[1]["skipPreflight"].as_bool().unwrap_or(false);
            let (tx, sig) = match trader_execution::verify_wire(&wire) {
                Ok(x) => x,
                Err(e) => {
                    return rpc_err(
                        -32003,
                        &format!("Transaction signature verification failure: {e}"),
                        Value::Null,
                    )
                }
            };
            let sig = sig.to_string();
            let fault = {
                let mut g = c.inner.lock();
                g.sends += 1;
                g.faults.pop_front()
            };
            let landed = c.inner.lock().txs.contains_key(&sig);
            if landed && !skip_preflight {
                return rpc_err(
                    -32002,
                    "Transaction simulation failed: This transaction has already been processed",
                    json!({"err":"AlreadyProcessed"}),
                );
            }
            if !skip_preflight && !matches!(fault, Some(Fault::FailOnchain)) {
                let mut g = c.inner.lock();
                let slot = g.slot;
                let valid = g
                    .blockhashes
                    .get(tx.message.recent_blockhash())
                    .map(|s| *s + c.cfg.blockhash_validity >= slot)
                    .unwrap_or(false);
                if !valid {
                    return rpc_err(
                        -32002,
                        "Transaction simulation failed: Blockhash not found",
                        json!({"err":"BlockhashNotFound"}),
                    );
                }
                match execute(&mut g, &c.dex, &tx, slot, false, false) {
                    None => return rpc_err(-32002, "Transaction simulation failed: Attempt to debit an account but found no record of a prior credit.", json!({"err":"AccountNotFound"})),
                    Some(r) if r.err.is_some() => return rpc_err(-32002, "Transaction simulation failed: Error processing Instruction", json!({"err": r.err})),
                    _ => {}
                }
            }
            let enqueue = |force_fail: bool, reorg: bool| {
                let mut g = c.inner.lock();
                if !g.pending.iter().any(|p| p.sig == sig) && !g.txs.contains_key(&sig) {
                    let land_at = g.slot + c.cfg.land_delay_slots;
                    g.pending.push(Pending {
                        sig: sig.clone(),
                        wire: wire.clone(),
                        land_at,
                        force_fail,
                        reorg,
                    });
                }
            };
            match fault {
                None => {
                    enqueue(false, false);
                    ok(json!(sig))
                }
                Some(Fault::RateLimit) => (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":"rate limited"})),
                ),
                Some(Fault::Http500Forwarded) => {
                    enqueue(false, false);
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})))
                }
                Some(Fault::Http500NotForwarded) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})))
                }
                Some(Fault::DelayAck(d)) => {
                    enqueue(false, false);
                    tokio::time::sleep(d).await;
                    ok(json!(sig))
                }
                Some(Fault::Drop) => ok(json!(sig)),
                Some(Fault::FailOnchain) => {
                    enqueue(true, false);
                    ok(json!(sig))
                }
                Some(Fault::Reorg) => {
                    enqueue(false, true);
                    ok(json!(sig))
                }
            }
        }
        "getSignatureStatuses" => {
            let g = c.inner.lock();
            let vals: Vec<Value> = params[0]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|s| {
                    let s = s.as_str().unwrap_or("");
                    match g.txs.get(s) {
                        Some(t) if !t.hidden => {
                            let age = g.slot.saturating_sub(t.slot);
                            let cs = if age >= c.cfg.finalize_after { "finalized" } else if age >= c.cfg.confirm_after { "confirmed" } else { "processed" };
                            json!({"slot": t.slot, "confirmations": if cs == "finalized" { Value::Null } else { json!(age) }, "err": t.err, "confirmationStatus": cs})
                        }
                        _ => Value::Null,
                    }
                })
                .collect();
            ok(json!({"context":{"slot":g.slot},"value":vals}))
        }
        "getTransaction" => {
            let g = c.inner.lock();
            let s = params[0].as_str().unwrap_or("");
            match g.txs.get(s) {
                Some(t) if !t.hidden && g.slot.saturating_sub(t.slot) >= c.cfg.confirm_after => {
                    let idx = t.keys.len();
                    let tb = |v: &Vec<(String, String, u64, u8)>| -> Vec<Value> {
                        v.iter().enumerate().map(|(i, (o, m, a, d))| json!({"accountIndex": idx + i, "mint": m, "owner": o, "uiTokenAmount": {"amount": a.to_string(), "decimals": d}})).collect()
                    };
                    ok(json!({
                        "slot": t.slot, "blockTime": null,
                        "transaction": {"message": {"accountKeys": t.keys}},
                        "meta": {"fee": t.fee, "err": t.err, "preBalances": t.pre, "postBalances": t.post,
                                 "preTokenBalances": tb(&t.token_pre), "postTokenBalances": tb(&t.token_post),
                                 "computeUnitsConsumed": t.cu, "logMessages": ["Program log: MOCK"]}
                    }))
                }
                _ => ok(Value::Null),
            }
        }
        "requestAirdrop" => {
            let k: Pubkey = params[0]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default();
            c.fund(&k, params[1].as_u64().unwrap_or(0));
            ok(json!("MockAirdrop1111111111111111111111111111111111111111111111111111111111111111111111111"))
        }
        "getRecentPrioritizationFees" => ok(json!([{"slot": c.slot(), "prioritizationFee": 1000}])),
        _ => rpc_err(-32601, "method not found (MOCK)", Value::Null),
    }
}
