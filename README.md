# Solana Telegram Trader

A Solana trading bot (Telegram bot + Mini App) built for speed: a Rust execution engine
that signs a Pump.fun trade in **0.2 ms**, puts it on the wire to the block leaders in
**~2 ms**, and — with a professional shred feed — sees it inside the block in **~50 ms
(best 13 ms)**. Every number below comes from real mainnet trades, verified on-chain.

> **Status:** private beta, **unaudited**, custodial test wallets, mainnet trading
> **off by default**. Do not use with funds you cannot lose.
> See [docs/KNOWN_LIMITATIONS.md](docs/KNOWN_LIMITATIONS.md).

## Results (mainnet, 162 verified trades, 2026-11-12/13)

| | result |
|---|---|
| order → signed | **0.2 ms** |
| order → sent to Jito / Helius / leaders' TPU | **~2 ms** |
| send → seen in the block (own node + our shred observer) | 99–137 ms median |
| send → seen in the block (**GetBlock shred stream**) | **54 ms median, 13 ms best, 25% under 40 ms** |
| landed / finalized / failed | 162 / 162 / 0 |
| cost of the whole test campaign | ~US$1.30 in fees and tips |

Full story, route comparison and an honest comparison with Axiom, GMGN, Photon and
BullX: **[docs/SPEED.md](docs/SPEED.md)**. Raw data: [docs/BENCHMARKS.md](docs/BENCHMARKS.md),
`benchmarks/results/`.

## How it is fast

```
Telegram / Mini App ──► engine (Rust)
                         │ quote from a live WebSocket account stream (no RPC round trip)
                         │ build + guard + sign once                       0.2 ms
                         ▼
                 same signed bytes, in parallel                            ~2 ms
          ┌──────────────┼───────────────────┐
     direct TPU/QUIC   Helius Sender       RPC (backup)
   (current + next     (staked, SWQoS,
    2 leaders)          ~US$0.0004 tip)
          └──────────────┼───────────────────┘
                         ▼
                   leader packs it into the block
                         ▼
   shred observer (our code, on our own Agave node) reads the trade
   straight out of the block data — fed by our node's Turbine stream or a
   pro shred feed (GetBlock) — before any RPC knows               ~50 ms
```

- **Sign once, send everywhere:** one signature, the same bytes to every route — never two
  different orders. Detection starts at the first route that may have forwarded it.
- **Competitive priority fee** (100,000 lamports default, same as GMGN/Photon's median) and
  a tight 100k compute-unit limit, so leaders rank us higher per lamport.
- **Own Solana RPC node** (Agave 4.4.1 built from source, 64 vCPU / 512 GB, Frankfurt):
  0.87 ms reads, and the base for the shred observer. Restarts from a persistent snapshot
  backup in ~25 min with no manual steps ([deploy/gcp/rpc-node](deploy/gcp/rpc-node/README.md)).
- **Shred observer** ([tools/shred-observer](tools/shred-observer/README.md)): copies
  incoming shreds read-only, rebuilds missing ones with Agave's erasure recovery, decodes
  Alpenglow block components, and reports the first shred carrying a watched signature.
- Safety: integer money math, per-wallet limits, pre-sign transaction guard, durable state
  before sending, ambiguous-send handling, same-signature rebroadcast, restart
  reconciliation; never "bought" before on-chain evidence.

## What works (tested)
- Telegram: `/start`, wallet creation, paste-mint token panel, preset buy / 25–100% sell,
  confirmation with full cost breakdown, live status (processing → submitted → executed →
  complete), positions, orders, settings, `/pause`. Replay-, double-tap- and spoof-safe.
- Mini App: Telegram initData sign-in, trade + confirmation sheet, order tracker,
  portfolio (PnL), orders, wallet tracker, settings + kill switch.
- Engine + senders: RPC, direct TPU/QUIC, Jito, Helius Sender — all used for the 162
  mainnet trades by `trader-cli mainnet-canary` (same engine as the bot). The Telegram
  bot itself has only traded in mock mode and on a local validator so far; its default
  route for new users is TPU + Helius staked + RPC.
- Tooling: `mainnet-canary` (route A/B, per-provider and shred timings, landing leader,
  block position, p50/p95/p99), `rpc-probe`, `send-probe`, on-chain competitor study.

## Quick start (mock mode, no network, no funds)
```bash
make setup          # cargo fetch + pnpm install + generate API types
make dev            # API + Mini App on http://127.0.0.1:8080 (MOCK chain)
make test           # 135 Rust tests + Playwright tests
```
Real Pump program locally (Linux/macOS; Windows: `scripts/localnet-wsl.sh`):
```bash
scripts/localnet.sh
make smoke-localnet
```
Configuration: [.env.example](.env.example). Mainnet test trades (owner only, tiny amounts):
[docs/RUNBOOK.md](docs/RUNBOOK.md).

## Repository
```
crates/domain          money, fees, intents, quotes, policy, state machine, PnL
crates/market-data     JSON-RPC pool (multi-provider), WebSocket streams, token parsers
crates/wallets         signers, envelope keystore
crates/execution       adapter trait, guard, sign-once builder, blockhash, locks
crates/router          sender contract, tips, health, fan-out (first-forward tracking)
crates/sender-rpc      RPC + Helius Sender    crates/sender-jito  Jito    crates/sender-tpu  direct QUIC
crates/protocol-pump   Pump bonding curve on pump-rust-client 0.5.0
crates/orders          engine, observer/reconciler, stores
crates/api             Axum API, Telegram bot, outbox, PgStore (binary: trader-api)
crates/cli             canary, probes, localnet tooling (binary: trader-cli)
tools/shred-observer   read-only shred decoder for our own Agave node
deploy/gcp/rpc-node    own RPC node: build, start, backup, boot automation
scripts/               bot_landing_study.py (competitors), logs_race.py (feed races)
apps/telegram-webapp   Next.js Mini App + Playwright
benchmarks/results     every measurement as JSON
docs/                  see below
```

## Documentation
[Speed](docs/SPEED.md) ·
[Benchmarks](docs/BENCHMARKS.md) ·
[Architecture](docs/ARCHITECTURE.md) ·
[Execution engine](docs/EXECUTION_ENGINE.md) ·
[RPC & routing](docs/RPC_AND_ROUTING.md) ·
[Security & custody](docs/SECURITY.md) ·
[API](docs/API.md) + [openapi.yaml](openapi.yaml) ·
[Database](docs/DATABASE.md) ·
[Testing](docs/TESTING.md) ·
[GCP deployment](docs/GCP_DEPLOYMENT.md) ·
[Own RPC node](deploy/gcp/rpc-node/README.md) ·
[Cost & shutdown](docs/COST_AND_SHUTDOWN.md) ·
[Runbook](docs/RUNBOOK.md) ·
[Known limitations](docs/KNOWN_LIMITATIONS.md) ·
[Progress](docs/PROGRESS.md)

## What is left
- Use the shred feed for the live "executed" status (needs leader-signature verification
  of shreds first, so fake packets cannot fake a fill).
- A professional shred feed in production (GetBlock US$300/month; Everstake ~US$199/month
  untested — a free 1-hour test is prepared).
- Staked sending beyond Helius SWQoS (e.g. Triton Jet), leader-timed sends.
- Production deployment of the bot, security review, non-custodial wallets.

## Safety defaults
`TRADER_MODE=mock`; mainnet requires `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`
**and** Postgres **and** a tester allowlist **and** per-wallet `mainnet_enabled`.
Tips (Helius ~US$0.0004/trade) are paid from the trader's own wallet and shown in the quote.
