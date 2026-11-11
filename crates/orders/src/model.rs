use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use trader_domain::pubkey::b58;
use trader_domain::{Lamports, OrderState, Quote, TradeIntent};
use trader_router::SendAttempt;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedRecord {
    pub signature: String,
    pub wire_b64: String,
    pub blockhash: String,
    pub last_valid_block_height: u64,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    pub tip_lamports: u64,
    pub tip_account: Option<String>,
    pub senders: Vec<trader_domain::SenderKind>,
    pub signed_at: DateTime<Utc>,
}

/// On-chain-verified result of a landed transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FillRecord {
    pub slot: u64,
    pub success: bool,
    pub error: Option<String>,
    /// Network fee charged (meta.fee).
    pub fee_lamports: u64,
    /// Wallet SOL change including fee (negative for buys).
    pub sol_delta_lamports: i128,
    /// Wallet token change for the traded mint (raw units).
    pub token_delta_raw: i128,
    pub token_decimals: Option<u8>,
    pub compute_units_consumed: Option<u64>,
    pub tip_lamports: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Order {
    pub id: Uuid,
    pub user_id: Uuid,
    pub wallet_id: Uuid,
    #[serde(with = "b58")]
    pub wallet: Pubkey,
    pub intent: TradeIntent,
    pub state: OrderState,
    pub quote: Option<Quote>,
    pub signed: Option<SignedRecord>,
    pub fill: Option<FillRecord>,
    pub reject_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderEvent {
    pub order_id: Uuid,
    pub user_id: Uuid,
    pub from: Option<OrderState>,
    pub to: OrderState,
    pub at: DateTime<Utc>,
    pub detail: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum CreateOutcome {
    Created(Order),
    /// Same (user_id, idempotency_key) already exists: return it unchanged.
    Existing(Order),
}

impl CreateOutcome {
    pub fn order(&self) -> &Order {
        match self {
            CreateOutcome::Created(o) | CreateOutcome::Existing(o) => o,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptRecord {
    pub order_id: Uuid,
    pub signature: String,
    pub attempt: SendAttempt,
    pub rebroadcast: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletBalances {
    pub sol: Lamports,
    pub token_raw: Option<u64>,
    pub token_account_exists: bool,
    pub slot: u64,
}
