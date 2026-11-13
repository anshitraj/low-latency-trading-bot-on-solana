//! Protocol fixture tests on REAL captured account bytes (localnet coin
//! created by the official pump.so; devnet curves). No network.

use base64::Engine;
use serde_json::Value;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;
use trader_domain::Side;
use trader_execution::protocol::{ProtocolAdapter, ProtocolError, QuoteRequest};
use trader_protocol_pump::{PumpAdapter, V3_USER_INDEX};

fn load(name: &str) -> (Pubkey, trader_protocol_pump::CurveSnapshot) {
    let p = format!("{}/../../fixtures/pump/{name}", env!("CARGO_MANIFEST_DIR"));
    let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let acct = |k: &str| {
        let a = &v["accounts"][k]["account"];
        let owner = Pubkey::from_str(a["owner"].as_str().unwrap()).unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(a["data"][0].as_str().unwrap())
            .unwrap();
        (owner, data)
    };
    let mint = Pubkey::from_str(v["mint"].as_str().unwrap()).unwrap();
    let (g, f, c, m) = (
        acct("global"),
        acct("fee_config"),
        acct("bonding_curve"),
        acct("mint"),
    );
    let ata: u64 = v["curve_base_ata_amount"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let snap = PumpAdapter::decode_snapshot(
        &mint,
        (&g.0, &g.1),
        (&f.0, &f.1),
        (&c.0, &c.1),
        (&m.0, &m.1),
        ata,
        v["slot"].as_u64().unwrap(),
    )
    .unwrap();
    (mint, snap)
}

fn req(mint: Pubkey, side: Side, amount: u64) -> QuoteRequest {
    QuoteRequest {
        mint,
        side,
        amount_in: amount,
        slippage_bps: 100,
        max_state_age: Duration::from_secs(3600),
    }
}

#[test]
fn localnet_curve_quotes_are_sane() {
    let (mint, snap) = load("localnet_curve.json");
    assert!(!snap.curve.complete);
    assert_eq!(snap.mint.decimals, 6);
    let a = PumpAdapter::offline();
    a.insert_snapshot(mint, snap);
    let small = a.quote_exact_in(&req(mint, Side::Buy, 10_000_000)).unwrap();
    let big = a
        .quote_exact_in(&req(mint, Side::Buy, 1_000_000_000))
        .unwrap();
    assert!(small.min_out <= small.expected_out);
    assert!(
        big.expected_out > small.expected_out,
        "more SOL buys more tokens"
    );
    // Larger order gets a worse average price (bonding curve convexity).
    assert!((big.expected_out as u128) * 10_000_000 < (small.expected_out as u128) * 1_000_000_000);
    assert!(big.price_impact_bps.unwrap() > small.price_impact_bps.unwrap());
    // Round trip loses to fees: selling what we'd buy returns less SOL.
    let back = a
        .quote_exact_in(&req(mint, Side::Sell, small.expected_out))
        .unwrap();
    assert!(
        back.expected_out < 10_000_000,
        "round trip must cost fees: {}",
        back.expected_out
    );
    assert!(small.protocol_fee_lamports.unwrap().0 > 0);
    // Compute-unit limit per side: sells never create accounts, so they ask for less.
    assert_eq!(
        small.compute_units,
        trader_protocol_pump::DEFAULT_COMPUTE_UNITS
    );
    assert_eq!(back.compute_units, trader_protocol_pump::SELL_COMPUTE_UNITS);
    assert!(
        trader_protocol_pump::SELL_COMPUTE_UNITS >= 50_382 * 125 / 100,
        "keep >=25% headroom over the measured max sell"
    );
    // builder: v3 buy with user at the IDL position
    let user = Pubkey::new_unique();
    let b = a.build_swap(&small, &user).unwrap();
    let ix = b
        .instructions
        .iter()
        .find(|i| i.program_id == trader_protocol_pump::PUMP_PROGRAM_ID)
        .unwrap();
    assert_eq!(ix.accounts[V3_USER_INDEX].pubkey, user);
    // zero amount rejected
    assert!(a.quote_exact_in(&req(mint, Side::Buy, 0)).is_err());
}

#[test]
fn stale_fixture_state_fails_closed() {
    let (mint, mut snap) = load("localnet_curve.json");
    snap.fetched_at = std::time::Instant::now() - Duration::from_secs(10);
    let a = PumpAdapter::offline();
    a.insert_snapshot(mint, snap);
    let mut r = req(mint, Side::Buy, 1_000_000);
    r.max_state_age = Duration::from_secs(2);
    assert!(matches!(a.quote_exact_in(&r), Err(ProtocolError::Stale(_))));
}

#[test]
fn devnet_fixtures_decode_and_classify() {
    for f in ["devnet_6Uw9_curve.json", "devnet_87xW_curve.json"] {
        let (mint, snap) = load(f);
        let complete = snap.curve.complete;
        let cashback = snap.curve.is_cashback_coin;
        let a = PumpAdapter::offline();
        a.insert_snapshot(mint, snap);
        let r = a.quote_exact_in(&req(mint, Side::Buy, 10_000_000));
        if complete || cashback {
            assert!(
                matches!(r, Err(ProtocolError::Unsupported { .. })),
                "{f}: {r:?}"
            );
        } else {
            let q = r.unwrap_or_else(|e| panic!("{f}: {e}"));
            assert!(q.expected_out > 0 && q.min_out <= q.expected_out);
        }
    }
}

#[test]
fn tampered_owner_is_rejected() {
    let p = format!(
        "{}/../../fixtures/pump/localnet_curve.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let data = |k: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(v["accounts"][k]["account"]["data"][0].as_str().unwrap())
            .unwrap()
    };
    let owner =
        |k: &str| Pubkey::from_str(v["accounts"][k]["account"]["owner"].as_str().unwrap()).unwrap();
    let mint = Pubkey::from_str(v["mint"].as_str().unwrap()).unwrap();
    let fake = Pubkey::new_unique();
    let r = PumpAdapter::decode_snapshot(
        &mint,
        (&owner("global"), &data("global")),
        (&owner("fee_config"), &data("fee_config")),
        (&fake, &data("bonding_curve")),
        (&owner("mint"), &data("mint")),
        0,
        1,
    );
    assert!(r.is_err(), "spoofed curve owner must be rejected");
    let mut bad = data("bonding_curve");
    bad[0] ^= 0xff; // wrong anchor discriminator
    let r = PumpAdapter::decode_snapshot(
        &mint,
        (&owner("global"), &data("global")),
        (&owner("fee_config"), &data("fee_config")),
        (&owner("bonding_curve"), &bad),
        (&owner("mint"), &data("mint")),
        0,
        1,
    );
    assert!(r.is_err(), "malformed curve must be rejected");
}
