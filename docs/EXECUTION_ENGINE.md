# Execution engine

Code: `crates/orders/src/engine.rs`, `observer.rs`; `crates/execution/*`; `crates/protocol-pump`.

## State machine (`crates/domain/src/state.rs`)
```
CREATED → QUOTED → AWAITING_AUTH ⇄ QUOTED → SIGNING → SIGNED → SUBMITTED → OBSERVED → CONFIRMED → FINALIZED
                 ↘ SIGNING                         ↘ REJECTED (all senders refused, nothing forwarded)
SUBMITTED/OBSERVED → FAILED_ONCHAIN | EXPIRED | UNKNOWN_RECONCILE;  OBSERVED → SUBMITTED (fork drop)
UNKNOWN_RECONCILE → SUBMITTED | OBSERVED | CONFIRMED | FINALIZED | FAILED_ONCHAIN | EXPIRED
CREATED/QUOTED/AWAITING_AUTH/SIGNING → REJECTED (validation, policy, quote, cancel, signing)
```
38 legal transitions (pinned by a test and mirrored by a Postgres trigger).
Invariants: anything possibly on the network (SIGNED…UNKNOWN_RECONCILE) can
never become REJECTED/cancelled; terminal = FINALIZED, FAILED_ONCHAIN, EXPIRED,
REJECTED; `SIGNING` never has a persisted signature, so a restart maps it to REJECTED.

## Pipeline
1. **Create** (`submit`): reserve queue capacity first (503 if full, nothing created), then `create_order` — idempotent on (user, idempotency key). Duplicates return the original order.
2. **Validate**: shape (`TradeIntent::validate_shape`), network equals engine network, mainnet gate, `WalletPolicy::check_intent` (paused/locked, order/turnover/trade-count caps, slippage, fee/tip caps, allowed senders).
3. **Quote**: adapter resolution (cached per mint) → `quote_exact_in` from cached state (refresh once if stale/missing) → costs: base fee 5,000/sig, priority fee `ceil(limit×price/1e6)` capped by min(user cap, policy cap), tip from the route plan, ATA rent if the user's account is missing, protocol-created account rent (`user_rent_candidates`), estimated protocol/creator fee. `max_total_debit` = max_in + all overhead.
4. **Auth**: `requires_approval` → AWAITING_AUTH. Approval re-checks under the wallet lock; an expired quote is re-quoted and refused if the new expected output is below the approved minimum.
5. **Sign once** under the per-wallet lock: fresh balances minus reservations → `check_quote` (expiry, mismatch, impact, slippage-inclusive order cap, reserve) → route plan (tip) → blockhash (≤30 s) → unique compute-unit limit per (wallet, blockhash) → `build_and_sign` (compile v0, guard, sign, serialize ≤1232 B) → **persist SIGNED** (signature unique) → reserve worst-case debit → release lock.
6. **Send**: same bytes to every sender concurrently. Any accepted/ambiguous outcome → SUBMITTED; only if every attempt was a definitive refusal (preflight error, 429, circuit open) → REJECTED and reservation released.
7. **Observe** (every 250 ms default): batch `getSignatureStatuses`; walk OBSERVED → CONFIRMED/FAILED_ONCHAIN → FINALIZED from on-chain status; on ≥confirmed fetch `getTransaction` and record fill (SOL delta incl. fee, token delta, fee, CU, created-account rent) and apply the position once per order. Missing after OBSERVED 3 polls → back to SUBMITTED. When **finalized** block height > `lastValidBlockHeight` and a history search still finds nothing → EXPIRED. While unobserved and valid, rebroadcast the same bytes every 2 s (max 30).
8. **Notify**: every transition emits an `OrderUpdate` → outbox (dedupe `order:<id>:<state>`).

## Duplicate prevention (defence in depth)
- Business: unique (user, idempotency key); Telegram buttons map to `tg:cb:<token>`; API requires `Idempotency-Key`.
- Callback tokens: single-use with atomic consume; replays return the original action → same order.
- State CAS on every transition; approval and processing are no-ops unless the state matches.
- One signature per order (unique constraint); never re-signed while a signature may land. Replacement transactions are not implemented at all.
- Identical-parameter orders in one blockhash window get distinct CU limits so they cannot collapse into one signature (regression-tested).
- Position fills idempotent per order (`position_lots.order_id` unique).

## Fees, compute and tips
- CU limit: adapter estimate (Pump: 150,000 = 2× max observed 73,933). CU price: user setting, capped so the priority fee ≤ caps. Dynamic fee estimation is not implemented.
- Tips: planned per sender set (`crates/router/src/tips.rs`): required-tip senders (Helius, Jito bundleOnly) force a tip ≥ their minimum or the order is refused if over the user's cap; two required senders with disjoint tip accounts are refused. The tip is the **last** instruction, so a failed swap pays no tip.

## Preflight
RPC sender runs with preflight on by default; `SKIP_PREFLIGHT=true` is refused on mainnet by config validation. Failed transactions that land still pay base + priority fee (shown in the fill).

## Pump adapter (`crates/protocol-pump`)
- Built on `pump-rust-client` 0.5.0 (official, MIT): IDL-generated builders, on-chain fee resolution (pump-fees tiers by market cap, per-coin creator fee).
- Instructions: `buy_exact_quote_in_v3` (spend exact SOL, min tokens), `sell_v3` (exact tokens, min SOL). Guard rule: only these discriminators (+`buy_v3`), and account #8 (`user`) must be the signing wallet.
- Accepts only SOL-quoted, non-complete, non-mayhem, non-cashback curves whose mint passes the risk screen. Owners checked: Global/curve owned by pump, FeeConfig by pump-fees, mint by a token program; Anchor discriminators checked by the SDK decoder.
- Quote state: `getMultipleAccounts` of global, fee config, curve, mint, curve ATA (both token programs) at `processed`.

## Known gaps
PumpSwap, Raydium, Jupiter, TPU sender, leader-aware routing, dynamic fees, streaming state, balance cache — see KNOWN_LIMITATIONS.md.
