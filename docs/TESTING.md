# Testing

## Commands

| what | command | needs |
|---|---|---|
| format | `cargo fmt --all -- --check` | — |
| lint | `cargo clippy --workspace --all-targets -- -D warnings` | — |
| Rust unit + integration + fault injection | `cargo test --workspace` | — (mock chain in-process) |
| Postgres store invariants | `scripts/postgres-wsl.sh up` then `TEST_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader cargo test -p trader-api --test pg` (or `make test-pg`) | any Postgres (WSL script, VM, Supabase, CI service) |
| Mini App E2E on Postgres | `scripts/postgres-wsl.sh reset-e2e` then `E2E_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader_e2e pnpm exec playwright test` | Postgres |
| Mini App typecheck/build | `cd apps/telegram-webapp && pnpm typecheck && pnpm build` | Node 24, pnpm 10 |
| Mini App E2E | `cd apps/telegram-webapp && pnpm exec playwright test` | Chromium (`pnpm exec playwright install chromium`) |
| dependency audit | `cargo audit` (config: `.cargo/audit.toml`) | network |
| secret scan | `bash scripts/secret-scan.sh` (CI also runs gitleaks) | — |
| real Pump program on local validator | `scripts/localnet.sh` then `trader-cli create-coin` / `smoke-pump` | solana-test-validator (Linux/WSL) |

## Results from this session (2026-11-11)

| suite | result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy -D warnings` (all targets) | pass |
| `cargo test --workspace` | **108 passed, 0 failed** (the 3 PgStore tests skip without `TEST_DATABASE_URL`; run separately, below) |
| PgStore tests on PostgreSQL 18.6 (WSL2) | **3/3 passed**; then **89/89 consecutive suite runs** over 100 s, 0 failures |
| Playwright (desktop Chrome + Pixel 7) | **16 passed, 0 failed** |
| `cargo audit` | pass with 4 documented ignores (see KNOWN_LIMITATIONS) |
| secret scan | clean |
| localnet Pump smoke (real `pump.so`) | 20/20 trades FINALIZED and independently verified |
| Playwright with Postgres as the store | **16/16 passed** in 2 consecutive runs after the WSL keepalive fix (before it, runs longer than ~60 s failed when WSL stopped the distro) |
| `terraform validate/plan` | **NOT RUN** (terraform not installed; no GCP project provided) |
| devnet / mainnet trades | **NOT RUN** (devnet: faucet limits; mainnet: not authorized) |

### Per-suite breakdown (Rust)
- `trader-domain` 31: exact money parsing/formatting (incl. proptests: format/parse roundtrip, slippage bounds, portion ≤ balance), fee/rent math, wallet policy limits, order state machine (exhaustive legal/illegal transitions, pinned table size, restart mapping, user-status never says "filled" before CONFIRMED), weighted-average PnL with no cost drift (proptest), latency trace.
- `trader-market-data` 8: RPC error classification (429 / 5xx / timeout / JSON-RPC), transaction balance parsing incl. v0 loaded addresses, URL redaction, SPL/Token-2022 parsing with TLV extensions, truncated-TLV rejection, slot-ordered bounded cache.
- `trader-wallets` 5: mock signer refuses mainnet/devnet, network-scoped managed signer, external signer never signs, envelope encryption (wrong KEK / wallet binding / tamper all fail).
- `trader-execution` 8: sign-once deterministic bytes + verification, wrong payer/network, guard (unknown program, drain transfer, tip cap, priority-fee cap, extra signer, ATA for someone else, program rule), blockhash staleness, per-wallet lock serialization, reservations, token-extension risk screen.
- `trader-router` 9, `trader-sender-rpc` 3, `trader-sender-jito` 2, `trader-sender-tpu` 1: same-bytes fan-out, ambiguous vs definitive outcomes, signature mismatch, circuit breaker + fallback, local rate limit, tip planning (required/optional/incompatible/cap), Helius tip accounts parse (a wrong constant was caught here), Jito against a mock block engine.
- `trader-protocol-pump` 4 + 4 fixture tests: IDL discriminators match the SDK, v3 account order/user index, rent candidates, fail-closed without state; quotes on **real captured account bytes** (localnet coin, two devnet curves), staleness, spoofed owner and corrupted discriminator rejected.
- `tests/integration` 9: buy→sell full lifecycle with ledger-reconciled balances, 50 duplicate submissions → one swap, 20 concurrent distinct buys never overspend the reserve, approve idempotency + cancel, preview persists nothing, policy rejections never reach a sender, worker queue + latency stages, regression for identical orders in one blockhash window.
- `tests/fault-injection` 12: HTTP 500 after forwarding, dropped tx rebroadcast with the same signature, 429, on-chain failure (fee only, no position), preflight rejection, block-height expiry (and re-sent expired bytes never land), fork drop/re-land, delayed ack past router timeout, restart recovery (landed while down → FINALIZED without re-sign; never sent → EXPIRED; mid-signing → REJECTED), randomized fault mix.
- `trader-api` 5 unit + 5 integration: initData HMAC (valid, forged token, tampered user, expired, future, missing hash = initDataUnsafe-style, duplicate-key smuggling, oversize), config gates; webhook secret + update dedupe + group chats ignored; full Telegram flow with attacker pressing victim buttons, double-taps and replayed confirms → exactly one on-chain swap; beta allowlist; Mini App session replay/tamper/expiry and cross-user isolation (404); admin endpoints hidden; Telegram 429 retry_after.

### Playwright (both projects)
outside-Telegram screen; forged-token and expired initData rejected by the
server; invalid mint; first run → wallet → test SOL → buy with confirmation
sheet (max total spend) → FINALIZED → sell 50% → portfolio (1 position) and
orders (2 rows); cancel never signs; kill switch blocks orders;
not-implemented features labeled; tracker add.

## Boundaries
- **Mock** (`TRADER_MODE=mock`): in-process simulated chain and pool, labeled MOCK everywhere. Never used as evidence of real execution.
- **Localnet**: real Pump bytecode, cloned devnet config; validates builders, accounts, fees and the RPC/observer path; no competition.
- **Devnet**: supported by config (`TRADER_MODE=devnet`), not run this session.
- **Mainnet**: disabled by default (requires `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`, DB, allowlist, and per-wallet `mainnet_enabled`). Not run.
