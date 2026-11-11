//! Fault injection: ambiguous sends, drops, 429s, preflight rejections,
//! expiry, forks, delayed acks, crash/restart recovery and randomized
//! failure rates. Invariant everywhere: one intent -> at most one executed swap.

use std::sync::Arc;
use std::time::Duration;
use trader_domain::{AmountSpec, Lamports, OrderState, Side};
use trader_orders::{Engine, EngineConfig, Observer, ObserverConfig, OrderStore};
use trader_testkit::{ChainConfig, Fault};
use trader_tests::{Harness, Opts};

const T: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_500_after_forwarding_is_ambiguous_not_failed() {
    let h = Harness::new(Opts::default()).await;
    h.chain.push_fault(Fault::Http500Forwarded);
    let o = h.engine.submit(h.buy(10_000_000, "a"), h.wallet, None).await.unwrap();
    let after_send = h.store.get_order(o.id).await.unwrap();
    assert_ne!(after_send.state, OrderState::Rejected, "ambiguous send must never be reported as failed");
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized);
    assert_eq!(h.chain.landed_ok().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_tx_is_rebroadcast_with_same_signature() {
    let h = Harness::new(Opts::default()).await;
    h.chain.push_fault(Fault::Drop);
    let o = h.engine.submit(h.buy(10_000_000, "d"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized);
    assert!(h.chain.sends() >= 2, "rebroadcast happened");
    let attempts = h.store.attempts();
    let sig = &o.signed.unwrap().signature;
    assert!(attempts.iter().all(|a| &a.signature == sig), "every attempt used the same signature");
    assert!(attempts.iter().any(|a| a.rebroadcast));
    assert_eq!(h.chain.landed_ok().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limited_single_sender_rejects_without_fee() {
    let h = Harness::new(Opts::default()).await;
    let before = h.chain.balance(&h.wallet);
    h.chain.push_fault(Fault::RateLimit);
    let o = h.engine.submit(h.buy(10_000_000, "r"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.chain.balance(&h.wallet), before, "no fee charged");
    assert!(h.chain.landed_ok().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_chain_failure_charges_fee_and_records_no_position() {
    let h = Harness::new(Opts::default()).await;
    let before = h.chain.balance(&h.wallet);
    h.chain.push_fault(Fault::FailOnchain);
    let o = h.engine.submit(h.buy(10_000_000, "f"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::FailedOnchain);
    let fill = o.fill.expect("failure decoded");
    assert!(!fill.success);
    assert_eq!(fill.token_delta_raw, 0);
    assert_eq!(before - h.chain.balance(&h.wallet), fill.fee_lamports, "only the network fee was charged");
    assert!(h.store.get_position(h.wallet_id, h.mint).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_insufficient_funds_is_rejected_before_forwarding() {
    let h = Harness::new(Opts { fund_lamports: 20_000_000, ..Default::default() }).await;
    // Policy reserve off so the engine lets it through to preflight.
    h.policies.0.lock().min_sol_reserve = Lamports(0);
    // Drain the wallet behind the engine's back after quoting is impossible here,
    // so instead make the trade larger than balance minus rent: engine catches it.
    let o = h.engine.submit(h.buy(19_000_000, "pf"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    assert!(o.reject_reason.unwrap().contains("insufficient SOL"));
    assert_eq!(h.chain.sends(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_landing_tx_expires_by_block_height_and_cannot_land_later() {
    let h = Harness::new(Opts {
        chain: ChainConfig { blockhash_validity: 15, ..Default::default() },
        observer: ObserverConfig { poll_interval: Duration::from_millis(15), rebroadcast_interval: Duration::from_millis(50), max_rebroadcasts: 0 },
        ..Default::default()
    })
    .await;
    let before = h.chain.balance(&h.wallet);
    h.chain.push_fault(Fault::Drop);
    let o = h.engine.submit(h.buy(10_000_000, "e"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Expired);
    // Rebroadcasting the expired bytes now must not land.
    let rec = o.signed.unwrap();
    use base64::Engine as _;
    let _ = h.rpc.send_transaction(&base64::engine::general_purpose::STANDARD.decode(rec.wire_b64).unwrap(), true, Some(0), trader_domain::Commitment::Confirmed).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.chain.landed_ok().is_empty());
    assert_eq!(h.chain.balance(&h.wallet), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_drop_then_reland_executes_once() {
    let h = Harness::new(Opts::default()).await;
    h.chain.push_fault(Fault::Reorg);
    let o = h.engine.submit(h.buy(10_000_000, "fork"), h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized);
    assert_eq!(h.chain.landed_ok().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_ack_beyond_router_timeout_is_ambiguous_then_lands() {
    let h = Harness::new(Opts { sender_timeout: Duration::from_millis(100), ..Default::default() }).await;
    h.chain.push_fault(Fault::DelayAck(Duration::from_millis(400)));
    let o = h.engine.submit(h.buy(10_000_000, "slow"), h.wallet, None).await.unwrap();
    let a = h.store.attempts();
    assert!(matches!(a[0].attempt.outcome, trader_router::SendOutcome::Ambiguous { .. }));
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized);
    assert_eq!(h.chain.landed_ok().len(), 1);
}

/// Crash after SIGNED/SUBMITTED, tx lands while the process is down, restart
/// reconciles to FINALIZED with the real fill and never re-signs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovers_in_flight_order_without_resigning() {
    let h = Harness::new(Opts { start_observer: false, ..Default::default() }).await;
    let o = h.engine.submit(h.buy(10_000_000, "crash"), h.wallet, None).await.unwrap();
    let o = h.store.get_order(o.id).await.unwrap();
    assert_eq!(o.state, OrderState::Submitted);
    let sig = o.signed.clone().unwrap().signature;
    tokio::time::sleep(Duration::from_millis(300)).await; // lands while "down"
    let sends_before = h.chain.sends();

    // "Restart": new engine + observer over the same durable store.
    let (tx, _rx) = tokio::sync::broadcast::channel(64);
    let obs = Arc::new(Observer::new(h.store.clone(), h.rpc.clone(), h.locks.clone(), tx.clone(), ObserverConfig { poll_interval: Duration::from_millis(15), ..Default::default() }));
    let engine2 = Engine::new(
        EngineConfig { network: trader_domain::Network::Mock, ..Default::default() },
        h.store.clone(),
        vec![Arc::new(h.dex.clone())],
        h.router.clone(),
        h.blockhash.clone(),
        h.locks.clone(),
        h.signers.clone(),
        Arc::new(trader_orders::balances::RpcBalances { rpc: h.rpc.clone() }),
        h.policies.clone(),
        obs.handle.clone(),
        tx,
    );
    assert_eq!(engine2.recover().await.unwrap(), 1);
    assert_eq!(h.store.get_order(o.id).await.unwrap().state, OrderState::UnknownReconcile);
    obs.clone().spawn();
    let done = h.wait_terminal(o.id, T).await;
    assert_eq!(done.state, OrderState::Finalized);
    assert_eq!(done.signed.unwrap().signature, sig, "same signature after restart");
    assert!(done.fill.unwrap().success);
    assert_eq!(h.chain.sends(), sends_before, "recovered orders are never rebroadcast or re-signed");
    assert_eq!(h.chain.landed_ok().len(), 1);
}

/// Crash after SIGNED but the tx never reached the network: restart observes
/// until block-height expiry; it is never re-sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_with_never_sent_signed_order_expires() {
    let h = Harness::new(Opts { start_observer: false, chain: ChainConfig { blockhash_validity: 10, ..Default::default() }, ..Default::default() }).await;
    h.chain.push_fault(Fault::Drop);
    let o = h.engine.submit(h.buy(10_000_000, "never"), h.wallet, None).await.unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel(64);
    let obs = Arc::new(Observer::new(h.store.clone(), h.rpc.clone(), h.locks.clone(), tx.clone(), ObserverConfig { poll_interval: Duration::from_millis(15), ..Default::default() }));
    let engine2 = Engine::new(
        EngineConfig { network: trader_domain::Network::Mock, ..Default::default() },
        h.store.clone(),
        vec![Arc::new(h.dex.clone())],
        h.router.clone(),
        h.blockhash.clone(),
        h.locks.clone(),
        h.signers.clone(),
        Arc::new(trader_orders::balances::RpcBalances { rpc: h.rpc.clone() }),
        h.policies.clone(),
        obs.handle.clone(),
        tx,
    );
    engine2.recover().await.unwrap();
    obs.clone().spawn();
    let done = h.wait_terminal(o.id, T).await;
    assert_eq!(done.state, OrderState::Expired);
    assert!(h.chain.landed_ok().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_rejects_orders_that_never_signed() {
    let h = Harness::new(Opts::default()).await;
    let i = h.intent(Side::Buy, AmountSpec::ExactSolIn { lamports: Lamports(1_000_000) }, "x", true);
    let o = h.engine.submit(i, h.wallet, None).await.unwrap();
    h.wait_state(o.id, OrderState::AwaitingAuth, T).await;
    h.store.force_state(o.id, OrderState::Signing); // crash mid-signing
    h.engine.recover().await.unwrap();
    let r = h.store.get_order(o.id).await.unwrap();
    assert_eq!(r.state, OrderState::Rejected);
    assert_eq!(h.chain.sends(), 0);
}

/// Randomized: 10% injected 500-after-forward, 10% drops, 5% 429 across 40 orders.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn randomized_fault_mix_never_duplicates() {
    let h = Harness::new(Opts::default()).await;
    let mut ids = vec![];
    let mut rng_state: u64 = 0x9E3779B97F4A7C15;
    for i in 0..40 {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        match rng_state % 100 {
            0..=9 => h.chain.push_fault(Fault::Http500Forwarded),
            10..=19 => h.chain.push_fault(Fault::Drop),
            20..=24 => h.chain.push_fault(Fault::RateLimit),
            _ => {}
        }
        let o = h.engine.submit(h.buy(5_000_000, &format!("mix-{i}")), h.wallet, None).await.unwrap();
        // replay the same intent (duplicate webhook delivery)
        let dup = h.engine.submit(h.buy(5_000_000, &format!("mix-{i}")), h.wallet, None).await.unwrap();
        assert_eq!(o.id, dup.id);
        ids.push(o.id);
    }
    let mut finalized = 0;
    for id in &ids {
        let o = h.wait_terminal(*id, T).await;
        assert!(matches!(o.state, OrderState::Finalized | OrderState::Rejected), "{:?}", o.state);
        if o.state == OrderState::Finalized {
            finalized += 1;
        }
    }
    assert_eq!(h.chain.landed_ok().len(), finalized, "landed swaps == finalized orders (no duplicates)");
    let sigs: std::collections::HashSet<_> = h.chain.landed_ok().into_iter().collect();
    assert_eq!(sigs.len(), finalized);
}
