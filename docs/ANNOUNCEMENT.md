# Announcement drafts (X + LinkedIn)

Every number here is measured on Solana mainnet and documented in [SPEED.md](SPEED.md) and
[BENCHMARKS.md](BENCHMARKS.md). Do not add claims that are not in those files (for example
"faster than Axiom/GMGN" — their end-to-end times are not observable).

## X (thread)

**1/**
I built a Solana trading bot from scratch in Rust and measured every millisecond on mainnet.

From sending a trade to seeing it inside the block: ~180 ms → ~50 ms median. Best: 13 ms.
162 real trades, all verified on-chain, total cost ~$1.30 in fees. 🧵

**2/**
Inside our own engine:
• quote → build → check → sign: 0.2 ms
• signed bytes on the wire to the block leaders: ~2 ms
Everything after that is Solana itself: the leader has to pack your trade, and the block
data has to reach you.

**3/**
What actually moved the needle:
• Send the same signed tx over several routes at once (direct TPU/QUIC to the leaders +
  Helius staked)
• Run our own Agave RPC node (64 cores, 512 GB) in Frankfurt
• Read our trade straight out of the raw block data ("shreds") instead of waiting for RPC

**4/**
Biggest surprise: a standard RPC "your tx landed" notification arrives ~100 ms after the trade is already in the block.
Our shred decoder on our own node beat Helius by ~60 ms.
A pro shred feed (GetBlock trial) beat our own node on 99.98% of 393,165 Pump.fun txs —
because unstaked nodes get block data last.

**5/**
What did NOT help:
• Paying 200× more tip for a "max" sender route
• Jito-only at small tips (~3 slots late)
• More servers in more regions (orders are signed in Frankfurt; physics wins)

**6/**
Result with a pro shred feed: 54 ms median, 25% of trades seen in the block under 40 ms.
Not every trade can be under 40 ms on ~200 ms slots — it depends on which validator leads
the slot. Full write-up + raw data in the repo.

## LinkedIn

**How fast can a Solana trade really be? I measured it.**

Over the last days I built a Solana trading bot (Telegram bot + Mini App) from scratch, with
a Rust execution engine, and benchmarked it on mainnet with 162 small real trades — every one
verified on-chain, about $1.30 in total fees.

What we measured:
- Our engine signs a trade in 0.2 ms and has it on the wire to the block leaders in ~2 ms.
- From sending to seeing the trade inside the block went from ~180 ms (standard RPC
  notification) to ~50 ms median, with the best trade at 13 ms and 25% under 40 ms.
  (Telegram's own delay from a user's click to our server is not included.)

What made the difference:
1. One signed transaction, sent over several routes in parallel (direct QUIC to the current
   and next leaders, plus a staked route), never two different orders.
2. Our own Solana RPC node (Agave, built from source, 64 cores / 512 GB) in Frankfurt.
3. A small Rust "shred observer" that copies the raw block data the node receives, rebuilds
   missing pieces with Solana's own erasure recovery, and spots our transaction before any
   RPC reports it.
4. A professional shred feed for the last ~80 ms: unstaked nodes sit at the end of Solana's
   relay tree, so a feed that receives data near the top is simply earlier.

What didn't help: paying much higher tips, Jito-only routing at small tips, and more servers
in more regions.

The honest limit: Solana slots are ~200 ms. Whether a trade can be seen in under 40 ms depends
on which validator leads the slot and where it is — so it happens on some trades, not all.

All numbers, methods and raw results are documented in the repository.
