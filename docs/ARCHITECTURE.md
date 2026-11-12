# Architecture

One Rust process (`trader-api`) hosts the HTTP API, the Telegram webhook, the
order engine, the observer and the notification outbox. PostgreSQL holds
durable state. The Mini App is a static Next.js export served by the same
process (same origin → no CORS, bearer auth → no CSRF surface).

```
            Telegram servers                 Mini App (WebView)
                   │ HTTPS webhook                │ HTTPS (Bearer session)
                   ▼                              ▼
   ┌──────────────────────────── trader-api (crates/api) ─────────────────────────────┐
   │ /telegram/webhook ─secret check─▶ update dedupe ─▶ bounded queue ─▶ bot handlers │
   │ /v1/* REST ─session auth─▶ service layer (users, wallets, settings, tokens)       │
   │                         │                                                         │
   │                         ▼  TradeIntent (idempotency key)                          │
   │  ┌────────────── Engine (crates/orders) ───────────────┐                          │
   │  │ policy ─▶ quote (ProtocolAdapter, cached state)      │                          │
   │  │ ─▶ AWAITING_AUTH ─approve─▶ wallet lock ─▶ build     │                          │
   │  │ ─▶ guard ─▶ sign once ─▶ persist SIGNED (durable)    │──▶ Router (crates/router)│
   │  │ ─▶ reserve funds ─▶ fan-out same bytes               │    RPC | Jito | Helius   │
   │  └──────────────────────────────────────────────────────┘    (TPU: QUIC, unstaked)     │
   │  Observer: getSignatureStatuses ─▶ OBSERVED/CONFIRMED/FINALIZED/FAILED/EXPIRED    │
   │            getTransaction ─▶ fill + positions; rebroadcast same bytes              │
   │  OrderUpdate broadcast ─▶ outbox (durable, deduped) ─▶ Telegram sendMessage/edit   │
   └──────────────┬──────────────────────────────┬───────────────────────────────────┘
                  ▼                              ▼
             PostgreSQL                    Solana RPC (+ optional senders)
```

## Crates
| crate | responsibility |
|---|---|
| `domain` | exact money math, fees/rent, intents, quotes, wallet policy, order state machine, PnL, latency trace |
| `market-data` | JSON-RPC client (error classification), SPL/Token-2022 parsers, slot-stamped cache |
| `wallets` | signer trait (mock / managed / external), envelope keystore |
| `execution` | protocol adapter trait, pre-sign guard, sign-once builder, blockhash service, wallet locks + reservations, token risk screen |
| `router` | `TransactionSender` contract, tip planning, health/breaker, rate limiter, same-bytes fan-out, mock sender |
| `sender-rpc` | standard RPC sender + Helius Sender mode |
| `sender-jito` | Jito Block Engine `sendTransaction` |
| `sender-tpu` | placeholder (not implemented) |
| `protocol-pump` | Pump bonding-curve adapter on the official `pump-rust-client` |
| `orders` | engine pipeline, observer/reconciler, store contract + MemStore, RPC balances |
| `observability` | Prometheus text metrics, redaction |
| `api` | Axum routes, auth, Telegram bot, outbox, PgStore, wiring |
| `testkit` | MOCK chain (JSON-RPC server, fault injection) + MOCK DEX |
| `cli` | localnet setup, coin creation, real-program smoke trades, fixture capture |
| `benchmarks` | hot path, engine, load, fault-rate benchmarks |

## Trust boundaries
1. **Telegram → webhook**: authenticated by the secret-token header; payloads are untrusted input (size-limited, parsed defensively, private chats only).
2. **Mini App → API**: initData HMAC validated server-side, single-use, age-limited; then short-lived opaque session (hash stored).
3. **Users → engine**: every intent passes shape validation, per-wallet policy and quote checks server-side; UI checks are convenience only.
4. **Adapters/routers → signer**: the guard re-validates the compiled message (payer, signer count, program allowlist, system transfers only to tip accounts within cap, ATA ownership, priority-fee cap, Pump discriminator + user position) before any signature.
5. **RPC → engine**: RPC data is trusted for state but checked: account owners, Anchor discriminators, genesis hash vs configured network, returned signature must equal the local signature.
6. **Process → keys**: sealed seeds in DB; KEK from Secret Manager; decrypted signer cached in memory (see SECURITY.md).

## Order sequence (Telegram buy)
```
user taps "Buy 0.05" ─▶ callback token check (user+chat bound, single use)
  ─▶ intent(idempotency "tg:cb:<token>") ─▶ CREATED ─▶ QUOTED ─▶ AWAITING_AUTH
  ─▶ outbox: quote message with Confirm/Cancel (new single-use tokens)
user taps Confirm ─▶ engine.approve (CAS) ─▶ [re-quote if expired; refuse if worse than approved min]
  ─▶ SIGNING ─▶ SIGNED (sig + bytes persisted) ─▶ fan-out ─▶ SUBMITTED
observer ─▶ OBSERVED ─▶ CONFIRMED (+ getTransaction fill, position) ─▶ FINALIZED
outbox ─▶ edits the status message at each step; "filled" only from CONFIRMED.
```

## Hot path rules
- No DB reads while building; one durable DB write (SIGNED) before send.
- Market state and blockhash come from caches; quotes fail closed if state > 2 s old or blockhash > 30 s old.
- One RPC round trip remains for balances (`getMultipleAccounts`) — tracked as a limitation.
- Telegram, outbox, positions and analytics never block the trade path.

## Data flow (market data)
`ProtocolAdapter::refresh` → `getMultipleAccounts` (global, fee config, curve, mint, curve ATA) at `processed` → owner/discriminator checks → snapshot with slot + fetch time → in-memory quotes. Streaming subscriptions are not implemented.
