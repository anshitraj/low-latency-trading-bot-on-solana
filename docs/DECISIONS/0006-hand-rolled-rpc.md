# ADR 0006 — Minimal JSON-RPC client instead of solana-rpc-client
Date: 2026-11-11 · Status: accepted

We need precise error classification (transport/timeout vs HTTP 429/5xx vs
JSON-RPC codes) to decide "possibly forwarded" vs "definitively rejected",
persistent keep-alive pools per endpoint, URL redaction, and a small dependency
graph. `crates/market-data/src/rpc.rs` implements only the methods we use and is
tested against a mock server and a real local validator.
