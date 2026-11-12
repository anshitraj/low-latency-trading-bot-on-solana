# Solana Telegram Trader (private beta)

Original Solana-only Telegram trading bot + Mini App. Rust execution engine
(sign-once, same-bytes multi-sender fan-out, independent landing observer,
durable idempotent state machine), Pump bonding-curve trading via the official
`pump-rust-client`, Telegram webhook bot, Next.js Mini App.

> **Status:** private-beta code, **unaudited, custodial test wallets, mainnet
> trading disabled by default.** Real Pump trades were verified on a local
> validator running the official Pump program — **not yet on devnet or mainnet.**
> See [docs/KNOWN_LIMITATIONS.md](docs/KNOWN_LIMITATIONS.md).

## What works (tested)
- Telegram: `/start`, wallet creation, paste-mint token panel, preset buy / 25–100% sell buttons, confirmation with full cost breakdown and max total spend, live status edits, positions, orders, settings, `/pause`. Replay-, double-tap- and spoof-safe callbacks.
- Mini App: Telegram initData sign-in (server-validated, single-use), trade + confirmation sheet, order tracker, portfolio (weighted-average PnL), orders, wallet tracker, settings + kill switch.
- Engine: integer money math, per-wallet hard limits, fail-closed stale state, pre-sign transaction guard, durable SIGNED before send, ambiguous-send handling, same-signature rebroadcast, block-height expiry, fork drop, restart reconciliation, zero duplicate executions under fault injection.
- Senders (same signed bytes fanned out in parallel): RPC and direct TPU/QUIC to the next leaders (both tested on localnet with the real Pump program); Jito and Helius Sender (tested against mocks; tipped mainnet transactions simulate cleanly; no mainnet send yet).

## Measured (this host, see [docs/BENCHMARKS.md](docs/BENCHMARKS.md))
- Offline quote→build→guard→sign→serialize: **p50 139 µs, p99 316 µs**.
- Engine intent→signed (in-memory store, cached state): **p50 203 µs / p99 459 µs** (1 wallet), p99 2.1 ms at 100 concurrent wallets.
- Engine on PostgreSQL, full request incl. durable SIGNED write (app + DB colocated on Linux, desktop disk): **p50 2.0 ms / p99 3.3–3.9 ms** (1 wallet), p50 3.9–4.3 ms / p99 6.6–8.0 ms (10 wallets); 50–100 concurrent orders miss the 15 ms p99 target.
- Localnet real `pump.so`: 20/20 FINALIZED, 85% landed within 1 slot, 100% within 2 (local validator — not mainnet). Re-run 2026-11-12: 20/20, 85%/100%.
- Mainnet / sender comparison / GCP region: **NOT MEASURED**.

## Quick start (mock mode, no network, no funds)
```bash
make setup          # cargo fetch + pnpm install + generate API types
make dev            # API + Mini App on http://127.0.0.1:8080 (MOCK chain)
make test           # 108 Rust tests + 16 Playwright tests
```
Real Pump program locally (Linux/macOS; Windows: `scripts/localnet-wsl.sh`):
```bash
scripts/localnet.sh                     # clones Pump config PDAs from devnet once, starts validator
make smoke-localnet                     # creates a coin and trades it 10×, verifies balances on-chain
```
Configuration: [.env.example](.env.example). Postgres without Docker: `make db-wsl` (WSL2, port 5433) then `make test-pg`; Supabase also works (see [docs/DATABASE.md](docs/DATABASE.md)).

## Repository
```
crates/domain          money, fees, intents, quotes, policy, state machine, PnL
crates/market-data     JSON-RPC client, token parsers, slot cache
crates/wallets         signers, envelope keystore
crates/execution       adapter trait, guard, sign-once builder, blockhash, locks, risk screen
crates/router          sender contract, tips, health/breaker, fan-out
crates/sender-rpc      RPC + Helius Sender      crates/sender-jito   Jito      crates/sender-tpu  (stub)
crates/protocol-pump   Pump bonding curve (v3) on pump-rust-client 0.5.0
crates/orders          engine, observer/reconciler, stores
crates/api             Axum API, Telegram bot, outbox, PgStore, wiring (binary: trader-api)
crates/observability   metrics, redaction
crates/testkit         MOCK chain + MOCK DEX (tests/mock mode only)
crates/cli             localnet tooling, real-program smoke trades (binary: trader-cli)
benchmarks/            trader-bench + results/*.json
tests/integration      end-to-end flows      tests/fault-injection   failure scenarios
apps/telegram-webapp   Next.js Mini App + Playwright
migrations/            PostgreSQL schema       fixtures/pump  captured account bytes
deploy/gcp/            Terraform (plan-only), VM files, deploy/stop/backup/region scripts
docs/                  see below
```

## Documentation
[Product requirements](docs/PRODUCT_REQUIREMENTS.md) ·
[Architecture](docs/ARCHITECTURE.md) ·
[Execution engine](docs/EXECUTION_ENGINE.md) ·
[RPC & routing](docs/RPC_AND_ROUTING.md) ·
[Security & custody](docs/SECURITY.md) ·
[API](docs/API.md) + [openapi.yaml](openapi.yaml) ·
[Database](docs/DATABASE.md) ·
[Testing](docs/TESTING.md) ·
[Benchmarks](docs/BENCHMARKS.md) ·
[GCP deployment](docs/GCP_DEPLOYMENT.md) ·
[Cost & shutdown](docs/COST_AND_SHUTDOWN.md) ·
[Runbook](docs/RUNBOOK.md) ·
[Known limitations](docs/KNOWN_LIMITATIONS.md) ·
[Progress](docs/PROGRESS.md) ·
[30-day plan](docs/30_DAY_DELIVERY_PLAN.md) ·
[Sources](docs/DEPENDENCIES_AND_SOURCES.md) ·
[Decisions](docs/DECISIONS/)

## Safety defaults
`TRADER_MODE=mock`; mainnet requires `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`
**and** Postgres **and** a tester allowlist **and** per-wallet `mainnet_enabled`.
No paid provider is assumed; Jito/Helius are opt-in and tip-funded by the trader.
No speed comparison with other products is claimed.
