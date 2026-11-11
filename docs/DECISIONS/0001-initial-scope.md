# ADR 0001 — Initial scope: P0 first, one DEX, honest gates
Date: 2026-11-11 · Status: accepted

Ship a complete, tested P0 (manual buy/sell on Pump bonding curves, wallet,
quotes with full costs, durable state machine, Telegram + Mini App, benchmarks)
before any P1/P2. Unimplemented features are labeled in the UI, API (501) and
docs. Mainnet stays disabled by default behind an operator phrase, a database,
an allowlist and per-wallet enablement.
