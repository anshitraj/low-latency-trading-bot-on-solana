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

### 2d. Same benchmarks on the GCP VM (Frankfurt)
`trader-vm`: c4-standard-4 (4 vCPU, 15 GB), europe-west3-a, Ubuntu 24.04,
hyperdisk-balanced 50 GB, Postgres 16 on the same VM. Files: `benchmarks/results/gcp-vm/`.

| | desktop (2b/2c) | **GCP VM** |
|---|---|---|
| RPC round trip to `api.mainnet-beta.solana.com` (reused connection) | ~75 ms | p50 23.4 ms (min 4.7, max 45.2; n=11) |
| Engine, in-memory store, 1 wallet: accepted→signed p50 / p99 | 0.20 / 0.25 ms | **0.08 / 0.10 ms** |
| Engine + Postgres, 1 wallet: request→send p50 / p99 | 2.0–2.1 / 3.3–3.9 ms (colocated WSL) | **1.60 / 2.68 ms** |
| Engine + Postgres, 10 wallets, pools 48+32: request→send p50 / p99 | 5.8–6.1 / 10–12 ms | **2.67 / 5.46 ms** |
| 50 / 100 wallets (48+32): request→send p50 | 18–19 / 37–39 ms | 11.7 / 22.3 ms |
| `pgbench` 1 client, write txn with fsync | 1.31 ms avg | 0.84 ms avg |

### 2e. Live mainnet Pump state, WebSocket stream vs RPC reads — `trader-bench live`
Read-only: real mainnet bonding-curve state for `6caZsnv4PgEhKUKEV1aPQHq94PQi5HyufAt1oaU7pump`
(public RPC), real ed25519 signing with a throwaway key, **mock sender (nothing
broadcast)**, cached balances; orders labelled localnet so mainnet gates stay on.
100 orders, 500 ms apart. Files: `benchmarks/results/gcp-vm/live-ws-20261112T094736Z.json`,
`live-rpc-20261112T094833Z.json`.

| on the GCP VM | order accepted→signed p50 / p95 / p99 / max |
|---|---|
| **WebSocket stream** (100/100 quotes from pushed state, 0 reconnects, warm-up 104 ms) | **0.204 / 0.229 / 0.235 / 0.239 ms** |
| RPC reads (state refetched when older than one slot) | 24.7 / 60.1 / 88.4 / 111.9 ms |
| wallet balance read (public address) | stream 0.007 ms vs RPC 20.3 ms p50 |

Mainnet **dry run** through `mainnet-canary` with streaming (desktop, owner-funded
wallet, `mainnet-canary-dryrun-20261112T094741Z.json`): order→signed 0.22–0.45 ms over
5 simulated buys (was 173–178 ms without streaming); every simulation err=null, ~69,800 CU.
Localnet send loop with streaming (`localnet-canary-ws-send-20261112.json`): 6/6
FINALIZED, balances verified, all 1 slot, sell size taken from streamed balances;
order→signed 0.22–0.67 ms.

Not measured yet: send round trip to a sender in Frankfurt (Helius/Jito), mainnet
landing, Telegram tap → server.

### 2g. MAINNET: first real trades (owner-run, 2026-11-12)
`trader-cli mainnet-canary --senders tpu,jito --iterations 3 --send` on the GCP VM in
Frankfurt, owner's disposable wallet, Helius RPC + WebSocket for reads, Jito Frankfurt
(tip 1,000 lamports) and direct TPU QUIC for sending, coin
`6caZsnv4PgEhKUKEV1aPQHq94PQi5HyufAt1oaU7pump` (bonding curve). Report:
`benchmarks/results/mainnet-canary-send-20261112T105326Z.json`. **n = 6; a small sample.**

Independently verified afterwards with the public RPC (`getSignatureStatuses`,
`getTransaction`): 6/6 `finalized`, `err = null`, slots match the report, payer = canary wallet.

| trade | landed slot − slot read before send | order→signed | signed→handed to senders | signed→bot saw it final |
|---|---:|---:|---:|---:|
| buy 1 | 2 | 0.233 ms | 6.8 ms | 780 ms |
| sell 1 | 1 | 0.209 ms | 2.1 ms | 692 ms |
| buy 2 | 2 | 0.208 ms | 2.1 ms | 716 ms |
| sell 2 | 2 | 0.215 ms | 2.3 ms | 643 ms |
| buy 3 | 2 | 0.210 ms | 2.3 ms | 1,458 ms |
| sell 3 | 2 | 0.201 ms | 2.2 ms | 1,091 ms |

- "Slot read before send" is the RPC's `processed` slot ~15 ms before submit; landing 1–2
  slots later is ≈ 210–430 ms at the measured ~210 ms/slot.
- "Bot saw it final" includes notification/poll delay: in this run the first observation
  already returned `finalized` (Alpenglow finality, below), so this is an upper bound on
  landing, not landing itself. The observer now stamps the WebSocket notification time
  (not yet re-measured on mainnet).
- Cost: 13,000,000 → 10,077,061 lamports: 2,860,040 rent now held by the new token account
  (1,513,840) and Pump `user_volume_accumulator` (1,346,200); 62,899 lamports consumed
  (6 × 8,000 fee, 6 × 1,000 Jito tip, Pump fees). Position closed (0 tokens left).

**Run 2** (same setup, after the WebSocket-notification timestamp fix;
`mainnet-canary-send-20261112T1107Z-run2.json`, transcribed from the terminal because the
run did not write its report, see below). 6/6 landed and finalized on-chain (`err = null`):

| trade | slots after pre-send read | order→signed | signed→senders | **signed→landed (WS processed)** | signed→bot saw final |
|---|---:|---:|---:|---:|---:|
| buy 1 | 1 | 0.234 ms | 6.7 ms | **238 ms** | 792 ms |
| sell 1 | 2 | 0.194 ms | 2.1 ms | **220 ms** | 562 ms |
| buy 2 | 1 | 0.215 ms | 2.0 ms | **185 ms** | 670 ms |
| sell 2 | 1 | 0.204 ms | 2.2 ms | **184 ms** | 659 ms |
| buy 3 | 2 | 0.198 ms | 1.9 ms | **298 ms** | 541 ms |
| sell 3 | — | — | — | landed slot 468502564, finalized | (stalled, bug) |

Server-side (order received → bytes at Jito + leaders) ≈ 2.1–2.4 ms (6.9 ms first).
Landing ≈ one ~210 ms slot after sending (184–298 ms incl. notification delay).
Bug found: Helius (free plan) answered 429 to `getTransaction`; the observer dropped the
already-FINALIZED sell before its fill was decoded and the canary waited until its 120 s
timeout, exiting without a report. Fixed: fills are retried with backoff (up to 10 min),
the canary writes its report on timeouts, and observer RPC load is cut (block height at
most every 2 s; 1 s fallback poll, 150 ms polls only for 1.5 s after a landing
notification). Regression test: `fill_is_retried_when_get_transaction_is_rate_limited`.

**Mainnet facts measured the same day (they replace older assumptions in this doc):**
- **Alpenglow is live** (activated epoch 1064, 2026-10-21, per solana.com/upgrades/alpenglow;
  target ~150 ms finality, previously ~12.8 s). Public RPC: `finalized` slot within 0–4 slots
  of `processed`.
- **Slot time ≈ 210 ms** (97 slots in 20.3 s; `getRecentPerformanceSamples` 212–215 ms), not 400 ms.
- **Rent is 5,080 lamports/byte** (`getMinimumBalanceForRentExemption`: 0 B → 650,240;
  165 B → 1,488,440; 170 B → 1,513,840), not 6,960. The code now reads it at startup.
- Local validator numbers in this file (sections 3, 2f) were measured with the older rules
  (400 ms slots, ~13 s finality) and are not mainnet predictions.

### 2h. Benchmark v3 — landing vs notification (tooling ready, NOT YET RUN)
Question: did trades land well before the ~220 ms WebSocket notification, i.e. is the
gap observation delay (fixable without touching the send path) or inclusion delay
(fees, admission, leader routing)?

Same VM, wallet, size limits. Run on the VM after filling `~/.trader/rpc_urls`
(RPC_AND_ROUTING.md "Multi-provider pool"):

```
trader-cli mainnet-canary --mint <bonding-curve coin> --senders tpu,jito --tpu-leaders 3 --iterations 25 --send
```
Optional: `--cu-price auto` (p75 of `getRecentPrioritizationFees`, capped),
`--tip-lamports auto` (Jito tip-floor p50, capped), `--jito-regions frankfurt,amsterdam`
(same signed bytes to each region; the tip is paid once), `--no-slot-updates`.
25 round trips = 50 trades ≈ US$0.10 in fees + tips (rent is recovered on the sell).

What the report records per trade (and the `summary` as n/p50/p95/p99/min/max):
- A–C: per provider, `signature_processed_ms_after_signed` (each provider's own
  `signatureSubscribe`) and when that provider first announced the landed slot
  (`slotSubscribe`, plus `slotsUpdatesSubscribe` lifecycle events
  — firstShredReceived / completed / frozen … — where the provider supports it).
- D: per-sender ack latency and outcome (TPU leaders vs each Jito region), kept apart
  from inclusion evidence.
- E: landed slot − slot at submit (slot now taken from the live stream, not a separate
  RPC read), fee, CU price and tip actually paid.
Not measurable with this setup: the exact execution time inside the slot; a
transaction-filtered Geyser stream (no free provider available to us).

### 2i. Provider probe from the owner's DESKTOP (India), read-only — `trader-cli rpc-probe --public`
`rpc-probe-20261112T125944Z.json`. Desktop, not the VM: absolute round trips are not
VM numbers; the WebSocket race (who announces a slot first) is the interesting part.

| provider | getSlot p50 (p95) | WebSocket slot lag behind first, p50 (p95) | slotsUpdates |
|---|---:|---:|---|
| Helius (free) | 270 ms (280) | **183 ms (323)**, first on 0/141 slots | yes |
| Alchemy (free) | **78 ms (91)** | — no `slotSubscribe`/`signatureSubscribe` on its Solana WS | no |
| public api.mainnet-beta | 213 ms (224) | **0 ms**, first on 141/141 slots | yes |
| Ankr | 403 "API key is not allowed to access blockchain" (Solana not enabled for the key) | | |
| dRPC | 400 "chain is not available on free plan" | | |
| Chainstack | not probed: the env value is a platform API key, not a node URL | | |

From the desktop, Helius's WebSocket announced every slot ~180 ms after the public
endpoint. **The VM run below shows this was the desktop's network path, not Helius.**

**Same probe on the VM (Frankfurt)** — `rpc-probe-vm-20261112T130510Z.json`, 30 calls,
60 s race, 282 slots compared:

| provider | getSlot p50 (p95), first call | WebSocket lag behind first, p50 (p95, max) | first to announce |
|---|---:|---:|---:|
| Helius (free) | 16.8 ms (24.6), 28 ms | **0 ms (101, 201)** | **202 / 282** |
| Alchemy (free) | **6.9 ms (23.4)**, 48 ms | — (no slot/signature WS) | — |
| public api.mainnet-beta | 18.1 ms (69.0), 1,942 ms | 64 ms (473, 733) | 80 / 282 |

Reading (VM):
- Helius is the best landing observer we have: first on 72% of slots; on the rest it
  trails the public endpoint by at most ~100 ms (p95) / 201 ms (max). So the ~185–300 ms
  signed→landed in run 2 is mostly **inclusion** (≈ one ~210 ms slot after send), with
  up to ~100 ms of observation delay on some trades, not the reverse.
- A second WebSocket observer (public endpoint) would catch the 28% of slots where it
  beats Helius; expected gain is tens of ms on those trades, not a step change.
- Alchemy is the fastest HTTP read from the VM (6.9 vs 16.8 ms p50). Warm orders read
  from the stream (~0.2 ms), so this only helps cold reads / fallbacks.
- This compares providers with each other; it cannot show how long after the leader
  produced the slot the first notification arrived (needs shred/Geyser data).

### 2j. Other platforms on-chain (read-only) — `scripts/bot_landing_study.py`
`bot-landing-study-20261112T134009Z.json`. Each platform's latest transactions found
through its public fee wallets (DefiLlama `dimension-adapters` fees/gmgnai.ts, bullx.ts,
axiom.ts, photon.ts); 120 sampled per platform, plus our 10 decodable canary trades.
A 25-per-platform spot check confirmed the samples are real swaps (Pump, PumpAMM,
Raydium CPMM, Meteora).

| | GMGN | BullX | Axiom | Photon | **ours** |
|---|---:|---:|---:|---:|---:|
| on-chain failed (of listed sigs) | 1.9% (34/1,799) | 4.5% (9/200) | 6.1% (234/3,812) | 2.0% (4/200) | 0/12 |
| activity (listed sigs span) | 1,799 in ~25 min | 200 in ~15 h | 3,812 (many wallets) | 200 in ~21 min | — |
| **priority fee p50 (p90), lamports** | **100,000** (148,049) | **1,088,800** (5,444,000) | 16,407 (80,006) | **100,000** (1,000,000) | **3,000** (3,001) |
| Jito tip seen (share of txs) | 0.8% | 0% | 1.7% | 6% (p50 1,000,000) | 100% (1,000) |
| tx versions | legacy 77%, v0 18%, v1 6% | v0 100% | **v1 98%** | v0 85%, v1 15% | v0 100% |
| blockhash age p50 (p90), slots | 51 (67) | 4 (10) | 138 (140) | **2** (6) | 9 (12) |

What this can and cannot say:
- **It cannot give their click→landed time.** The chain has no record of when the
  user clicked. Blockhash age (landing slot − slot of `recentBlockhash`) is only an upper
  bound on build→land, and for GMGN and Axiom it is clearly a **policy**, not speed:
  Axiom lands every trade with a 135–140-slot-old hash (≈ 10 slots before expiry, so a
  stuck trade dies within ~2 s), GMGN with a 44–54-slot-old one. Photon (2) and BullX
  (3–4) sign with fresh hashes, so their build→land is at most ~0.4–0.8 s. Ours (9)
  reflects the canary's 2 s blockhash refresh, not landing (we measured 1–2 slots from
  send to landing, 2g).
- **Priority fee is the clear gap.** Our 3,000 lamports is ~33× below GMGN's and
  Photon's typical 100,000 and ~360× below BullX's. In a contested block the leader
  orders by fee per CU, so this matters more than any millisecond we can still save.
  Test `--cu-price auto` and higher fixed values in Benchmark v3 (each 100,000 lamports
  ≈ US$0.008 at ~US$83/SOL).
- Few of their trades tip Jito directly; they may pay other landing services (Nozomi,
  0slot, bloXroute, Helius Sender …) whose tip accounts this script does not detect.
- "Failed" here means landed and reverted (slippage etc.); dropped transactions never
  reach the chain, so the true landing rate is not visible from the chain.
- BullX: 200 trades in ~15 h — almost inactive (consistent with reports it shut down
  in 2026; not verified).
- Our sample is 10 trades; theirs are a ~20-minute window each. Rerun before relying on it.

### 2l. MAINNET run 3 — three routes A/B, new fees (owner-run, 2026-11-13)
`mainnet-canary --routes "tpu,jito;tpu,helius;tpu,jito@frankfurt+amsterdam+ny+tokyo" --iterations 18 --send`
on the VM; priority 100,000 lamports/tx at a 100k-CU limit; report
`mainnet-canary-send-20261113T092346Z.json`. **36/36 FINALIZED**, balances verified per
trade, and independently afterwards (Alchemy `getSignatureStatuses`): 36/36 finalized,
`err = null`, all slots match. Spent 3,917,384 lamports (~US$0.33; 36 × 105,000 fees, tips
24 × 1,000 + 12 × 5,000, Pump fees); position closed.

| stage (n = 36) | p50 | p95 | min / max |
|---|---:|---:|---:|
| order → signed | 0.203 ms | 0.239 ms | 0.185 / 0.241 |
| signed → first sender accepted | **0.084 ms** | 1.32 ms | 0.04 / 6.97 |
| **signed → landed (first processed notification)** | **180.6 ms** | 869 ms | **68.2** / 1,087 |
| signed → bot saw it final | 523 ms | 1,434 ms | 395 / 1,594 |
| slots after the slot seen at submit | 1 | 4 | 0 / 4 |

By route (12 trades each) — **no significant difference at this sample size**:

| route | landed p50 | p95 | min |
|---|---:|---:|---:|
| tpu + Jito Frankfurt | 207 ms | 302 ms | 117 |
| tpu + Helius Sender Frankfurt | 163 ms | 1,087 ms | 120 |
| tpu + Jito FRA/AMS/NY/TYO | 181 ms | 388 ms | 68 |

Helius's two slow trades (869, 1,087 ms; 4 slots) both had a far leader (176 ms RTT), and the
Tokyo engine once refused (rate limit after the first call). By leader distance — **this is the
factor that matters**:

| landing-slot leader (QUIC RTT from VM) | n | landed p50 | p95 | max |
|---|---:|---:|---:|---:|
| near < 20 ms | 20 | 163 ms | 277 ms | 328 |
| mid 20–60 ms | 7 | 155 ms | 388 ms | 388 |
| far ≥ 60 ms | 9 | **302 ms** | **1,087 ms** | 1,087 |

Near + mid leaders (27 trades): p50 163 ms, p90 277 ms.

Where the time goes (from Helius `slotsUpdatesSubscribe` events of each landing slot):
- In ~60% of trades Helius had already received the landing slot's first shred **before we
  signed**: the trade went into a block that was already being produced.
- The signature notification arrives **before** that slot is frozen (p50 115 ms earlier;
  7–255 ms), i.e. Helius reports as it replays the block, not at slot end.
- So the ~180 ms is: time until the leader packs our transaction + shred propagation to the
  RPC + the RPC's replay of the block up to our position (our median block position: 50%).
  A per-trade split is not reliable from this data (transactions are not evenly spaced in
  time within a block), so we cannot yet say how many ms is inclusion vs observation.
- Second observer: the public endpoint delivered only 5/36 signature notifications, all later
  than Helius; Alchemy has none. Helius was first on every trade.

Compared with run 2 (old 3,000-lamport fee, n = 5, 184–298 ms): median about the same, minimum
68 ms, 4 trades ≤ 120 ms. The network was not congested, so the higher fee is not visible here.

### 2k. Phase 2: multi-route A/B tooling, leader distance (2026-11-13)
**Tooling.** `mainnet-canary --routes "tpu,jito;tpu,helius;tpu,jito@frankfurt+amsterdam+ny+tokyo"`
runs one engine per route (shared store, wallet lock, blockhash, observer); round trip
*i* uses route *i mod n*, so routes interleave in time. After a send run each landed trade
is annotated read-only with the landing slot's leader, its TPU address, our QUIC handshake
RTT to it (`leader_region` near <20 ms / mid / far ≥60 ms) and the transaction's index in
its block. The summary adds `by_route`, `by_leader_region`, `same_slot_share` and
`block_position_pct`.

**Bug found and fixed while testing.** The engine waited for *every* sender before handing
the signature to the observer, so one slow route delayed detection of a trade another route
had already delivered. Localnet (`localnet-canary-routes-20261113.json`, TPU unreachable
from Windows→WSL over UDP, gives up after 1.5 s): `rpc,tpu` trades landed in the send slot
but were "seen" 1.70–1.80 s after signing. Fix: `Router::fan_out_first` returns at the first
attempt that may have forwarded the transaction; slower attempts finish in the background
and are recorded later; "nothing forwarded" still requires every attempt. After
(`…-20261113-b.json`): same route acks in 1.2 ms and lands 126–425 ms after signing (local
400 ms slots), 8/8 FINALIZED, balances verified. On mainnet this matters whenever a leader
connection is cold.

**Mainnet dry run (VM)**: all three routes simulate cleanly (53,355 CU; tips 1,000 Jito /
5,000 Helius); order→signed 0.23–0.26 ms.

**Leader distance from the Frankfurt VM** (`send-probe --leaders 90`, next 400 slots,
49 distinct leaders, read-only QUIC handshakes): near <20 ms **30 (61%)**, mid 20–60 ms 11
(22%), far ≥60 ms **8 (16%)** (86–459 ms). Jito: Frankfurt 2.4 ms, Amsterdam 7.5, NY 85,
Tokyo 231. Helius Sender: Frankfurt 1.3 ms, Amsterdam 7.4.

What this means for phases 3–5 (regional servers): orders are signed in Frankfurt, so a
relay in New York cannot beat the direct path to a US leader (Frankfurt→NY→leader ≈
Frankfurt→leader, ~40–45 ms one way either way). A regional server only helps if the
*order itself* starts there (a user or signal near that region) — i.e. phase 4, which
means signing keys on that server. For ~16% of leader windows the far leader adds roughly
one-way RTT (~45–230 ms) before our transaction can be included; the fan-out to the next
leaders and Jito's regional engines already cover the following windows.

### 2f. Send hop and direct TPU
Read-only probe from the GCP VM (`trader-cli send-probe`; no transaction sent):

| path | round trip p50 (min–max) |
|---|---|
| Helius Sender Frankfurt (HTTP) | **1.43 ms** (1.31–1.65) |
| Jito block engine Frankfurt (`getTipAccounts`) | **2.27 ms** (1.96–2.38) |
| Jito Amsterdam | 7.75 ms |
| QUIC handshake to the next 12 mainnet leaders' TPU | p50 29.6 ms; **2.2 ms** for leaders in Europe, up to 232 ms for US/Asia |
| public RPC `getSlot` | 28 ms (14.9–43.6) |

Local validator with the real `pump.so`, sending **only** over direct TPU QUIC
(`localnet-canary-tpu-send-20261112.json`) and RPC+TPU (`localnet-canary-rpc-tpu-send-20261112.json`),
WebSocket streaming + signature notifications on: 12/12 trades FINALIZED, balances
verified, all 1 slot. Signed → handed to sender 0.0–1.7 ms. Signed → seen processed
24–409 ms (where in the local validator's 400 ms slot it arrived; best cases 24, 35, 39 ms).

Mainnet dry runs with tipped senders (`mainnet-canary-dryrun-20261112T102819Z.json`, `…102827Z.json`):
`--senders tpu,jito` (tip 1,000) and `--senders tpu,helius` (tip 5,000) simulate cleanly
(69,973 CU incl. the tip transfer); order→signed 0.43–0.92 ms.

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
| Mainnet landing per sender (A/B: RPC vs Jito vs Helius vs TPU) | One combined run only (tpu+jito, n=6, section 2g); no per-sender attribution of which path landed first |
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
