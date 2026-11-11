# Benchmarks

Every number here was produced by code in this repo on the machine below and is
saved as JSON under `benchmarks/results/`. Nothing is estimated. Numbers are
**not** comparable to any third-party product; we have no comparable dataset
for GMGN, BullX or others and make no claim about them.

Host: `ANSHITRAJ-PC`, Windows 11 Pro, 20 logical CPUs, release build
(`opt-level=3, lto=thin, codegen-units=1`). Local validator ran in WSL2
Ubuntu on the same host. **No GCP VM was used** (no deployment authorized yet),
so region/RTT numbers are `NOT MEASURED`.

## Methodology

- Monotonic clock (`std::time::Instant`) for all durations within one process;
  wall clock recorded separately (`wall_start`) for correlation only.
- Stages (see `crates/domain/src/trace.rs`): `t_intent_accepted`,
  `t_quote_ready`, `t_tx_built`, `t_signed`, `t_send_start`, `t_sender_ack`,
  `t_first_observed_processed`, `t_confirmed`, `t_finalized`.
  `t_webhook_received`/`t_auth_ok` are defined but not yet stamped by the HTTP layer.
- A sender acknowledgement is never counted as landing; landing comes from
  `getSignatureStatuses` / `getTransaction` observed independently.
- Percentiles: HdrHistogram (3 significant digits) or nearest-rank on small samples.
- Warmup: 1,000 iterations discarded for the offline hot path.

## Results

### 1. Offline hot path — `trader-bench hotpath --iterations 20000`
Pump quote (official SDK math on a real captured localnet curve,
`fixtures/pump/localnet_curve.json`) → v3 instruction build → compile v0
message → pre-sign guard → ed25519 sign → serialize. No network, no DB.
File: `benchmarks/results/hotpath-20261111T165039Z.json`.

| stage | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| quote (`quote_exact_in`) | 1.2 µs | 2.4 µs | 4.5 µs | 182 µs |
| build swap ix (PDA derivations) | 100.6 µs | 123.9 µs | 236.5 µs | 549 µs |
| compile + guard + sign + serialize | 37.4 µs | 55.2 µs | 88.1 µs | 622 µs |
| **total** | **139 µs** | **182 µs** | **316 µs** | 733 µs |

Tx size: 744–787 bytes (limit 1232). The build step is dominated by ~10
`find_program_address` calls inside the SDK; caching static PDAs per
(mint, user) is a known, unimplemented optimization.

### 2. Engine warm path — `trader-bench engine --iterations 4000 --concurrency 1,10,50,100`
Real `Engine`: idempotent create (MemStore), policy, quote, guard, sign, persist
SIGNED, MOCK sender. Cached Pump state + cached balances. **Excludes RPC,
Postgres and KMS.** File: `benchmarks/results/engine-20261111T165614Z.json`.

| concurrency (wallets) | orders | intent→signed p50 | p90 | p95 | p99 | max |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 4000 | 203 µs | 252 µs | 295 µs | 459 µs | 801 µs |
| 10 | 4000 | 331 µs | 639 µs | 713 µs | 992 µs | 6.0 ms |
| 50 | 4000 | 707 µs | 1.24 ms | 1.41 ms | 2.16 ms | 6.4 ms |
| 100 | 4000 | 698 µs | 1.20 ms | 1.41 ms | 2.08 ms | 6.5 ms |

Target "p50 < 5 ms, p99 < 15 ms warm-cache intent→signed": **met in this
configuration** (in-memory store, cached state). With Postgres the SIGNED write
is a synchronous durable insert — **NOT MEASURED** (no Postgres available locally).

### 3. Real Pump program on a local validator — `trader-cli smoke-pump --iterations 10`
Official `pump.so` (pump-rust-client 0.5.0 artifacts) + admin PDAs cloned from
devnet, `solana-test-validator` 4.2 in WSL2, full engine → `RpcSender`
(`maxRetries=0`, preflight on) → independent observer (100 ms poll).
File: `benchmarks/results/localnet-pump-smoke.json`.

| metric | buys (n=10) | sells (n=10) |
|---|---|---|
| state | 10/10 FINALIZED | 10/10 FINALIZED |
| balances independently verified (RPC before/after vs decoded fill) | 10/10 | 10/10 |
| debit ≤ quoted max total spend | 10/10 (equal) | n/a |
| landed in ≤1 slot after send / ≤2 slots | 10 / 10 | 7 / 10 |
| intent→signed p50 / p95 (includes 1 RPC balance read) | 2.33 / 3.31 ms | 2.11 / 3.22 ms |
| signed→first observed p50 / p95 | 109 / 507 ms | 107 / 514 ms |
| compute units consumed | 73,933 first buy; 58,517 after | 47,425 |
| quote→fill slippage | 0 bps | 0 bps |
| network fee per tx | 5,150 lamports | 5,150 lamports |

Overall ≤1-slot share 85% (17/20), ≤2-slot 100%. Caveats: a local validator
has no competition, no leader rotation across machines and 400 ms slots; the
signed→observed figure is quantized by the 100 ms poll and slot boundaries.
**This is not mainnet landing performance.** CU limit is 150,000 (2× the max
observed).

The first buy debited 53,923,630 lamports for a 0.05 SOL buy: 50,000,000 swap +
2,074,080 Token-2022 ATA rent + 1,844,400 pump `user_volume_accumulator` rent +
5,150 fees. The 1.84 M lamport account was initially unaccounted in quotes; this
benchmark caught it and quotes now include it (`user_rent_candidates`).

### 4. Mock-chain load — `trader-bench load --rates 100,500,1000 --seconds 30`
File: `benchmarks/results/load-20261111T165757Z.json`. MOCK chain (20 ms slots,
finalize after 4 slots) — measures pipeline/observer behaviour, not network.

| rate (intents/min) | orders | FINALIZED | duplicates | intent→finalized p50 / p99 |
|---:|---:|---:|---:|---:|
| 100 | 50 | 50 | 0 | 94 / 124 ms |
| 500 | 250 | 250 | 0 | 98 / 123 ms |
| 1000 | 500 | 500 | 0 | 96 / 124 ms |

### 5. Fault injection — `trader-bench faults --rates 1,5,10 --orders 200`
Every request delivered twice (duplicate webhook). File:
`benchmarks/results/faults-20261111T165759Z.json`.

| fault rate | injected | FINALIZED | REJECTED | duplicate executions |
|---:|---|---:|---:|---:|
| 1% | 2× 429, 1× timeout-after-forward | 198 | 2 | **0** |
| 5% | 3 drop, 2 http500-forwarded, 3× 429, 2 timeout | 197 | 3 | **0** |
| 10% | 8 drop, 6 http500-forwarded, 3× 429, 2 timeout | 197 | 3 | **0** |

REJECTED orders are exactly the 429s with a single sender (never forwarded, no
fee). Dropped and ambiguous sends were rebroadcast with the same signature and
landed once.

## NOT MEASURED (and why)

| item | reason |
|---|---|
| Mainnet landing, sender A/B (RPC vs Jito vs Helius vs TPU) | Mainnet trading not authorized; TPU sender not implemented |
| GCP region RTT (Frankfurt vs Singapore) | No GCP deployment authorized; script ready: `deploy/gcp/scripts/region-ab.sh` |
| Telegram ingress→confirmation | Requires a real bot token + public HTTPS endpoint |
| Postgres SIGNED-write latency | No Postgres available on this host during the session |
| KMS signing overhead | KMS signer not implemented |
| Devnet landing | Not run (devnet faucet limits); localnet with the real program used instead |

## Reproduce
```
cargo run --release -p trader-benchmarks -- hotpath --iterations 20000
cargo run --release -p trader-benchmarks -- engine --iterations 4000 --concurrency 1,10,50,100
cargo run --release -p trader-benchmarks -- load --rates 100,500,1000 --seconds 30
cargo run --release -p trader-benchmarks -- faults --rates 1,5,10 --orders 200
scripts/localnet.sh   # (Windows: scripts/localnet-wsl.sh)
cargo run -p trader-cli -- create-coin
cargo run --release -p trader-cli -- smoke-pump --mint <MINT> --iterations 10 --report benchmarks/results/localnet-pump-smoke.json
```
