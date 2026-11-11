# Progress log

## 2026-11-11 — session 1 (day 1 of 30)

Mainnet status: **DISABLED**. GCP: **nothing provisioned**. Cash/SOL spent: **$0**.

### Done
- Verified current docs: Telegram initData + Bot API, Jito sendTransaction, Helius Sender (tiers, tip accounts), Jupiter Swap V2 (/order, /build), Pump public docs (v3 trades, fees on curve, end of cashback), GCP KMS Ed25519, GCP pricing (third-party, estimate).
- Rust workspace (16 crates): domain, market-data, wallets, execution, router, sender-rpc (+Helius mode), sender-jito, sender-tpu (stub), protocol-pump, orders, observability, api, testkit, cli, benchmarks, tests.
- Real Pump trades on a local validator with the official `pump.so` (20/20 FINALIZED, balances independently verified).
- Telegram webhook bot, REST API, PgStore + migrations, outbox, Next.js Mini App, Playwright E2E.
- Benchmarks (hot path, engine, load, faults), docs, GCP Terraform/scripts (not applied), CI workflow.

### Bugs found by tests and fixed
1. Two distinct orders with identical parameters in the same blockhash window serialized to identical bytes (one would have been silently dropped by the cluster). Fix: unique compute-unit limit per (wallet, blockhash); regression test.
2. Quotes under-stated max spend: Pump's first v3 trade creates a 137-byte `user_volume_accumulator` (1,844,400 lamports) paid by the user. Fix: `ProtocolAdapter::user_rent_candidates`, existence checked in the balance read; smoke asserts debit ≤ quoted max.
3. Helius tip-account constant truncated during transcription — caught by the parse test.
4. Position fills could double-apply after restart reconciliation. Fix: idempotent per order (`position_lots.order_id` unique / MemStore set).
5. Quadratic slowdowns (CU-uniqueness scan, MemStore usage scan) found by the engine benchmark at concurrency 1. Fixed to O(1).
6. Found while reading the Pump SDK fixture notes (not by a test): v3 trades refuse legacy cashback coins → the adapter now refuses them instead of only warning.

### Commands actually run (results)
```
cargo fmt --all -- --check                                   # pass
cargo clippy --workspace --all-targets -- -D warnings        # pass
cargo test --workspace                                       # 107 passed, 0 failed (+3 PgStore tests skipped: no DB)
cargo audit                                                  # pass (4 documented ignores)
bash scripts/secret-scan.sh                                  # clean
cd apps/telegram-webapp && pnpm typecheck && pnpm build      # pass
pnpm exec playwright test                                    # 16 passed
trader-cli clone-pump-accounts --rpc https://api.devnet.solana.com   # 37 accounts
scripts/localnet-wsl.sh ; trader-cli create-coin ; trader-cli smoke-pump --iterations 10   # 20/20 FINALIZED
trader-bench hotpath|engine|load|faults                      # see docs/BENCHMARKS.md
```

### Blockers / needs owner
- Docker Desktop VM did not start → Postgres tests NOT RUN. Start Docker Desktop (or provide a Postgres URL) and run `TEST_DATABASE_URL=… cargo test -p trader-api --test pg`.
- Telegram bot token + public HTTPS domain to test the real bot.
- GCP project id, billing account, and approval for `terraform apply`.
- Written approval (and ≤$20 budget) before any mainnet micro-trade.

### Next (per 30-day plan)
PgStore verification; devnet run; PumpSwap adapter; KMS-wrapped KEK; dynamic priority fees; WebSocket state/balance cache; wallet-tracking alerts; then authorized mainnet micro-trades and sender A/B.
