//! PostgreSQL implementation of OrderStore + AppStore (sqlx, runtime queries).
//! Invariants are enforced by the schema too (migrations/0001_init.sql):
//! unique (user_id, idempotency_key), unique signature, legal transitions
//! trigger, append-only journals, non-negative amounts.

use crate::appstore::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use solana_pubkey::Pubkey;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::Row;
use std::str::FromStr;
use trader_domain::pnl::{Fill, Position};
use trader_domain::{
    AmountSpec, Network, OrderState, Quote, Side, TradeIntent, UsageSnapshot, WalletPolicy,
};
use trader_orders::{
    AttemptRecord, CreateOutcome, FillRecord, Order, OrderEvent, OrderStore, SignedRecord,
    StoreError,
};
use trader_wallets::{SealedKey, SignerKind};
use uuid::Uuid;

pub struct PgStore {
    /// Fully durable pool (`synchronous_commit` server default, normally on).
    /// Used for every write that can matter after a crash: SIGNED (must be
    /// durable before any send), all post-send transitions, fills, positions,
    /// rejections, app data.
    pub pool: PgPool,
    /// Pre-sign pool with `synchronous_commit = off`: only CREATED and
    /// QUOTED/claim writes. They are visible immediately but their WAL flush is
    /// deferred. Safe because WAL is flushed in order: the durable SIGNED commit
    /// (which happens after them) flushes them too, so a crash can only lose
    /// pre-sign rows of orders that were never signed — nothing could have been
    /// sent. Disable with `DATABASE_ASYNC_PRESIGN_COMMITS=false`.
    pub presign: PgPool,
    network: Network,
}

fn be<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}
fn ae<E: std::fmt::Display>(e: E) -> AppStoreError {
    AppStoreError::Backend(e.to_string())
}
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

fn signer_kind_str(k: SignerKind) -> &'static str {
    match k {
        SignerKind::Mock => "mock",
        SignerKind::ManagedLocal => "managed_local",
        SignerKind::Kms => "kms",
        SignerKind::External => "external",
    }
}
fn signer_kind_from(s: &str) -> SignerKind {
    match s {
        "mock" => SignerKind::Mock,
        "managed_local" => SignerKind::ManagedLocal,
        "kms" => SignerKind::Kms,
        _ => SignerKind::External,
    }
}

const ORDER_SELECT: &str = "SELECT o.id, o.user_id, o.wallet_id, o.wallet_pubkey, o.state, o.quote, o.reject_reason, o.created_at, o.updated_at, i.intent, t.signed, t.fill
    FROM orders o JOIN trade_intents i ON i.id = o.id LEFT JOIN transactions t ON t.order_id = o.id";

impl PgStore {
    /// Pool tuning (env, all optional):
    /// - `DATABASE_MAX_CONNECTIONS` (default 16), `DATABASE_MIN_CONNECTIONS` (default 4):
    ///   keep warm connections so the hot-path SIGNED write never pays a TCP/TLS/SCRAM
    ///   handshake (observed multi-second stalls opening new connections through the
    ///   WSL2 localhost relay; same risk with any remote DB such as Supabase).
    /// - `DATABASE_ACQUIRE_TIMEOUT_SECS` (default 10).
    /// - Per-acquire liveness ping disabled (saves one round trip per query); broken
    ///   connections surface as query errors and are replaced by the pool.
    ///
    /// For Supabase use the *session* pooler or direct connection (port 5432) with
    /// `sslmode=require`; with the *transaction* pooler (6543) add
    /// `statement-cache-capacity=0` to the URL. See docs/DATABASE.md.
    pub async fn connect(url: &str, network: Network) -> anyhow::Result<Self> {
        let num = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let async_presign =
            std::env::var("DATABASE_ASYNC_PRESIGN_COMMITS").as_deref() != Ok("false");
        // Size both pools to fit the server's max_connections (minus headroom
        // for migrations/admin/other clients). Exceeding it makes connection
        // opens fail under load (observed: pool timeouts at 2 x 64 > 100).
        let probe = PgPoolOptions::new().max_connections(1).connect(url).await?;
        let server_max: u32 = sqlx::query_scalar::<_, String>("SHOW max_connections")
            .fetch_one(&probe)
            .await?
            .parse()
            .unwrap_or(100);
        probe.close().await;
        let budget = server_max.saturating_sub(10).max(4);
        let want_main = num("DATABASE_MAX_CONNECTIONS", 16).max(1) as u32;
        let want_pre = if async_presign {
            num(
                "DATABASE_PRESIGN_MAX_CONNECTIONS",
                (want_main / 2).max(2) as u64,
            )
            .max(1) as u32
        } else {
            0
        };
        let (main_max, pre_max) = if want_main + want_pre > budget {
            let main = (budget * want_main / (want_main + want_pre)).max(2);
            let pre = if async_presign {
                budget.saturating_sub(main).max(1)
            } else {
                0
            };
            tracing::warn!(
                server_max,
                want_main,
                want_pre,
                main,
                pre,
                "database pools clamped to fit max_connections"
            );
            (main, pre)
        } else {
            (want_main, want_pre)
        };
        let min_conn = num("DATABASE_MIN_CONNECTIONS", 4) as u32;
        let opts = |max: u32| {
            PgPoolOptions::new()
                .max_connections(max)
                .min_connections(min_conn.min(max))
                .acquire_timeout(std::time::Duration::from_secs(num(
                    "DATABASE_ACQUIRE_TIMEOUT_SECS",
                    10,
                )))
                .idle_timeout(None)
                .test_before_acquire(false)
        };
        let pool = opts(main_max).connect(url).await?;
        sqlx::migrate!("../../migrations").run(&pool).await?;
        let presign = if async_presign {
            opts(pre_max)
                .after_connect(|conn, _meta| {
                    Box::pin(async move {
                        use sqlx::Executor;
                        conn.execute("SET synchronous_commit = off").await?;
                        Ok(())
                    })
                })
                .connect(url)
                .await?
        } else {
            pool.clone()
        };
        Ok(Self {
            pool,
            presign,
            network,
        })
    }

    fn row_to_order(r: &PgRow) -> Result<Order, StoreError> {
        let state: String = r.try_get("state").map_err(be)?;
        let intent: Json<TradeIntent> = r.try_get("intent").map_err(be)?;
        let quote: Option<Json<Quote>> = r.try_get("quote").map_err(be)?;
        let signed: Option<Json<SignedRecord>> = r.try_get("signed").map_err(be)?;
        let fill: Option<Json<FillRecord>> = r.try_get("fill").map_err(be)?;
        let wallet: String = r.try_get("wallet_pubkey").map_err(be)?;
        Ok(Order {
            id: r.try_get("id").map_err(be)?,
            user_id: r.try_get("user_id").map_err(be)?,
            wallet_id: r.try_get("wallet_id").map_err(be)?,
            wallet: Pubkey::from_str(&wallet).map_err(be)?,
            intent: intent.0,
            state: OrderState::from_str(&state).map_err(be)?,
            quote: quote.map(|q| q.0),
            signed: signed.map(|s| s.0),
            fill: fill.map(|f| f.0),
            reject_reason: r.try_get("reject_reason").map_err(be)?,
            created_at: r.try_get("created_at").map_err(be)?,
            updated_at: r.try_get("updated_at").map_err(be)?,
        })
    }

    async fn current_state(&self, id: Uuid) -> Result<OrderState, StoreError> {
        let s: Option<String> = sqlx::query_scalar("SELECT state FROM orders WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        OrderState::from_str(&s.ok_or(StoreError::NotFound)?).map_err(be)
    }
}

#[async_trait]
impl OrderStore for PgStore {
    async fn create_order(
        &self,
        intent: &TradeIntent,
        wallet: Pubkey,
    ) -> Result<CreateOutcome, StoreError> {
        if intent.network != self.network {
            return Err(StoreError::Backend(
                "intent network does not match store network".into(),
            ));
        }
        let buy = match intent.amount {
            AmountSpec::ExactSolIn { lamports } => lamports.0 as i64,
            _ => 0,
        };
        // Postgres stores microseconds; truncate so the locally built Order
        // equals what a later read returns.
        let now = chrono::SubsecRound::trunc_subsecs(chrono::Utc::now(), 6);
        // One round trip: the three inserts run in a single atomic statement.
        // ON CONFLICT DO NOTHING makes duplicates insert nothing (count = 0).
        let created: i64 = sqlx::query_scalar(
            "WITH i AS (
                INSERT INTO trade_intents (id, user_id, wallet_id, network, idempotency_key, intent, created_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT (user_id, idempotency_key) DO NOTHING
                RETURNING id),
             o AS (
                INSERT INTO orders (id, user_id, wallet_id, wallet_pubkey, network, side, mint, buy_lamports, state, created_at, updated_at)
                SELECT id, $2, $3, $8, $4, $9, $10, $11, 'CREATED', $7, $7 FROM i
                RETURNING id),
             e AS (
                INSERT INTO order_events (order_id, from_state, to_state, detail, at)
                SELECT id, NULL, 'CREATED', '{}'::jsonb, $7 FROM o
                RETURNING order_id)
             SELECT COUNT(*) FROM e",
        )
        .bind(intent.id)
        .bind(intent.user_id)
        .bind(intent.wallet_id)
        .bind(intent.network.as_str())
        .bind(&intent.idempotency_key)
        .bind(Json(intent))
        .bind(now)
        .bind(wallet.to_string())
        .bind(intent.side.as_str())
        .bind(intent.mint.to_string())
        .bind(buy)
        .fetch_one(&self.presign)
        .await
        .map_err(be)?;
        if created == 0 {
            let id: Uuid = sqlx::query_scalar(
                "SELECT id FROM trade_intents WHERE user_id = $1 AND idempotency_key = $2",
            )
            .bind(intent.user_id)
            .bind(&intent.idempotency_key)
            .fetch_one(&self.pool)
            .await
            .map_err(be)?;
            return Ok(CreateOutcome::Existing(self.get_order(id).await?));
        }
        // Built locally: no read-back round trip.
        Ok(CreateOutcome::Created(Order {
            id: intent.id,
            user_id: intent.user_id,
            wallet_id: intent.wallet_id,
            wallet,
            intent: intent.clone(),
            state: OrderState::Created,
            quote: None,
            signed: None,
            fill: None,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        }))
    }

    async fn get_order(&self, id: Uuid) -> Result<Order, StoreError> {
        let r = sqlx::query(&format!("{ORDER_SELECT} WHERE o.id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?
            .ok_or(StoreError::NotFound)?;
        Self::row_to_order(&r)
    }

    async fn list_orders(&self, user_id: Uuid, limit: usize) -> Result<Vec<Order>, StoreError> {
        let rows = sqlx::query(&format!(
            "{ORDER_SELECT} WHERE o.user_id = $1 ORDER BY o.created_at DESC LIMIT $2"
        ))
        .bind(user_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(Self::row_to_order).collect()
    }

    async fn list_unresolved(&self) -> Result<Vec<Order>, StoreError> {
        let rows = sqlx::query(&format!("{ORDER_SELECT} WHERE o.state NOT IN ('FINALIZED','FAILED_ONCHAIN','EXPIRED','REJECTED') AND o.network = $1"))
            .bind(self.network.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter().map(Self::row_to_order).collect()
    }

    async fn transition(
        &self,
        id: Uuid,
        from: OrderState,
        to: OrderState,
        detail: serde_json::Value,
    ) -> Result<(), StoreError> {
        from.transition(to)?;
        // CAS update + journal row in one atomic statement (one round trip).
        let r = sqlx::query(
            "WITH u AS (
                UPDATE orders SET state = $3, updated_at = now()
                WHERE id = $1 AND state = $2 RETURNING id)
             INSERT INTO order_events (order_id, from_state, to_state, detail)
             SELECT id, $2, $3, $4 FROM u",
        )
        .bind(id)
        .bind(from.as_str())
        .bind(to.as_str())
        .bind(Json(detail))
        .execute(&self.pool)
        .await
        .map_err(be)?;
        if r.rows_affected() == 0 {
            return Err(StoreError::Conflict {
                expected: from,
                found: self.current_state(id).await?,
            });
        }
        Ok(())
    }

    async fn quote_and_advance(
        &self,
        id: Uuid,
        quote: &Quote,
        detail: serde_json::Value,
        then: OrderState,
    ) -> Result<(), StoreError> {
        OrderState::Quoted.transition(then)?;
        let res: String = sqlx::query_scalar("SELECT order_quote_and_advance($1, $2, $3, $4)")
            .bind(id)
            .bind(Json(quote))
            .bind(Json(detail))
            .bind(then.as_str())
            .fetch_one(&self.presign)
            .await
            .map_err(be)?;
        match res.as_str() {
            "OK" => Ok(()),
            "NOT_FOUND" => Err(StoreError::NotFound),
            other => Err(StoreError::Conflict {
                expected: OrderState::Created,
                found: OrderState::from_str(other).map_err(be)?,
            }),
        }
    }

    async fn save_quote(&self, id: Uuid, quote: &Quote) -> Result<(), StoreError> {
        sqlx::query("UPDATE orders SET quote = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(Json(quote))
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn save_signed(&self, id: Uuid, s: &SignedRecord) -> Result<(), StoreError> {
        // SIGNING -> SIGNED, the transactions row (unique signature) and the
        // journal row in ONE atomic statement: either all persist or none.
        let res = sqlx::query_scalar::<_, i64>(
            "WITH u AS (
                UPDATE orders SET state = 'SIGNED', updated_at = now()
                WHERE id = $1 AND state = 'SIGNING' RETURNING id),
             t AS (
                INSERT INTO transactions (signature, order_id, network, signed, last_valid_block_height, tip_lamports)
                SELECT $2, id, $3, $4, $5, $6 FROM u RETURNING order_id),
             e AS (
                INSERT INTO order_events (order_id, from_state, to_state, detail)
                SELECT order_id, 'SIGNING', 'SIGNED', $7 FROM t RETURNING order_id)
             SELECT COUNT(*) FROM e",
        )
        .bind(id)
        .bind(&s.signature)
        .bind(self.network.as_str())
        .bind(Json(s))
        .bind(s.last_valid_block_height as i64)
        .bind(s.tip_lamports as i64)
        .bind(Json(serde_json::json!({"signature": s.signature, "last_valid_block_height": s.last_valid_block_height})))
        .fetch_one(&self.pool)
        .await;
        match res {
            Ok(1) => Ok(()),
            Ok(_) => Err(StoreError::Conflict {
                expected: OrderState::Signing,
                found: self.current_state(id).await?,
            }),
            Err(e) if is_unique_violation(&e) => Err(StoreError::DuplicateSignature),
            Err(e) => Err(be(e)),
        }
    }

    async fn reject(&self, id: Uuid, from: OrderState, reason: &str) -> Result<(), StoreError> {
        from.transition(OrderState::Rejected)?;
        let r = sqlx::query(
            "WITH u AS (
                UPDATE orders SET state = 'REJECTED', reject_reason = $3, updated_at = now()
                WHERE id = $1 AND state = $2 RETURNING id)
             INSERT INTO order_events (order_id, from_state, to_state, detail)
             SELECT id, $2, 'REJECTED', $4 FROM u",
        )
        .bind(id)
        .bind(from.as_str())
        .bind(reason)
        .bind(Json(serde_json::json!({"reason": reason})))
        .execute(&self.pool)
        .await
        .map_err(be)?;
        if r.rows_affected() == 0 {
            return Err(StoreError::Conflict {
                expected: from,
                found: self.current_state(id).await?,
            });
        }
        Ok(())
    }

    async fn record_attempts(&self, recs: &[AttemptRecord]) -> Result<(), StoreError> {
        if recs.is_empty() {
            return Ok(());
        }
        let rows: Vec<serde_json::Value> = recs
            .iter()
            .map(|a| {
                serde_json::json!({
                    "order_id": a.order_id,
                    "signature": a.signature,
                    "sender": a.attempt.sender.as_str(),
                    "endpoint": a.attempt.endpoint,
                    "outcome": a.attempt.outcome.code(),
                    "detail": a.attempt.outcome,
                    "latency_us": a.attempt.latency_us,
                    "rebroadcast": a.rebroadcast,
                    "wall_start": a.attempt.wall_start,
                })
            })
            .collect();
        sqlx::query(
            "INSERT INTO sender_attempts (order_id, signature, sender, endpoint, outcome, detail, latency_us, rebroadcast, wall_start)
             SELECT (a->>'order_id')::uuid, a->>'signature', a->>'sender', a->>'endpoint', a->>'outcome',
                    a->'detail', (a->>'latency_us')::bigint, (a->>'rebroadcast')::boolean, (a->>'wall_start')::timestamptz
             FROM jsonb_array_elements($1) AS a",
        )
        .bind(Json(serde_json::Value::Array(rows)))
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(())
    }

    async fn save_fill(&self, id: Uuid, f: &FillRecord) -> Result<(), StoreError> {
        sqlx::query("UPDATE transactions SET fill = $2, landed_slot = $3, fee_lamports = $4 WHERE order_id = $1")
            .bind(id)
            .bind(Json(f))
            .bind(f.slot as i64)
            .bind(f.fee_lamports as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        for (kind, v) in [("network", f.fee_lamports), ("tip", f.tip_lamports)] {
            if v > 0 {
                sqlx::query("INSERT INTO fee_records (order_id, kind, lamports) VALUES ($1,$2,$3)")
                    .bind(id)
                    .bind(kind)
                    .bind(v as i64)
                    .execute(&self.pool)
                    .await
                    .map_err(be)?;
            }
        }
        Ok(())
    }

    async fn usage_today(&self, wallet_id: Uuid) -> Result<UsageSnapshot, StoreError> {
        let day_start = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc();
        // One round trip. Loss is an approximation (documented): realized
        // losses on positions touched today.
        let r = sqlx::query(
            "SELECT COUNT(*)::bigint AS n,
                    COALESCE(SUM(buy_lamports), 0)::bigint AS t,
                    (SELECT COALESCE(-SUM(LEAST(realized_pnl, 0)), 0)::text
                       FROM positions WHERE wallet_id = $1 AND updated_at >= $2) AS loss
             FROM orders
             WHERE wallet_id = $1 AND state <> 'REJECTED' AND created_at >= $2",
        )
        .bind(wallet_id)
        .bind(day_start)
        .fetch_one(&self.pool)
        .await
        .map_err(be)?;
        let loss: String = r.try_get("loss").map_err(be)?;
        Ok(UsageSnapshot {
            trades_today: r.try_get::<i64, _>("n").map_err(be)? as u32,
            turnover_today: trader_domain::Lamports(r.try_get::<i64, _>("t").map_err(be)? as u64),
            realized_loss_today: trader_domain::Lamports(
                loss.parse::<u128>().unwrap_or(0).min(u64::MAX as u128) as u64,
            ),
        })
    }

    async fn apply_position_fill(
        &self,
        order_id: Uuid,
        user_id: Uuid,
        wallet_id: Uuid,
        mint: Pubkey,
        side: Side,
        fill: &Fill,
    ) -> Result<Option<Position>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        let lot = sqlx::query("INSERT INTO position_lots (order_id, wallet_id, mint, side, qty_raw, lamports, fees, rent) VALUES ($1,$2,$3,$4,$5::numeric,$6::numeric,$7::numeric,$8::numeric) ON CONFLICT (order_id) DO NOTHING")
            .bind(order_id)
            .bind(wallet_id)
            .bind(mint.to_string())
            .bind(side.as_str())
            .bind(fill.qty_raw.to_string())
            .bind(fill.lamports.to_string())
            .bind(fill.fees_lamports.to_string())
            .bind(fill.rent_lamports.to_string())
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        if lot.rows_affected() == 0 {
            tx.rollback().await.ok();
            return Ok(None); // already applied
        }
        let row = sqlx::query("SELECT qty_raw::text, cost_basis::text, realized_pnl::text, fees_paid::text, rent_locked::text, gross_bought::text, gross_sold::text, buys, sells FROM positions WHERE wallet_id = $1 AND mint = $2 FOR UPDATE")
            .bind(wallet_id)
            .bind(mint.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(be)?;
        let mut p = match row {
            Some(r) => pos_from_row(&r)?,
            None => Position::default(),
        };
        match side {
            Side::Buy => p.apply_buy(fill).map_err(be)?,
            Side::Sell => {
                p.apply_sell(fill).map_err(be)?;
            }
        }
        sqlx::query(
            "INSERT INTO positions (wallet_id, mint, user_id, qty_raw, cost_basis, realized_pnl, fees_paid, rent_locked, gross_bought, gross_sold, buys, sells, updated_at)
             VALUES ($1,$2,$3,$4::numeric,$5::numeric,$6::numeric,$7::numeric,$8::numeric,$9::numeric,$10::numeric,$11,$12, now())
             ON CONFLICT (wallet_id, mint) DO UPDATE SET qty_raw = EXCLUDED.qty_raw, cost_basis = EXCLUDED.cost_basis, realized_pnl = EXCLUDED.realized_pnl,
               fees_paid = EXCLUDED.fees_paid, rent_locked = EXCLUDED.rent_locked, gross_bought = EXCLUDED.gross_bought, gross_sold = EXCLUDED.gross_sold,
               buys = EXCLUDED.buys, sells = EXCLUDED.sells, updated_at = now()",
        )
        .bind(wallet_id)
        .bind(mint.to_string())
        .bind(user_id)
        .bind(p.qty_raw.to_string())
        .bind(p.cost_basis_lamports.to_string())
        .bind(p.realized_pnl_lamports.to_string())
        .bind(p.fees_paid_lamports.to_string())
        .bind(p.rent_locked_lamports.to_string())
        .bind(p.gross_bought_lamports.to_string())
        .bind(p.gross_sold_lamports.to_string())
        .bind(p.buys as i32)
        .bind(p.sells as i32)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(Some(p))
    }

    async fn get_position(
        &self,
        wallet_id: Uuid,
        mint: Pubkey,
    ) -> Result<Option<Position>, StoreError> {
        let row = sqlx::query("SELECT qty_raw::text, cost_basis::text, realized_pnl::text, fees_paid::text, rent_locked::text, gross_bought::text, gross_sold::text, buys, sells FROM positions WHERE wallet_id = $1 AND mint = $2")
            .bind(wallet_id)
            .bind(mint.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.map(|r| pos_from_row(&r)).transpose()
    }

    async fn list_positions(&self, wallet_id: Uuid) -> Result<Vec<(Pubkey, Position)>, StoreError> {
        let rows = sqlx::query("SELECT mint, qty_raw::text, cost_basis::text, realized_pnl::text, fees_paid::text, rent_locked::text, gross_bought::text, gross_sold::text, buys, sells FROM positions WHERE wallet_id = $1 ORDER BY updated_at DESC")
            .bind(wallet_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter()
            .map(|r| {
                let m: String = r.try_get("mint").map_err(be)?;
                Ok((Pubkey::from_str(&m).map_err(be)?, pos_from_row(r)?))
            })
            .collect()
    }

    async fn events(&self, id: Uuid) -> Result<Vec<OrderEvent>, StoreError> {
        let rows = sqlx::query("SELECT e.order_id, o.user_id, e.from_state, e.to_state, e.at, e.detail FROM order_events e JOIN orders o ON o.id = e.order_id WHERE e.order_id = $1 ORDER BY e.id")
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter()
            .map(|r| {
                let from: Option<String> = r.try_get("from_state").map_err(be)?;
                let to: String = r.try_get("to_state").map_err(be)?;
                let detail: Json<serde_json::Value> = r.try_get("detail").map_err(be)?;
                Ok(OrderEvent {
                    order_id: r.try_get("order_id").map_err(be)?,
                    user_id: r.try_get("user_id").map_err(be)?,
                    from: from
                        .map(|f| OrderState::from_str(&f))
                        .transpose()
                        .map_err(be)?,
                    to: OrderState::from_str(&to).map_err(be)?,
                    at: r.try_get("at").map_err(be)?,
                    detail: detail.0,
                })
            })
            .collect()
    }

    async fn find_by_signature(&self, sig: &str) -> Result<Option<Order>, StoreError> {
        let id: Option<Uuid> =
            sqlx::query_scalar("SELECT order_id FROM transactions WHERE signature = $1")
                .bind(sig)
                .fetch_optional(&self.pool)
                .await
                .map_err(be)?;
        match id {
            Some(id) => Ok(Some(self.get_order(id).await?)),
            None => Ok(None),
        }
    }
}

fn pos_from_row(r: &PgRow) -> Result<Position, StoreError> {
    let n = |i: usize| -> Result<u128, StoreError> {
        let s: String = r
            .try_get(i + if r.columns().len() == 10 { 1 } else { 0 })
            .map_err(be)?;
        s.parse().map_err(be)
    };
    let off = if r.columns().len() == 10 { 1 } else { 0 };
    let realized: String = r.try_get(2 + off).map_err(be)?;
    Ok(Position {
        qty_raw: n(0)?,
        cost_basis_lamports: n(1)?,
        realized_pnl_lamports: realized.parse().map_err(be)?,
        fees_paid_lamports: n(3)?,
        rent_locked_lamports: n(4)?,
        gross_bought_lamports: n(5)?,
        gross_sold_lamports: n(6)?,
        buys: r.try_get::<i32, _>(7 + off).map_err(be)? as u32,
        sells: r.try_get::<i32, _>(8 + off).map_err(be)? as u32,
    })
}

#[async_trait]
impl AppStore for PgStore {
    async fn upsert_telegram_user(
        &self,
        telegram_id: i64,
        username: Option<String>,
        beta_allowed: bool,
    ) -> Result<User, AppStoreError> {
        let r = sqlx::query("INSERT INTO users (id, telegram_id, username, beta_allowed) VALUES ($1,$2,$3,$4) ON CONFLICT (telegram_id) DO UPDATE SET username = EXCLUDED.username, beta_allowed = EXCLUDED.beta_allowed RETURNING id, telegram_id, username, beta_allowed, created_at")
            .bind(Uuid::new_v4())
            .bind(telegram_id)
            .bind(username)
            .bind(beta_allowed)
            .fetch_one(&self.pool)
            .await
            .map_err(ae)?;
        user_from(&r)
    }
    async fn get_user(&self, id: Uuid) -> Result<User, AppStoreError> {
        let r = sqlx::query(
            "SELECT id, telegram_id, username, beta_allowed, created_at FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(ae)?
        .ok_or(AppStoreError::NotFound)?;
        user_from(&r)
    }
    async fn user_by_telegram(&self, telegram_id: i64) -> Result<Option<User>, AppStoreError> {
        let r = sqlx::query("SELECT id, telegram_id, username, beta_allowed, created_at FROM users WHERE telegram_id = $1").bind(telegram_id).fetch_optional(&self.pool).await.map_err(ae)?;
        r.map(|r| user_from(&r)).transpose()
    }
    async fn create_wallet(
        &self,
        w: &Wallet,
        sealed: Option<&SealedKey>,
    ) -> Result<(), AppStoreError> {
        let mut tx = self.pool.begin().await.map_err(ae)?;
        let res = sqlx::query("INSERT INTO wallets (id, user_id, network, pubkey, signer_kind, label, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(w.id)
            .bind(w.user_id)
            .bind(w.network.as_str())
            .bind(&w.pubkey)
            .bind(signer_kind_str(w.signer_kind))
            .bind(&w.label)
            .bind(w.created_at)
            .execute(&mut *tx)
            .await;
        if let Err(e) = res {
            return Err(if is_unique_violation(&e) {
                AppStoreError::Conflict("wallet exists".into())
            } else {
                ae(e)
            });
        }
        if let Some(s) = sealed {
            sqlx::query("INSERT INTO wallet_key_metadata (wallet_id, version, kek_id, wrapped_dek, sealed_seed) VALUES ($1,$2,$3,$4,$5)")
                .bind(w.id)
                .bind(s.version as i16)
                .bind(&s.kek_id)
                .bind(&s.wrapped_dek)
                .bind(&s.sealed_seed)
                .execute(&mut *tx)
                .await
                .map_err(ae)?;
        }
        tx.commit().await.map_err(ae)
    }
    async fn list_wallets(&self, user_id: Uuid) -> Result<Vec<Wallet>, AppStoreError> {
        let rows = sqlx::query("SELECT id, user_id, network, pubkey, signer_kind, label, created_at FROM wallets WHERE user_id = $1 ORDER BY created_at").bind(user_id).fetch_all(&self.pool).await.map_err(ae)?;
        rows.iter().map(wallet_from).collect()
    }
    async fn get_wallet(&self, id: Uuid) -> Result<Wallet, AppStoreError> {
        let r = sqlx::query("SELECT id, user_id, network, pubkey, signer_kind, label, created_at FROM wallets WHERE id = $1").bind(id).fetch_optional(&self.pool).await.map_err(ae)?.ok_or(AppStoreError::NotFound)?;
        wallet_from(&r)
    }
    async fn sealed_key(&self, wallet_id: Uuid) -> Result<Option<SealedKey>, AppStoreError> {
        let r = sqlx::query("SELECT version, kek_id, wrapped_dek, sealed_seed FROM wallet_key_metadata WHERE wallet_id = $1").bind(wallet_id).fetch_optional(&self.pool).await.map_err(ae)?;
        r.map(|r| {
            Ok(SealedKey {
                version: r.try_get::<i16, _>("version").map_err(ae)? as u8,
                kek_id: r.try_get("kek_id").map_err(ae)?,
                wrapped_dek: r.try_get("wrapped_dek").map_err(ae)?,
                sealed_seed: r.try_get("sealed_seed").map_err(ae)?,
            })
        })
        .transpose()
    }
    async fn get_policy(&self, wallet_id: Uuid) -> Result<Option<WalletPolicy>, AppStoreError> {
        let v: Option<Json<WalletPolicy>> =
            sqlx::query_scalar("SELECT policy FROM risk_limits WHERE wallet_id = $1")
                .bind(wallet_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(ae)?;
        Ok(v.map(|j| j.0))
    }
    async fn set_policy(&self, wallet_id: Uuid, p: &WalletPolicy) -> Result<(), AppStoreError> {
        sqlx::query("INSERT INTO risk_limits (wallet_id, policy) VALUES ($1,$2) ON CONFLICT (wallet_id) DO UPDATE SET policy = EXCLUDED.policy, updated_at = now()").bind(wallet_id).bind(Json(p)).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn get_settings(&self, user_id: Uuid) -> Result<Option<UserSettings>, AppStoreError> {
        let v: Option<Json<UserSettings>> =
            sqlx::query_scalar("SELECT settings FROM user_settings WHERE user_id = $1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(ae)?;
        Ok(v.map(|j| j.0))
    }
    async fn set_settings(&self, user_id: Uuid, s: &UserSettings) -> Result<(), AppStoreError> {
        sqlx::query("INSERT INTO user_settings (user_id, settings) VALUES ($1,$2) ON CONFLICT (user_id) DO UPDATE SET settings = EXCLUDED.settings, updated_at = now()").bind(user_id).bind(Json(s)).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn create_session(
        &self,
        token_hash: &str,
        user_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<(), AppStoreError> {
        sqlx::query(
            "INSERT INTO telegram_sessions (token_hash, user_id, expires_at) VALUES ($1,$2,$3)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(ae)?;
        Ok(())
    }
    async fn session_user(&self, token_hash: &str) -> Result<Option<Uuid>, AppStoreError> {
        sqlx::query_scalar(
            "SELECT user_id FROM telegram_sessions WHERE token_hash = $1 AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(ae)
    }
    async fn remember_init_data(
        &self,
        hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, AppStoreError> {
        sqlx::query("DELETE FROM telegram_init_data_seen WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map_err(ae)?;
        let r = sqlx::query("INSERT INTO telegram_init_data_seen (hash, expires_at) VALUES ($1,$2) ON CONFLICT DO NOTHING").bind(hash).bind(expires_at).execute(&self.pool).await.map_err(ae)?;
        Ok(r.rows_affected() == 1)
    }
    async fn create_callback(&self, t: &CallbackToken) -> Result<(), AppStoreError> {
        sqlx::query("INSERT INTO callback_tokens (token, user_id, telegram_id, chat_id, action, single_use, expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(&t.token)
            .bind(t.user_id)
            .bind(t.telegram_id)
            .bind(t.chat_id)
            .bind(Json(&t.action))
            .bind(t.action.single_use())
            .bind(t.expires_at)
            .execute(&self.pool)
            .await
            .map_err(ae)?;
        Ok(())
    }
    async fn check_callback(
        &self,
        token: &str,
        telegram_id: i64,
        chat_id: i64,
    ) -> Result<CallbackCheck, AppStoreError> {
        let Some(r) = sqlx::query("SELECT telegram_id, chat_id, action, single_use, expires_at, used_at FROM callback_tokens WHERE token = $1").bind(token).fetch_optional(&self.pool).await.map_err(ae)? else {
            return Ok(CallbackCheck::Unknown);
        };
        let action: Json<CallbackAction> = r.try_get("action").map_err(ae)?;
        if r.try_get::<i64, _>("telegram_id").map_err(ae)? != telegram_id
            || r.try_get::<i64, _>("chat_id").map_err(ae)? != chat_id
        {
            return Ok(CallbackCheck::WrongUser);
        }
        if r.try_get::<DateTime<Utc>, _>("expires_at").map_err(ae)? <= Utc::now() {
            return Ok(CallbackCheck::Expired);
        }
        if r.try_get::<bool, _>("single_use").map_err(ae)? {
            // Atomic consume: only one concurrent delivery wins.
            let won = sqlx::query(
                "UPDATE callback_tokens SET used_at = now() WHERE token = $1 AND used_at IS NULL",
            )
            .bind(token)
            .execute(&self.pool)
            .await
            .map_err(ae)?;
            if won.rows_affected() == 0 {
                return Ok(CallbackCheck::Replayed(action.0));
            }
        }
        Ok(CallbackCheck::Ok(action.0))
    }
    async fn mark_update(&self, update_id: i64) -> Result<bool, AppStoreError> {
        let r = sqlx::query(
            "INSERT INTO telegram_updates (update_id) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(update_id)
        .execute(&self.pool)
        .await
        .map_err(ae)?;
        Ok(r.rows_affected() == 1)
    }
    async fn add_tracked(&self, t: &TrackedWallet) -> Result<(), AppStoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracked_wallets WHERE user_id = $1")
            .bind(t.user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(ae)?;
        if n >= 20 {
            return Err(AppStoreError::Conflict(
                "tracking limit (20) reached".into(),
            ));
        }
        sqlx::query("INSERT INTO tracked_wallets (user_id, address, label) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING").bind(t.user_id).bind(&t.address).bind(&t.label).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn remove_tracked(&self, user_id: Uuid, address: &str) -> Result<bool, AppStoreError> {
        let r = sqlx::query("DELETE FROM tracked_wallets WHERE user_id = $1 AND address = $2")
            .bind(user_id)
            .bind(address)
            .execute(&self.pool)
            .await
            .map_err(ae)?;
        Ok(r.rows_affected() > 0)
    }
    async fn list_tracked(&self, user_id: Uuid) -> Result<Vec<TrackedWallet>, AppStoreError> {
        let rows = sqlx::query("SELECT user_id, address, label, created_at FROM tracked_wallets WHERE user_id = $1 ORDER BY created_at").bind(user_id).fetch_all(&self.pool).await.map_err(ae)?;
        rows.iter()
            .map(|r| {
                Ok(TrackedWallet {
                    user_id: r.try_get("user_id").map_err(ae)?,
                    address: r.try_get("address").map_err(ae)?,
                    label: r.try_get("label").map_err(ae)?,
                    created_at: r.try_get("created_at").map_err(ae)?,
                })
            })
            .collect()
    }
    async fn enqueue(&self, m: &OutboxMessage) -> Result<bool, AppStoreError> {
        let r = sqlx::query("INSERT INTO outbox_notifications (id, chat_id, text, buttons, dedupe_key, next_attempt_at, edit_message_id) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (dedupe_key) DO NOTHING")
            .bind(m.id)
            .bind(m.chat_id)
            .bind(&m.text)
            .bind(Json(&m.buttons))
            .bind(&m.dedupe_key)
            .bind(m.next_attempt_at)
            .bind(m.edit_message_id)
            .execute(&self.pool)
            .await
            .map_err(ae)?;
        Ok(r.rows_affected() == 1)
    }
    async fn due_messages(&self, limit: usize) -> Result<Vec<OutboxMessage>, AppStoreError> {
        let rows = sqlx::query("SELECT id, chat_id, text, buttons, dedupe_key, attempts, next_attempt_at, edit_message_id FROM outbox_notifications WHERE sent_at IS NULL AND next_attempt_at <= now() ORDER BY next_attempt_at LIMIT $1")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(ae)?;
        rows.iter()
            .map(|r| {
                let b: Option<Json<serde_json::Value>> = r.try_get("buttons").map_err(ae)?;
                Ok(OutboxMessage {
                    id: r.try_get("id").map_err(ae)?,
                    chat_id: r.try_get("chat_id").map_err(ae)?,
                    text: r.try_get("text").map_err(ae)?,
                    buttons: b.map(|j| j.0).unwrap_or(serde_json::Value::Null),
                    dedupe_key: r.try_get("dedupe_key").map_err(ae)?,
                    attempts: r.try_get::<i32, _>("attempts").map_err(ae)? as u32,
                    next_attempt_at: r.try_get("next_attempt_at").map_err(ae)?,
                    edit_message_id: r.try_get("edit_message_id").map_err(ae)?,
                })
            })
            .collect()
    }
    async fn mark_sent(&self, id: Uuid, mid: Option<i64>) -> Result<(), AppStoreError> {
        sqlx::query("UPDATE outbox_notifications SET sent_at = now(), telegram_message_id = $2 WHERE id = $1").bind(id).bind(mid).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn mark_retry(
        &self,
        id: Uuid,
        at: DateTime<Utc>,
        err: &str,
    ) -> Result<(), AppStoreError> {
        sqlx::query("UPDATE outbox_notifications SET attempts = attempts + 1, next_attempt_at = $2, last_error = $3 WHERE id = $1").bind(id).bind(at).bind(err.chars().take(300).collect::<String>()).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn set_order_message(
        &self,
        order_id: Uuid,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), AppStoreError> {
        sqlx::query("INSERT INTO order_messages (order_id, chat_id, message_id) VALUES ($1,$2,$3) ON CONFLICT (order_id) DO NOTHING").bind(order_id).bind(chat_id).bind(message_id).execute(&self.pool).await.map_err(ae)?;
        Ok(())
    }
    async fn order_message(&self, order_id: Uuid) -> Result<Option<(i64, i64)>, AppStoreError> {
        let r = sqlx::query("SELECT chat_id, message_id FROM order_messages WHERE order_id = $1")
            .bind(order_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(ae)?;
        r.map(|r| {
            Ok((
                r.try_get("chat_id").map_err(ae)?,
                r.try_get("message_id").map_err(ae)?,
            ))
        })
        .transpose()
    }
    async fn audit(
        &self,
        user_id: Option<Uuid>,
        kind: &str,
        detail: serde_json::Value,
    ) -> Result<(), AppStoreError> {
        sqlx::query("INSERT INTO audit_events (user_id, kind, detail) VALUES ($1,$2,$3)")
            .bind(user_id)
            .bind(kind)
            .bind(Json(detail))
            .execute(&self.pool)
            .await
            .map_err(ae)?;
        Ok(())
    }
}

fn user_from(r: &PgRow) -> Result<User, AppStoreError> {
    Ok(User {
        id: r.try_get("id").map_err(ae)?,
        telegram_id: r.try_get("telegram_id").map_err(ae)?,
        username: r.try_get("username").map_err(ae)?,
        beta_allowed: r.try_get("beta_allowed").map_err(ae)?,
        created_at: r.try_get("created_at").map_err(ae)?,
    })
}

fn wallet_from(r: &PgRow) -> Result<Wallet, AppStoreError> {
    let net: String = r.try_get("network").map_err(ae)?;
    let kind: String = r.try_get("signer_kind").map_err(ae)?;
    Ok(Wallet {
        id: r.try_get("id").map_err(ae)?,
        user_id: r.try_get("user_id").map_err(ae)?,
        network: Network::from_str(&net).map_err(ae)?,
        pubkey: r.try_get("pubkey").map_err(ae)?,
        signer_kind: signer_kind_from(&kind),
        label: r.try_get("label").map_err(ae)?,
        created_at: r.try_get("created_at").map_err(ae)?,
    })
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;

    /// Supabase connection strings we document must parse (no network).
    #[test]
    fn supabase_style_urls_parse() {
        // Shared pooler, session mode (IPv4, port 5432) — recommended from GCP.
        PgConnectOptions::from_str("postgres://postgres.abcdefghijklmnop:pw@aws-0-eu-central-1.pooler.supabase.com:5432/postgres?sslmode=require").unwrap();
        // Shared pooler, transaction mode (port 6543) — requires disabling the statement cache.
        PgConnectOptions::from_str("postgres://postgres.abcdefghijklmnop:pw@aws-0-eu-central-1.pooler.supabase.com:6543/postgres?sslmode=require&statement-cache-capacity=0").unwrap();
    }
}
