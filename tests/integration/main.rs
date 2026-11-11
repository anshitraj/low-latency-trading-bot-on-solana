//! Deterministic end-to-end flows against the MOCK chain. Real engine,
//! router, RPC sender, observer and store; only the chain is simulated.

use std::time::Duration;
use trader_domain::{AmountSpec, Bps, Lamports, OrderState, Side};
use trader_orders::OrderStore;
use trader_tests::{Harness, Opts};

const T: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buy_then_sell_full_lifecycle_with_reconciled_balances() {
    let mut h = Harness::new(Opts::default()).await;
    let sol_before = h.chain.balance(&h.wallet);

    let o = h
        .engine
        .submit(h.buy(100_000_000, "buy-1"), h.wallet, None)
        .await
        .unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized, "{:?}", o.reject_reason);
    let fill = o.fill.clone().expect("fill decoded from getTransaction");
    assert!(fill.success);
    assert_eq!(
        fill.token_delta_raw as u64,
        h.chain.token(&h.wallet, &h.mint),
        "fill matches on-chain token balance"
    );
    let sol_after = h.chain.balance(&h.wallet);
    assert_eq!(
        sol_before as i128 + fill.sol_delta_lamports,
        sol_after as i128,
        "fill SOL delta matches ledger"
    );
    assert!(fill.fee_lamports >= 5_000);
    let q = o.quote.unwrap();
    assert!(
        fill.token_delta_raw as u64 >= q.min_out,
        "received >= min_out"
    );
    assert!(
        (-fill.sol_delta_lamports) as u64 <= q.max_total_debit.0,
        "debit within max shown to user"
    );

    let pos = h
        .store
        .get_position(h.wallet_id, h.mint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pos.qty_raw, fill.token_delta_raw as u128);

    // Journal: CREATED -> QUOTED -> SIGNING -> SIGNED -> SUBMITTED -> OBSERVED -> ... -> FINALIZED
    let ev: Vec<OrderState> = h
        .store
        .events(o.id)
        .await
        .unwrap()
        .iter()
        .map(|e| e.to)
        .collect();
    assert_eq!(
        &ev[..5],
        &[
            OrderState::Created,
            OrderState::Quoted,
            OrderState::Signing,
            OrderState::Signed,
            OrderState::Submitted
        ]
    );
    assert_eq!(*ev.last().unwrap(), OrderState::Finalized);
    assert!(ev.contains(&OrderState::Observed));

    // Notifications never claim a fill before CONFIRMED.
    let mut saw_confirm = false;
    while let Ok(u) = h.updates.try_recv() {
        if matches!(u.state, OrderState::Confirmed | OrderState::Finalized) {
            saw_confirm = true;
        }
        if !saw_confirm {
            assert!(!u.detail.contains("filled"));
        }
    }

    // Sell 100%
    let sell = h.intent(
        Side::Sell,
        AmountSpec::SellPercent { bps: Bps::MAX },
        "sell-1",
        false,
    );
    let s = h.engine.submit(sell, h.wallet, None).await.unwrap();
    let s = h.wait_terminal(s.id, T).await;
    assert_eq!(s.state, OrderState::Finalized, "{:?}", s.reject_reason);
    assert_eq!(h.chain.token(&h.wallet, &h.mint), 0);
    let pos = h
        .store
        .get_position(h.wallet_id, h.mint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pos.qty_raw, 0);
    assert_eq!(pos.cost_basis_lamports, 0);
    assert!(
        pos.realized_pnl_lamports < 0,
        "round trip through 1% fee pool loses money: {}",
        pos.realized_pnl_lamports
    );
    assert_eq!(h.chain.landed_ok().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn fifty_duplicate_submissions_execute_once() {
    let h = Harness::new(Opts::default()).await;
    let intent = h.buy(50_000_000, "tg:cb:dup-123");
    let mut hs = vec![];
    for _ in 0..50 {
        let (e, mut i, w) = (h.engine.clone(), intent.clone(), h.wallet);
        i.id = uuid::Uuid::new_v4(); // a retried webhook builds a fresh intent id
        hs.push(tokio::spawn(async move {
            e.submit(i, w, None).await.unwrap().id
        }));
    }
    let ids: Vec<uuid::Uuid> = futures::future::join_all(hs)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert!(
        ids.iter().all(|i| *i == ids[0]),
        "all duplicates resolve to one order"
    );
    let o = h.wait_terminal(ids[0], T).await;
    assert_eq!(o.state, OrderState::Finalized);
    assert_eq!(h.store.order_count(), 1);
    assert_eq!(h.chain.landed_ok().len(), 1, "exactly one swap executed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_distinct_buys_never_overspend_reserve() {
    // 0.5 SOL wallet, reserve 0.01 SOL, 20 concurrent 0.1 SOL buys.
    let h = Harness::new(Opts {
        fund_lamports: 500_000_000,
        ..Default::default()
    })
    .await;
    let mut hs = vec![];
    for i in 0..20 {
        let (e, w) = (h.engine.clone(), h.wallet);
        let intent = h.buy(100_000_000, &format!("k{i}"));
        hs.push(tokio::spawn(async move {
            e.submit(intent, w, None).await.unwrap().id
        }));
    }
    let ids: Vec<uuid::Uuid> = futures::future::join_all(hs)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    let mut filled = 0;
    for id in ids {
        let o = h.wait_terminal(id, T).await;
        match o.state {
            OrderState::Finalized => filled += 1,
            OrderState::Rejected => {
                let r = o.reject_reason.unwrap();
                assert!(r.contains("insufficient SOL"), "unexpected reject: {r}")
            }
            s => panic!("unexpected {s}"),
        }
    }
    assert!((1..=4).contains(&filled), "filled {filled}");
    assert_eq!(h.chain.landed_ok().len(), filled);
    assert!(
        h.chain.balance(&h.wallet) >= 10_000_000,
        "reserve preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn awaiting_auth_approve_is_idempotent_and_cancel_is_safe() {
    let h = Harness::new(Opts::default()).await;
    let i = h.intent(
        Side::Buy,
        AmountSpec::ExactSolIn {
            lamports: Lamports(10_000_000),
        },
        "auth-1",
        true,
    );
    let o = h.engine.submit(i, h.wallet, None).await.unwrap();
    let o = h.wait_state(o.id, OrderState::AwaitingAuth, T).await;
    assert!(o.signed.is_none(), "nothing signed before approval");
    assert_eq!(h.chain.sends(), 0);
    // wrong user cannot approve
    assert!(h.engine.approve(o.id, uuid::Uuid::new_v4()).await.is_err());
    // 20 concurrent approvals (double-tapped button / replayed callback)
    let mut hs = vec![];
    for _ in 0..20 {
        let (e, id, u) = (h.engine.clone(), o.id, h.user_id);
        hs.push(tokio::spawn(
            async move { e.approve(id, u).await.map(|_| ()) },
        ));
    }
    for r in futures::future::join_all(hs).await {
        r.unwrap().unwrap();
    }
    let done = h.wait_terminal(o.id, T).await;
    assert_eq!(done.state, OrderState::Finalized);
    assert_eq!(h.chain.landed_ok().len(), 1);
    // cancel after landing is a no-op
    let c = h.engine.cancel(o.id, h.user_id).await.unwrap();
    assert_eq!(c.state, OrderState::Finalized);

    // cancel before approval
    let i2 = h.intent(
        Side::Buy,
        AmountSpec::ExactSolIn {
            lamports: Lamports(10_000_000),
        },
        "auth-2",
        true,
    );
    let o2 = h.engine.submit(i2, h.wallet, None).await.unwrap();
    h.wait_state(o2.id, OrderState::AwaitingAuth, T).await;
    let c2 = h.engine.cancel(o2.id, h.user_id).await.unwrap();
    assert_eq!(c2.state, OrderState::Rejected);
    assert_eq!(c2.reject_reason.as_deref(), Some("cancelled_by_user"));
    let after = h.engine.approve(o2.id, h.user_id).await.unwrap();
    assert_eq!(
        after.state,
        OrderState::Rejected,
        "approve after cancel does nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preview_has_full_costs_and_persists_nothing() {
    let h = Harness::new(Opts::default()).await;
    let q = h
        .engine
        .preview(&h.buy(100_000_000, "p1"), &h.wallet)
        .await
        .unwrap();
    assert_eq!(h.store.order_count(), 0);
    assert_eq!(h.chain.sends(), 0);
    assert!(q.min_out <= q.expected_out);
    assert_eq!(q.costs.base_fee, Lamports(5_000));
    assert_eq!(
        q.costs.rent_deposit,
        Lamports(2_039_280),
        "ATA rent shown when account missing"
    );
    assert!(q.max_total_debit.0 >= 100_000_000 + 5_000 + 2_039_280);
    assert!(
        matches!(q.source, trader_domain::QuoteSource::Mock),
        "mock quotes are labeled MOCK"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_violations_reject_without_sending() {
    let h = Harness::new(Opts::default()).await;
    // over per-order max
    let o = h
        .engine
        .submit(h.buy(2_000_000_000, "big"), h.wallet, None)
        .await
        .unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    assert!(o.reject_reason.unwrap().contains("exceeds per-order max"));
    // kill switch
    h.policies.0.lock().paused = true;
    let o = h
        .engine
        .submit(h.buy(1_000_000, "paused"), h.wallet, None)
        .await
        .unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    h.policies.0.lock().paused = false;
    // unknown mint
    let mut i = h.buy(1_000_000, "nomarket");
    i.mint = solana_pubkey::Pubkey::new_unique();
    let o = h.engine.submit(i, h.wallet, None).await.unwrap();
    assert_eq!(h.wait_terminal(o.id, T).await.state, OrderState::Rejected);
    // mainnet intent on a mock engine
    let mut m = h.buy(1_000_000, "mainnet");
    m.network = trader_domain::Network::Mainnet;
    let o = h.engine.submit(m, h.wallet, None).await.unwrap();
    assert_eq!(h.wait_terminal(o.id, T).await.state, OrderState::Rejected);
    assert_eq!(
        h.chain.sends(),
        0,
        "no rejected order ever reached a sender"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sell_more_than_balance_rejected_and_zero_liquidity_rejected() {
    let h = Harness::new(Opts::default()).await;
    let s = h.intent(Side::Sell, AmountSpec::ExactTokenIn { raw: 1 }, "s", false);
    let o = h.engine.submit(s, h.wallet, None).await.unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    h.dex.pools.write().get_mut(&h.mint).unwrap().0.sol_reserve = 0;
    let o = h
        .engine
        .submit(h.buy(1_000_000, "zl"), h.wallet, None)
        .await
        .unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Rejected);
    assert!(o.reject_reason.unwrap().contains("zero liquidity"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_queue_path_and_latency_trace() {
    let h = Harness::new(Opts {
        workers: true,
        ..Default::default()
    })
    .await;
    let o = h
        .engine
        .submit(
            h.buy(10_000_000, "q1"),
            h.wallet,
            Some(trader_domain::LatencyTrace::start()),
        )
        .await
        .unwrap();
    let o = h.wait_terminal(o.id, T).await;
    assert_eq!(o.state, OrderState::Finalized);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let traces = h.observer.traces.lock().clone();
    let (_, t, _) = traces
        .iter()
        .find(|(id, _, _)| *id == o.id)
        .expect("trace recorded");
    use trader_domain::Stage::*;
    for s in [
        IntentAccepted,
        QuoteReady,
        TxBuilt,
        Signed,
        SendStart,
        SenderAck,
        FirstObservedProcessed,
        Confirmed,
        Finalized,
    ] {
        assert!(t.us(s).is_some(), "missing stage {s:?}");
    }
    assert!(t.us(Signed).unwrap() <= t.us(SendStart).unwrap());
    assert!(t.us(SenderAck).unwrap() <= t.us(FirstObservedProcessed).unwrap());
}

/// Regression: two distinct orders with identical parameters in the same
/// blockhash window must produce distinct signatures and BOTH execute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_distinct_orders_get_distinct_signatures() {
    let h = Harness::new(Opts::default()).await;
    let a = h
        .engine
        .submit(h.buy(10_000_000, "same-a"), h.wallet, None)
        .await
        .unwrap();
    let b = h
        .engine
        .submit(h.buy(10_000_000, "same-b"), h.wallet, None)
        .await
        .unwrap();
    let a = h.wait_terminal(a.id, T).await;
    let b = h.wait_terminal(b.id, T).await;
    assert_eq!(
        (a.state, b.state),
        (OrderState::Finalized, OrderState::Finalized)
    );
    let (sa, sb) = (a.signed.unwrap(), b.signed.unwrap());
    assert_ne!(sa.signature, sb.signature);
    assert_eq!(h.chain.landed_ok().len(), 2);
}
