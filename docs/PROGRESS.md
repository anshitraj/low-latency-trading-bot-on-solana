# Progress log

## 2026-11-11 — session 1 (day 1 of 30)

Mainnet status: **DISABLED**. GCP: **nothing provisioned**. Cash/SOL spent: **$0**.

### Done
- Verified current docs: Telegram initData + Bot API, Jito sendTransaction, Helius Sender (tiers, tip accounts), Jupiter Swap V2 (/order, /build), Pump public docs (v3 trades, fees on curve, end of cashback), GCP KMS Ed25519, GCP pricing (third-party, estimate).
- Rust workspace (16 crates): domain, market-data, wallets, execution, router, sender-rpc (+Helius mode), sender-jito, sender-tpu (stub), protocol-pump, orders, observability, api, testkit, cli, benchmarks, tests.
- Real Pump trades on a local validator with the official `pump.so` (20/20 FINALIZED, balances independently verified).
- Telegram webhook bot, REST API, PgStore + migrations, outbox, Next.js Mini App, Playwright E2E.
- Benchmarks (hot path, engine, load, faults), docs, GCP Terraform/scripts (not applied), CI workflow.

### Bugs found and fixed (source noted)
1. Two distinct orders with identical parameters in the same blockhash window serialized to identical bytes (one would have been silently dropped by the cluster). Fix: unique compute-unit limit per (wallet, blockhash); regression test.
2. Quotes under-stated max spend: Pump's first v3 trade creates a 137-byte `user_volume_accumulator` (1,844,400 lamports) paid by the user. Fix: `ProtocolAdapter::user_rent_candidates`, existence checked in the balance read; smoke asserts debit ≤ quoted max.
3. Helius tip-account constant truncated during transcription — caught by the parse test.
4. Found in code review: position fills could double-apply after restart reconciliation. Fix: idempotent per order (`position_lots.order_id` unique / MemStore set).
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
- Telegram bot token + public HTTPS domain to test the real bot.
- GCP project id, billing account, and approval for `terraform apply`.
- Written approval (and ≤$20 budget) before any mainnet micro-trade.

### Next (per 30-day plan)
PgStore verification; devnet run; PumpSwap adapter; KMS-wrapped KEK; dynamic priority fees; WebSocket state/balance cache; wallet-tracking alerts; then authorized mainnet micro-trades and sender A/B.

## 2026-11-11/12 — session 1, part 2 (Postgres without Docker)
Owner asked not to use Docker Desktop. Decision: PostgreSQL inside the existing
WSL2 Ubuntu for dev/tests (`scripts/postgres-wsl.sh`), Postgres on the GCP VM for
the beta (fastest), Supabase documented as a managed alternative.

Findings:
- A native Windows PostgreSQL service already listens on 5432 → the WSL instance uses 5433 (the Windows service was not touched).
- Playwright on Postgres failed intermittently: WSL idle-terminates the distro ~60 s after the last `wsl.exe` exits (`os error 10053`, then pool timeouts). Fix: hidden keepalive in `postgres-wsl.sh up`. Verified: 89/89 PgStore suite runs over 100 s with no wsl calls; Playwright 16/16 twice.
- PgStore test reruns collided on `wallets(network, pubkey)` because `Pubkey::new_unique()` is a per-process counter; tests now use random keys (the constraint behaved correctly).
- Pool hardened: 4 warm connections, no per-acquire ping, 10 s acquire timeout, env-tunable.
- Measured the Postgres hot path (engine benchmark): intent→signed p50 5.3 ms with 1 wallet, 109 ms with 100 wallets → target missed; optimizations listed in BENCHMARKS.md 2b.

Commands run:
```
scripts/postgres-wsl.sh up
TEST_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader cargo test -p trader-api --test pg   # 3 passed; 89/89 in a 100 s loop
scripts/postgres-wsl.sh reset-e2e && E2E_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader_e2e pnpm exec playwright test   # 16 passed (x2)
trader-bench engine --iterations 2000 --concurrency 1,10,50,100 --database-url postgres://trader:trader@127.0.0.1:5433/trader_bench    # BENCHMARKS.md 2b
```

## 2026-11-12 — session 1, part 3 (Postgres hot-path latency)
Owner asked to cut DB round trips, then to check speed properly.

Done:
- Pre-sign DB round trips 5 → 2: single-CTE create, plpgsql `order_quote_and_advance` (migration 0002), CTE transitions / SIGNED / reject, one-statement batched send attempts, one-query daily usage.
- Second pool with `synchronous_commit=off` for pre-sign writes; SIGNED and later stay durable (safety argument in DATABASE.md). `DATABASE_ASYNC_PRESIGN_COMMITS=false` disables it.
- Pools clamped to server `max_connections` − 10; new `DATABASE_PRESIGN_MAX_CONNECTIONS`.
- Engine: a sent transaction is always handed to the observer (from SIGNED if the SUBMITTED write fails); reservation released when no sender forwarded.

Bugs found by the benchmark (and fixed):
- 2 × 64 pool connections > Postgres `max_connections=100` → connection opens failed under load; 16 orders rejected "pool timed out"; 10 sent orders left in SIGNED because the SUBMITTED write failed and the engine then skipped the observer. Fixed by the clamp + always-observe change. Not covered by an automated test yet (no failing-store harness).

Measured (BENCHMARKS.md 2b, validator stopped, 4 repeat runs):
- 1 wallet: accepted→signed p50 1.5 ms (was 5.3), p99 2.3–2.8 ms; full request incl. durable SIGNED p50 3.3–4.0 ms, p99 5–20 ms.
- 10 / 50 / 100 wallets with pools sized to load (48+32): request p50 6 / 19 / 38 ms.
- Colocated on Linux (bench + Postgres in one VM, BENCHMARKS.md 2c): 1 wallet request p50 2.0–2.1 ms, p99 3.3–3.9 ms; 10 wallets p50 3.9–4.3 ms, p99 6.6–8.0 ms (2 of 3 runs; the third was 2–4× slower from disk fsync swings).
- In-memory store: 0.20–0.51 ms p50 (engine CPU floor).
- Real Pump bytecode on localnet: 20/20 FINALIZED and verified; intent→signed p50 2.0 ms.
- Environment: WSL localhost relay adds ~0.16 ms per round trip and occasional multi-second stalls (pgbench through the relay: 5 s window with 6.4 ms avg, 20 ms stddev; one engine run aborted on a 10 s pool-acquire timeout; one Playwright-on-Postgres test failed once, passed on rerun).

Commands run:
```
cargo fmt --all --check; cargo clippy --workspace --all-targets -- -D warnings   # pass
cargo test --workspace                                                            # 113 passed, 0 failed
TEST_DATABASE_URL=postgres://trader:trader@127.0.0.1:5433/trader cargo test -p trader-api --test pg   # 5 passed
trader-bench engine --iterations 2000 --concurrency 1,10,50,100 --database-url ...trader_bench?sslmode=disable   # 4 runs
DATABASE_MAX_CONNECTIONS=48 DATABASE_PRESIGN_MAX_CONNECTIONS=32 trader-bench engine ... --concurrency 10,50,100   # 2 runs
trader-cli smoke-pump --mint <MINT> --iterations 10 --report benchmarks/results/localnet-pump-smoke-v2.json        # 20/20
pnpm exec playwright test                                    # 16 passed (memory store)
E2E_DATABASE_URL=...trader_e2e pnpm exec playwright test     # 15 passed, 1 failed once; isolated rerun 2/2; next full run 16/16
```

Not claimed: anything about GMGN/BullX or other bots (no comparable data); mainnet landing; cloud Postgres latency.

## 2026-11-12 — session 1, part 4 (mainnet canary tooling)
Owner wants a real mainnet test with ~US$1 (US$0.01 buys). Built `trader-cli keygen` and
`trader-cli mainnet-canary` (RUNBOOK "Mainnet canary"). Dry run is the default and cannot
broadcast (signatures zeroed, `simulateTransaction` with `sigVerify=false`); `--send` needs
the env acknowledgement plus a typed confirmation on an interactive terminal, so it is
run by the owner, not by an agent.

Verified on localnet with the real `pump.so` (`--localnet-test`): dry run simulated OK
(67,871 CU) and its signature was absent from the chain afterwards; 3 send round trips
→ 6/6 FINALIZED, balances independently verified, all landed 1 slot after send. Cost:
first buy −4,046,480 lamports (120,000 swap + 3,918,480 rent + 8,000 fee); later
round trips −18,965 lamports each.

Not done: mainnet run (owner action), close-token-account / sweep commands.

Mainnet dry run (same day, owner-funded canary wallet, public RPC, coin `6caZsnv4PgEhKUKEV1aPQHq94PQi5HyufAt1oaU7pump` on its bonding curve):
- Read-only first: our decoders read live mainnet Global/FeeConfig/BondingCurve; offline quote→build→sign p50 136 µs; a migrated coin was refused.
- First dry run was REJECTED by the per-wallet `mainnet_enabled` policy gate (fail-closed, as designed; `…084505Z.json`). The canary also inherited the 0.01 SOL default reserve, which a ~US$1 wallet cannot meet after rent. Canary policy is now explicit (per-wallet mainnet on, reserve = rent floor + 100,000 lamports, tips 0, daily loss ≤ 0.01 SOL).
- Then: simulation err=null, 69,726 / 69,728 CU, quote 120,000 lamports → 1,488,072,049 raw tokens (min 1,413,668,446). intent→signed 173 ms over the public RPC from this desktop (fresh state + balance reads), vs ~2 ms on localnet: remote RPC round trips dominate.
- Verified afterwards: wallet balance unchanged (13,000,000 lamports), signature not on chain.
- Observed: public RPC returns 429 after a burst of calls; mainnet now carries transaction version 1 (we send v0). Canary status polling default raised to 400 ms (`--poll-ms`).

## 2026-11-12 — session 1, part 5 (GCP VM + WebSocket streaming)
Owner approved a GCP VM (new project, c4-standard-4, Frankfurt). New project creation
worked but billing link failed ("Cloud billing quota exceeded"); owner chose
`vps-sandbox`; empty project deleted. Built `trader-vm` with its own VPC (IAP-only SSH)
and a ₹105,000 credit-burn budget (billing account currency is INR). See GCP_DEPLOYMENT.md.

Built WebSocket account streaming (market-data `ws.rs`, Pump adapter `with_stream`,
`StreamBalances`, API `WS_URL`/`WS_STREAMING`, canary `--no-ws`, `trader-bench live`).
Measured (BENCHMARKS.md 2d/2e): on the VM order→signed 0.204 ms p50 with the stream vs
24.7 ms with RPC reads; mainnet dry run from the desktop 0.22–0.45 ms (was 173 ms);
localnet streamed send loop 6/6 FINALIZED.

Issues found: the live bench first failed on the engine gate "mock sender not allowed on
mainnet" (kept; bench labels orders localnet). A leftover validator inside WSL survived
TaskStop and was killed mid-test by the next start (environment, documented).

## 2026-11-12 — session 1, part 6 (fast send path)
Owner asked for faster sending than RPC without paying much. Built: direct TPU QUIC sender
(own client: leader tracker, pre-warmed connections, fan-out to next 2 leaders, throwaway
TLS identity), Jito Frankfurt and Helius Frankfurt as canary senders with tips, signature
notifications that wake the observer, and `trader-cli send-probe` (read-only).
Measured from the VM: Helius FRA 1.43 ms, Jito FRA 2.27 ms, EU leaders' TPU 2.2 ms, public
RPC 28 ms. Localnet: TPU-only and RPC+TPU 12/12 FINALIZED; best signed→seen 24–39 ms.
Mainnet tipped dry runs OK. Not done: real mainnet sends (owner action), staked access,
choosing Jito region by leader location. Canary key not yet on the VM (owner stopped the copy; nothing was uploaded).

## 2026-11-12 — session 1, part 7 (first mainnet trades)
Owner stored a Helius RPC key on the VM (`~/.trader/env`, mode 600; never in chat) and ran
`mainnet-canary --senders tpu,jito --iterations 3 --send`. Result: 6/6 FINALIZED, balances
verified, independently confirmed on-chain; order→signed 0.20–0.23 ms; signed→handed to
senders 2.1–2.3 ms (first 6.8 ms); landed 1–2 slots after the pre-send slot read; 62,899
lamports consumed plus 2,860,040 lamports of recoverable rent.

What the run exposed: (1) mainnet now runs Alpenglow (finality target ~150 ms) and ~210 ms
slots, so docs assuming 400 ms / 12.8 s were corrected; (2) rent is 5,080 lamports/byte, our
hard-coded 6,960 over-estimated rent by ~37% (safe side) and is now read from the cluster;
(3) the observer first saw each trade already finalized, so it now stamps the WebSocket
notification time; (4) the TPU tracker measures slot time instead of assuming 400 ms.

Run 2 (owner-run): 6/6 landed and finalized on-chain; signed→landed (WebSocket processed)
184–298 ms; server-side order→senders ≈ 2.1–2.4 ms. Helius free plan returned 429s under
our polling load; an observer bug (FINALIZED order dropped before its fill was decoded)
stalled the canary on the last sell. Fixed with a regression test; observer RPC calls cut.
