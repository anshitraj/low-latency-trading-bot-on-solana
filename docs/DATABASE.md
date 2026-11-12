# Database

PostgreSQL (tested on 18.6; any recent version expected to work), migrations in
`migrations/` (embedded via `sqlx::migrate!`, run at startup). Store:
`crates/api/src/pg.rs` — **integration-tested against real Postgres**
(`crates/api/tests/pg.rs`, 3 tests, plus the full Playwright suite with Postgres
as the backing store). Without `DATABASE_URL` the service uses in-memory stores
(mock/dev only; refused on mainnet).

## Where to run Postgres (no Docker required)
| option | use for | how | notes |
|---|---|---|---|
| WSL2 Ubuntu (Windows dev) | local dev + tests | `scripts/postgres-wsl.sh up` -> `postgres://trader:trader@127.0.0.1:5433/trader` | port 5433 avoids a native Windows Postgres on 5432; a hidden keepalive stops WSL from idle-terminating the distro (~60 s after the last `wsl.exe` exits), which otherwise kills Postgres mid-run (`os error 10053`); `down` releases it |
| Postgres on the GCP VM | private beta (fastest) | `deploy/gcp/vm/docker-compose.prod.yml`, or `apt install postgresql` on the VM | same host as the engine: no network hop on the synchronous SIGNED write |
| Supabase | managed alternative / staging | `DATABASE_URL` = **shared pooler, session mode** string (port 5432, IPv4) + `?sslmode=require` | direct connections are IPv6-only (IPv4 is a paid add-on) and GCP's default VPC is IPv4-only; for the **transaction** pooler (6543) append `&statement-cache-capacity=0`; run migrations through session mode; every pre-sign write pays the network round trip; verify current free-plan limits (storage, pausing of inactive projects). Not tried with a real Supabase project. |

Pool settings (env): `DATABASE_MAX_CONNECTIONS` (16, durable pool), `DATABASE_PRESIGN_MAX_CONNECTIONS`
(half of the durable pool, min 2), `DATABASE_MIN_CONNECTIONS` (4 per pool, kept warm),
`DATABASE_ACQUIRE_TIMEOUT_SECS` (10), `DATABASE_ASYNC_PRESIGN_COMMITS` (true). Both pools are
clamped at startup to the server's `max_connections` − 10 (a warning is logged). The per-acquire
liveness ping is disabled to save a round trip. Size the pools to the number of orders you expect
in flight at once (BENCHMARKS.md 2b: 16+8 queues beyond ~10 concurrent wallets).

### Two pools: async pre-sign commits, durable from SIGNED on
Writes before signing (intent + order creation, `CREATED→QUOTED→SIGNING`, rejections before
signing) go through a pool whose sessions run `SET synchronous_commit = off`: they return before
the WAL fsync. `SIGNED` and everything after it (transactions, send attempts, landing states,
fills, positions) go through the durable pool. This is safe because nothing has been signed or
sent before `SIGNED`, and PostgreSQL flushes WAL strictly in order, so a durable `SIGNED` commit
implies every earlier async commit is on disk. A crash can only lose orders that never reached
`SIGNED`; the client retry (same idempotency key) recreates them. Set
`DATABASE_ASYNC_PRESIGN_COMMITS=false` to make every write durable (one pool).

### Hot-path statements (one round trip each)
- create: one CTE inserts the intent (`ON CONFLICT DO NOTHING`), order and first event.
- `order_quote_and_advance(id, quote, detail, then)` (migration 0002): CAS `CREATED→QUOTED`, store
  quote, CAS to `SIGNING`/`AWAITING_AUTH`, two journal rows; returns `OK`, `NOT_FOUND` or the current state.
- transitions: `WITH u AS (UPDATE … WHERE state = $from RETURNING id) INSERT INTO order_events …`.
- `SIGNED`: one CTE (order CAS + `transactions` insert + event); unique violation → duplicate signature.
- send attempts: one `INSERT … SELECT FROM jsonb_array_elements($1)`.

## Entities
```
users ─┬─< wallets ─┬─ wallet_key_metadata (sealed seed, wrapped DEK, kek_id)
       │            ├─ risk_limits (WalletPolicy JSON)
       │            ├─< positions (weighted average) ─< position_lots (fill ledger, unique order_id)
       │            └─< balances (snapshot cache, P1)
       ├─ user_settings
       ├─< telegram_sessions (token hash)       callback_tokens (single-use buttons)
       ├─< trade_intents (unique user_id+idempotency_key) ─ orders ─┬─< order_events (append-only)
       │                                                            ├─ transactions (unique signature, 1 per order)
       │                                                            ├─< sender_attempts
       │                                                            ├─< fee_records
       │                                                            └─ order_messages (Telegram message to edit)
       ├─< tracked_wallets
       ├─< copy_rules ─< copy_executions (unique rule+source_signature+action)   [P1, unused]
       ├─< conditional_orders                                                    [P1, unused]
       └─< referral_codes ─< referral_earnings                                   [P2, unused]
telegram_updates (update_id dedupe), telegram_init_data_seen, outbox_notifications (unique dedupe_key), audit_events (append-only)
```

## Invariants enforced by the schema
- `UNIQUE (user_id, idempotency_key)` on `trade_intents`.
- `transactions.signature` primary key; `transactions.order_id` unique (one signed payload per order).
- `orders.state` CHECK of the 13 states + trigger `check_order_transition` mirroring the domain table.
- `order_events`, `audit_events`, `position_lots` append-only (UPDATE/DELETE raise).
- Non-negative CHECKs on amounts (`buy_lamports`, fees, tips, quantities, cost basis).
- `wallets (network, pubkey)` unique; every trade row carries `network`.
- `copy_executions (rule_id, source_signature, action)` unique (source-event dedupe, for P1).
- `outbox_notifications.dedupe_key` unique.

## Amounts
u64 values that fit BIGINT are stored as BIGINT; u128/i128 accounting (positions,
lots) as `NUMERIC(39,0)`/`NUMERIC(40,0)` read/written via text casts (no floats).

## PnL ledger
Method: **weighted average cost**. Buy: qty += tokens received; cost += SOL
debited excluding refundable rent (includes swap, protocol/creator fees, network
fee, tip). Sell: cost removed = cost × sold / qty (all cost when selling all);
realized += SOL credited (net of fees) − cost removed. Unrealized PnL is only
reported when an executable sell quote exists; otherwise `null` ("unavailable").
Fills come from `getTransaction` balance deltas, never from quotes.
Property test: any sequence of partial sells that closes the position removes
exactly the total cost (`crates/domain/src/pnl.rs`).

## Retention
`order_events`, `audit_events`, fills: keep (financial record). Sessions,
callback tokens, initData hashes, Telegram update ids: prune after expiry
(initData hashes pruned on insert; others: operator job — not automated yet).
Outbox: keep 30 days (not automated yet).

## Reconciliation
On startup `Engine::recover()` loads non-terminal orders for the configured
network: CREATED/QUOTED/SIGNING → REJECTED (nothing was ever sent);
SIGNED/SUBMITTED/OBSERVED → UNKNOWN_RECONCILE and observed until terminal
(never rebroadcast); CONFIRMED → observed to FINALIZED; AWAITING_AUTH kept
(approval re-quotes).
