//! Read the cluster's rent rate and install it process-wide.

use crate::{RpcClient, RpcError};
use serde_json::json;

/// Query `getMinimumBalanceForRentExemption` for 0 and 1 bytes, check the
/// result has the expected `(128 + len) * rate` shape, and install the rate
/// via `trader_domain::fees::set_rent_lamports_per_byte`. Returns the rate.
pub async fn sync_rent(rpc: &RpcClient) -> Result<u64, RpcError> {
    let f0: u64 = rpc
        .call("getMinimumBalanceForRentExemption", json!([0]))
        .await?;
    let f1: u64 = rpc
        .call("getMinimumBalanceForRentExemption", json!([1]))
        .await?;
    let per_byte = f1.saturating_sub(f0);
    if per_byte == 0 || f0 != trader_domain::fees::ACCOUNT_STORAGE_OVERHEAD * per_byte {
        return Err(RpcError::Decode(format!(
            "unexpected rent schedule: f(0)={f0} f(1)={f1}"
        )));
    }
    trader_domain::fees::set_rent_lamports_per_byte(per_byte);
    Ok(per_byte)
}
