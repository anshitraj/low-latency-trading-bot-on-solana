//! PgStore invariants against a real PostgreSQL. Runs only when
//! TEST_DATABASE_URL is set (CI provides one); otherwise prints SKIPPED.
//! Each test uses a fresh schema-isolated database state via unique ids.

use solana_pubkey::Pubkey;
use std::sync::Arc;
use trader_api::appstore::{
    AppStore, CallbackAction, CallbackCheck, CallbackToken, OutboxMessage, Wallet,
};
use trader_api::pg::PgStore;
use trader_domain::intent::FeePreference;
use trader_domain::pnl::Fill;
use trader_domain::{
    AmountSpec, Bps, Lamports, Network, OrderState, SenderKind, Side, TradeIntent,
};
use trader_orders::{CreateOutcome, OrderStore, SignedRecord, StoreError};
use trader_wallets::SignerKind;
use uuid::Uuid;

async fn store() -> Option<Arc<PgStore>> {
    let url = match std::env::var("TEST_DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("SKIPPED: TEST_DATABASE_URL not set (PgStore tests NOT RUN)");
            return None;
        }
    };
    Some(Arc::new(
        PgStore::connect(&url, Network::Mock)
            .await
            .expect("connect + migrate"),
    ))
}

async fn seed(s: &PgStore) -> (Uuid, Uuid, Pubkey) {
    let u = s
        .upsert_telegram_user(rand_id(), Some("pg".into()), true)
        .await
        .unwrap();
    let pk = Pubkey::new_unique();
    let w = Wallet {
        id: Uuid::new_v4(),
        user_id: u.id,
        network: Network::Mock,
        pubkey: pk.to_string(),
        signer_kind: SignerKind::ManagedLocal,
        label: "t".into(),
        created_at: chrono::Utc::now(),
    };
    s.create_wallet(&w, None).await.unwrap();
    (u.id, w.id, pk)
}

fn rand_id() -> i64 {
    (Uuid::new_v4().as_u128() % 1_000_000_000_000) as i64
}

fn intent(user: Uuid, wallet: Uuid, key: &str) -> TradeIntent {
    TradeIntent {
        id: Uuid::new_v4(),
        user_id: user,
        wallet_id: wallet,
        network: Network::Mock,
        side: Side::Buy,
        mint: Pubkey::new_unique(),
        amount: AmountSpec::ExactSolIn {
            lamports: Lamports(1_000),
        },
        slippage: Bps::new(100).unwrap(),
        fees: FeePreference::default(),
        senders: vec![SenderKind::Mock],
        idempotency_key: key.into(),
        requires_approval: false,
        created_at: chrono::Utc::now(),
    }
}

fn signed(sig: &str) -> SignedRecord {
    SignedRecord {
        signature: sig.into(),
        wire_b64: "AA==".into(),
        blockhash: "x".into(),
        last_valid_block_height: 10,
        compute_unit_limit: 1,
        compute_unit_price_micro_lamports: 0,
        tip_lamports: 0,
        tip_account: None,
        senders: vec![SenderKind::Mock],
        signed_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn idempotency_cas_signature_uniqueness_and_journal() {
    let Some(s) = store().await else { return };
    let (u, w, pk) = seed(&s).await;
    let i = intent(u, w, "k1");
    let a = s.create_order(&i, pk).await.unwrap();
    let mut i2 = i.clone();
    i2.id = Uuid::new_v4();
    let b = s.create_order(&i2, pk).await.unwrap();
    assert!(matches!(a, CreateOutcome::Created(_)));
    assert!(
        matches!(&b, CreateOutcome::Existing(o) if o.id == i.id),
        "same (user, key) returns the original"
    );

    // concurrent creates with the same key -> one row
    let key = format!("race-{}", Uuid::new_v4());
    let mut hs = vec![];
    for _ in 0..20 {
        let (s, mut it) = (s.clone(), intent(u, w, &key));
        it.id = Uuid::new_v4();
        hs.push(tokio::spawn(async move {
            s.create_order(&it, pk).await.unwrap().order().id
        }));
    }
    let ids: Vec<Uuid> = futures::future::join_all(hs)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert!(ids.iter().all(|x| *x == ids[0]));

    let id = i.id;
    s.transition(
        id,
        OrderState::Created,
        OrderState::Quoted,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    // stale CAS loses
    assert!(matches!(
        s.transition(
            id,
            OrderState::Created,
            OrderState::Quoted,
            serde_json::json!({})
        )
        .await,
        Err(StoreError::Conflict { .. })
    ));
    // illegal transition refused by domain check before touching the DB
    assert!(s
        .transition(
            id,
            OrderState::Quoted,
            OrderState::Finalized,
            serde_json::json!({})
        )
        .await
        .is_err());
    s.transition(
        id,
        OrderState::Quoted,
        OrderState::Signing,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let sig = format!("sig-{}", Uuid::new_v4());
    s.save_signed(id, &signed(&sig)).await.unwrap();
    assert_eq!(s.get_order(id).await.unwrap().state, OrderState::Signed);
    // the same signature for a different order is rejected
    let other = intent(u, w, "k2");
    s.create_order(&other, pk).await.unwrap();
    s.transition(
        other.id,
        OrderState::Created,
        OrderState::Quoted,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    s.transition(
        other.id,
        OrderState::Quoted,
        OrderState::Signing,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert!(matches!(
        s.save_signed(other.id, &signed(&sig)).await,
        Err(StoreError::DuplicateSignature)
    ));
    assert_eq!(
        s.get_order(other.id).await.unwrap().state,
        OrderState::Signing,
        "rolled back"
    );

    // DB trigger refuses illegal transitions even via raw SQL
    let r = sqlx::query("UPDATE orders SET state = 'CREATED' WHERE id = $1")
        .bind(id)
        .execute(&s.pool)
        .await;
    assert!(r.is_err(), "trigger must block SIGNED -> CREATED");
    // journal is append-only
    let r = sqlx::query("DELETE FROM order_events WHERE order_id = $1")
        .bind(id)
        .execute(&s.pool)
        .await;
    assert!(r.is_err(), "order_events must be append-only");
    let ev: Vec<OrderState> = s.events(id).await.unwrap().iter().map(|e| e.to).collect();
    assert_eq!(
        ev,
        vec![
            OrderState::Created,
            OrderState::Quoted,
            OrderState::Signing,
            OrderState::Signed
        ]
    );
    assert_eq!(s.find_by_signature(&sig).await.unwrap().unwrap().id, id);
    let usage = s.usage_today(w).await.unwrap();
    assert!(usage.trades_today >= 2);
}

#[tokio::test]
async fn position_fill_is_idempotent_per_order() {
    let Some(s) = store().await else { return };
    let (u, w, pk) = seed(&s).await;
    let i = intent(u, w, "pos");
    s.create_order(&i, pk).await.unwrap();
    let mint = Pubkey::new_unique();
    let f = Fill {
        qty_raw: 1_000,
        lamports: 5_000,
        fees_lamports: 10,
        rent_lamports: 0,
    };
    assert!(s
        .apply_position_fill(i.id, u, w, mint, Side::Buy, &f)
        .await
        .unwrap()
        .is_some());
    assert!(
        s.apply_position_fill(i.id, u, w, mint, Side::Buy, &f)
            .await
            .unwrap()
            .is_none(),
        "second apply is a no-op"
    );
    let p = s.get_position(w, mint).await.unwrap().unwrap();
    assert_eq!(
        (p.qty_raw, p.cost_basis_lamports, p.buys),
        (1_000, 5_000, 1)
    );
    assert_eq!(s.list_positions(w).await.unwrap().len(), 1);
}

#[tokio::test]
async fn callbacks_updates_outbox_sessions() {
    let Some(s) = store().await else { return };
    let (u, _w, _) = seed(&s).await;
    let user = s.get_user(u).await.unwrap();
    let tok = format!("t{}", Uuid::new_v4().simple());
    s.create_callback(&CallbackToken {
        token: tok.clone(),
        user_id: u,
        telegram_id: user.telegram_id,
        chat_id: user.telegram_id,
        action: CallbackAction::Buy {
            mint: "m".into(),
            lamports: 1,
        },
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(60),
        used_at: None,
    })
    .await
    .unwrap();
    assert_eq!(
        s.check_callback(&tok, user.telegram_id + 1, user.telegram_id)
            .await
            .unwrap(),
        CallbackCheck::WrongUser
    );
    // concurrent presses: exactly one Ok
    let mut hs = vec![];
    for _ in 0..10 {
        let (s, t, id) = (s.clone(), tok.clone(), user.telegram_id);
        hs.push(tokio::spawn(async move {
            s.check_callback(&t, id, id).await.unwrap()
        }));
    }
    let res: Vec<CallbackCheck> = futures::future::join_all(hs)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        res.iter()
            .filter(|r| matches!(r, CallbackCheck::Ok(_)))
            .count(),
        1
    );
    assert_eq!(
        res.iter()
            .filter(|r| matches!(r, CallbackCheck::Replayed(_)))
            .count(),
        9
    );

    let upd = rand_id();
    assert!(s.mark_update(upd).await.unwrap());
    assert!(!s.mark_update(upd).await.unwrap());

    let m = OutboxMessage {
        id: Uuid::new_v4(),
        chat_id: 1,
        text: "x".into(),
        buttons: serde_json::Value::Null,
        dedupe_key: format!("d-{}", Uuid::new_v4()),
        attempts: 0,
        next_attempt_at: chrono::Utc::now(),
        edit_message_id: None,
    };
    assert!(s.enqueue(&m).await.unwrap());
    let mut m2 = m.clone();
    m2.id = Uuid::new_v4();
    assert!(!s.enqueue(&m2).await.unwrap(), "dedupe key");

    let h = format!("h{}", Uuid::new_v4());
    s.create_session(&h, u, chrono::Utc::now() + chrono::Duration::seconds(60))
        .await
        .unwrap();
    assert_eq!(s.session_user(&h).await.unwrap(), Some(u));
    let ih = format!("i{}", Uuid::new_v4());
    assert!(s
        .remember_init_data(&ih, chrono::Utc::now() + chrono::Duration::seconds(60))
        .await
        .unwrap());
    assert!(!s
        .remember_init_data(&ih, chrono::Utc::now() + chrono::Duration::seconds(60))
        .await
        .unwrap());
}
