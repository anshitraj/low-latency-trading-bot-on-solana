# ADR 0004 — Conditional orders (TP/SL/limit) are off-chain, best effort, deferred
Date: 2026-11-11 · Status: accepted (not implemented)

Any TP/SL/limit/trailing logic would run as an off-chain watcher using sourced
prices with timestamps, hysteresis and finite state machines, executing through
the same engine (same caps and idempotency). Stops are never guaranteed. Deferred
until P0 is proven on devnet/mainnet micro-trades. Table `conditional_orders`
exists; API returns 501.
