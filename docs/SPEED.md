# Speed: what we measured, and how it compares

All numbers are from real **mainnet** trades on the Pump.fun bonding curve (≈US$0.01 buys,
then sell 100%), sent from our server in Frankfurt (GCP `europe-west3`), every trade
verified on-chain afterwards. Details, raw reports and caveats: [BENCHMARKS.md](BENCHMARKS.md)
(sections 2g–2r) and `benchmarks/results/`. Measured 2026-11-12/13.

## The three numbers that matter

| interval | what it means | result |
|---|---|---|
| **order → signed** | our engine: quote, build, check, sign | **0.2 ms** (p50 0.203 ms over 162 mainnet trades) |
| **order → bytes at the leader** | sent to Jito / Helius / the leaders' TPU | **~2 ms** (first sender accepted p50 0.08 ms after signing) |
| **send → our trade seen in the block** | earliest proof it is in the block (shreds) | **54 ms median, best 13 ms, 4 of 16 under 40 ms** (with a pro shred feed) |

The first two are fully under our control and already far below 40 ms. The third is
limited by Solana itself: the leader must pack the trade into its block, and the block
data must reach us.

## How we got from ~180 ms to ~50 ms

| step | send → seen (median) | what changed | measured in |
|---|---:|---|---|
| Helius WebSocket only | 180.6 ms | baseline (36 trades) | 2l |
| + competitive priority fee (100k lamports) | ~same on a quiet network | protects against being skipped on busy launches | 2j, 2l |
| own Agave RPC node, stock notification | 312 ms | node notifies only at slot end (worse) | 2m |
| own node + **shred observer** (our code) | 137 ms | read our trade straight out of the block data | 2n |
| + watch list (report first shred carrying our signature) | 99–127 ms | no wait for the whole batch | 2o, 2p |
| + **GetBlock shred stream** (pro feed, trial) | **54 ms** (buys 49 ms) | block data from the front of Solana's relay tree | 2r |

Read-only race on **393,165** Pump.fun transactions (10 min): GetBlock first on **99.98%**;
our unstaked node **83 ms** behind at the median; Helius processed **135 ms** behind.

## Sending routes tested (same observer, same fee)

| route | median send → seen | fastest | notes |
|---|---:|---:|---|
| **direct TPU (QUIC to current + next leaders)** | 93–127 ms | 35.4 ms | free; best typical time |
| Helius Sender SWQoS-only (staked) | 97–204 ms | **23.7 ms** | 5,000-lamport tip (~US$0.0004) |
| TPU + Helius SWQoS (our default) | 50–153 ms | 13.0 ms | best of both, ~US$0.0004/trade |
| Helius Sender Max | 129 ms | 61.5 ms | 0.001 SOL tip (~US$0.08) — **not faster** than free TPU |
| Jito only (1,000 tip) | 575 ms | 314 ms | ~3 slots at low tips — dropped as a fast path |

Who leads the slot matters more than the route: trades meeting a nearby leader
(<20 ms away) were seen in ~50–100 ms; far leaders (US/Asia, ~16% of slots) in 150–300 ms.

## Comparison with other trading bots (honest)

Other bots do **not publish** click → landed times, and the chain does not record when a
user clicked, so nobody can measure their end-to-end speed from the outside. What we
*could* measure, from 120 of each platform's recent on-chain trades (2026-11-12, found via
their public fee wallets, see [BENCHMARKS.md](BENCHMARKS.md) 2j):

| | **ours** | Axiom | GMGN | Photon | BullX |
|---|---|---|---|---|---|
| order → signed | **0.2 ms (measured)** | not published | not published | not published | not published |
| send → seen in block | **54 ms median, 13 ms best** (pro feed) | not measurable from chain | not measurable from chain | not measurable from chain | not measurable from chain |
| build → land, upper bound from blockhash age | (1–2 slots measured directly) | n/a (signs with ~138-slot-old hashes on purpose) | n/a (~51-slot-old hashes) | ≤ ~420 ms (2 slots) | ≤ ~840 ms (4 slots) |
| failed on-chain | 0 of 162 | 6.1% | 1.9% | 2.0% | 4.5% |
| priority fee per trade (median) | 100,000 lamports default (10,000 in tests) | 16,407 | 100,000 | 100,000 | 1,088,800 |
| send routes | TPU + Helius staked (+ RPC) | own router | not visible | not visible | not visible |

Read this carefully: our "seen in block" time and their blockhash-age bounds measure
different things. The fair statement is: **our engine signs in 0.2 ms, puts the trade on
the wire in ~2 ms, and — with a professional shred feed — sees it in the block in ~50 ms
(best 13 ms).** We cannot claim to be faster than Axiom/GMGN end to end, because their
numbers are not observable.

## Did we beat 40 ms?

- **Yes, sometimes:** 7 trades under 40 ms in total (13.0, 23.7, 24.6, 35.4, 35.8, 36.7,
  39.4 ms); with the pro feed, **25%** of trades (4 of 16).
- **Not as the median:** best median 49–54 ms. A median under 40 ms needs the trade to
  reach a nearby leader at the start of its packing window *and* staked delivery; on
  Solana's ~200 ms slots no bot can guarantee that for every trade.

## Cost of the test campaign

All 162 mainnet test trades together cost about **15M lamports ≈ US$1.30** in fees and tips
(plus ~2.9M lamports of rent parked in the token accounts, recoverable). Infrastructure ran on GCP credits: trading VM (4 vCPU) and,
for a few hours, our own Agave RPC node (64 vCPU / 512 GB, ~US$4.5/hour, now stopped).

## Would Everstake, DoubleZero or Shyft be faster than GetBlock?

Not tested. GetBlock already delivered first on 99.98% of transactions versus our own
node, so the remaining room is small: premium feeds all sit near the top of Solana's relay
tree, and vendor benchmarks between them show single-digit-ms differences. DoubleZero
runs its own fiber network (could be slightly ahead); Shyft RabbitStream is a decoded gRPC
stream (usually a little behind raw UDP shreds); Everstake is raw UDP like GetBlock
(likely similar, cheaper at ~US$199/month vs US$300). A one-hour free test of Everstake is
fully prepared (see [deploy/gcp/rpc-node/README.md](../deploy/gcp/rpc-node/README.md)).
