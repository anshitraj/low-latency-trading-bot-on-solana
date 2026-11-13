
// ---- trader-shred-observer local build patch (appended to ledger/src/shred.rs) ----
// Exposes the same Merkle erasure recovery Agave uses internally
// (`merkle::recover`), so the read-only observer can rebuild missing data
// shreds from coding shreds. Only applied to the checkout used to build the
// observer; the running agave-validator binary is not rebuilt.
pub fn recover_merkle_for_observer(
    shreds: Vec<Shred>,
    reed_solomon_cache: &ReedSolomonCache,
) -> Result<Vec<Shred>, Error> {
    Ok(merkle::recover(shreds, reed_solomon_cache)?
        .filter_map(Result::ok)
        .collect())
}
