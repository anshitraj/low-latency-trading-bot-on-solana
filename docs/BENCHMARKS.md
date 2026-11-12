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

Target "p50 < 5 ms, p99 < 15 ms warm-cache intent→signed": met with the
in-memory store at every concurrency; with PostgreSQL see 2b. A re-run on
2026-11-12 (`engine-20261112T065040Z.json`) agrees: 0.20 / 0.51 ms p50 at 1 / 100 wallets.

### 2b. Engine warm path on PostgreSQL — `trader-bench engine --database-url …`
Same engine, but every order write is a real PostgreSQL 18.6 write. Postgres runs
in WSL2 on the same host and is reached through WSL's localhost relay (so every
round trip crosses a VM boundary); fresh database per run; local validator
**stopped** during the run (it shares the virtual disk and skews fsync timing).

What each column measures (all from one monotonic clock inside `submit()`):
- **accepted→signed**: idempotent order row exists → transaction signed. Covers
  policy + daily-usage read, quote, `CREATED→QUOTED→SIGNING` (one plpgsql call), build, guard, sign.
- **request→send**: `submit()` entry → bytes handed to the sender. Adds the
  idempotent create and the **durable** `SIGNED` write (fsync) that must precede any send.

#### Before → after the round-trip work (1 wallet, same host)
| | before (`engine-pg-20261111T181806Z`) | after (4 runs, `engine-pg-20261112T064753Z`…`065005Z`) |
|---|---:|---:|
| DB round trips before signing | ~5 sequential | 2 (usage read, quote+advance) |
| accepted→signed p50 / p99 | 5.28 / 11.6 ms | **1.48–1.59 / 2.28–2.77 ms** |
| request→send p50 / p99 | not recorded | **3.30–4.03 / 4.99–20.3 ms** |

#### After, by concurrency (default pools 16 durable + 8 pre-sign; range over 4 runs)
| wallets | submitted | accepted→signed p50 | p99 | request→send p50 | p99 | throughput |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 8000/8000 | 1.48–1.59 ms | 2.3–2.8 ms | 3.3–4.0 ms | 5.0–20.3 ms | 72–178/s |
| 10 | 8000/8000 | 1.68–1.96 ms | 3.0–3.5 ms | 10.4–15.8 ms | 17–132 ms | 247–445/s |
| 50 | 8000/8000 | 14–21 ms | 37–70 ms | 38–62 ms | 93–505 ms | 361–557/s |
| 100 | 8000/8000 | 21–49 ms | 51–127 ms | 43–109 ms | 89–606 ms | 411–1138/s |

#### Pool sized to concurrency (`DATABASE_MAX_CONNECTIONS=48`, `DATABASE_PRESIGN_MAX_CONNECTIONS=32`; 2 runs, `engine-pg-20261112T065032Z`, `…065039Z`)
| wallets | accepted→signed p50 / p99 | request→send p50 / p99 | throughput |
|---:|---:|---:|---:|
| 10 | 2.27–2.33 / 4.1–5.2 ms | 5.8–6.1 / 10.2–11.8 ms | 865–907/s |
| 50 | 6.2–6.5 / 23–75 ms | 18.2–19.0 / 110–128 ms | 1270–1309/s |
| 100 | 15.5–15.7 / 96–105 ms | 37.1–39.1 / 207–209 ms | 1282–1305/s |

Controlled check at 10 wallets as the *first* level of a fresh process, 2 runs per
pool size (`engine-pg-20261112T065058Z`…`065107Z`): request→send p50 6.3–6.5 ms
(16+8) vs 6.1–6.5 ms (48+32) — pool size does not matter at 10 wallets. The
10–16 ms seen at 10 wallets in the 4-level runs comes from running right after
the 1-wallet level (a checkpoint was writing in the background); that is also
what a real host does, so both are reported.

**Verdict (through the relay).** Single-wallet intent→signed now meets the target on Postgres
(p50 1.5 ms, p99 < 3 ms). The full request including the durable write meets
p50 < 5 ms; its p99 meets 15 ms in 1 of 4 runs — the tail is fsync on this
desktop's virtual disk. Beyond ~10 concurrent wallets latency is bounded by
connection queueing and serialized fsyncs: sizing the pool to concurrency cuts
it 2–3×, but 50–100 simultaneous orders still exceed 15 ms.

What changed:
1. Pre-sign writes collapsed: one CTE for create (intent + order + event), one
   plpgsql call `order_quote_and_advance` for `CREATED→QUOTED→SIGNING|AWAITING_AUTH`
   (two CAS transitions + quote + two journal rows), CTE transitions, one
   `INSERT … jsonb_array_elements` for send attempts, one daily-usage query.
2. Pre-sign writes use a second pool with `synchronous_commit=off`; `SIGNED` and
   every later write stays durable. Safe because nothing has been signed or
   sent before `SIGNED`, and PostgreSQL flushes WAL in order: the durable
   `SIGNED` commit cannot become durable unless every earlier async commit
   already is. A crash can lose only orders that never reached `SIGNED`, which
   is the same as the request never arriving (the client retries with the same
   idempotency key). Off switch: `DATABASE_ASYNC_PRESIGN_COMMITS=false`.
3. Both pools are clamped to the server's `max_connections` minus 10. Found by
   this benchmark: 2 × 64 connections > 100 made opens fail under load, 16 orders
   were rejected with "pool timed out", and 10 sent orders stayed `SIGNED`
   because the `SUBMITTED` write failed (`engine-pg-20261112T063003Z.json`:
   1990/2000 and 1973/2000 submitted). The engine now always hands a sent
   transaction to the observer (from `SIGNED` if `SUBMITTED` cannot be written)
   and releases the wallet reservation when nothing was forwarded. Since the
   fix: 52,000/52,000 orders submitted in the runs tabled above. **One other
   run** (with the local validator still running) aborted at 10 wallets: a
   connection acquire waited the full 10 s (`pool timed out while waiting for an
   open connection`) and `submit()` returned that error. Root cause not
   identified (no Postgres error logged, no checkpoint in progress); suspected
   WSL relay or virtual-disk stall. Watch `pool timed out` in production logs.
4. SSL off vs on (`sslmode=disable` / `prefer`) on localhost: no measurable difference.

Not done (would cut further): run Postgres on the same Linux host/VPC without the
WSL relay; a disk with fast fsync (GCP pd-ssd/hyperdisk, or Cloud SQL where HA
replication adds latency instead); keep daily usage in memory; group commit
tuning (`commit_delay`) for 50+ concurrent wallets. None measured.

### 2c. Same benchmark, engine and Postgres colocated on Linux (no relay)
Linux build of `trader-bench` run inside the same WSL2 VM as Postgres, TCP to
127.0.0.1 (no Windows→WSL relay). Closest available stand-in for app + DB on one
Linux VM; the disk is still this desktop's virtual disk. Probe of the raw database
(`pgbench`, 1 client): SELECT round trip 0.07 ms in-VM vs 0.23 ms through the
relay; TPC-B-like write transaction with fsync 1.31 ms avg (stddev 1.7) in-VM vs
3.05 ms (stddev 6.5) through the relay.

Default pools (16+8), 3 runs (`engine-pg-20261112T065928Z`, `…065941Z`, `…070019Z`):

| wallets | accepted→signed p50 / p99 | request→send p50 / p99 | throughput |
|---:|---:|---:|---:|
| 1 | 0.83–0.86 / 1.4–1.8 ms | **2.04–2.12 / 3.3–3.9 ms**; slow run 2.53 / 18.9 ms | 256/s (92/s) |
| 10 | 1.0–1.5 / 2.0–2.6 ms | **3.9–4.3 / 6.6–8.0 ms**; slow run 10.6 / 18.9 ms | 1201–1253/s (439/s) |
| 50 | 6.0–6.9 / 17–19 ms | 13.5–15.2 / 30–36 ms; slow run 42 / 75 ms | 1639–1857/s |
| 100 | 12.5–13.3 / 25 ms | 26.4–27.6 / 52–58 ms; slow run 106 / 206 ms | 1801–1834/s |

Pools 48+32, 2 runs (`…070032Z` slow, `…070038Z`): 50 wallets request p50 10.1 ms
(slow run 28.2), 100 wallets 21.2 ms (57.8), ~2,400 orders/s.

The "slow" runs are the same binary minutes apart; only the disk's fsync time
changed (Windows host I/O underneath the VM). Both are reported because a
production disk can do the same.

**Verdict (colocated).** With 1–10 concurrent orders the full request — idempotent
create, quote, sign, and the durable SIGNED write — meets p50 < 5 ms and p99 < 15 ms
in 2 of 3 runs. 50–100 simultaneous orders do not meet p99 < 15 ms on this disk.

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

Re-run after the 2026-11-12 engine/store changes (`localnet-pump-smoke-v2.json`):
20/20 FINALIZED, 20/20 balances verified, buys ≤1 slot 10/10, sells 7/10 (≤2 slots 10/10);
intent→signed p50 / p95 2.00 / 4.46 ms (buy), 2.08 / 4.15 ms (sell); signed→first
observed p50 107 ms (buy) / 417 ms (sell), p95 442 ms. Bimodal: ~100 ms when the tx
lands in the current slot, ~430 ms when it lands in the next one.

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
| Postgres on a GCP VM / Cloud SQL / Supabase | Only measured on WSL2 on the dev desktop; no cloud database authorized |
| Mainnet landing, sender A/B (RPC vs Jito vs Helius vs TPU) | Mainnet trading not authorized; TPU sender not implemented |
| GCP region RTT (Frankfurt vs Singapore) | No GCP deployment authorized; script ready: `deploy/gcp/scripts/region-ab.sh` |
| Telegram ingress→confirmation | Requires a real bot token + public HTTPS endpoint |
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
