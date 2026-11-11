# RPC and routing

## Provider support matrix (verified 2026-11-11)

| path | status | access/cost | notes |
|---|---|---|---|
| Standard RPC `sendTransaction` | implemented/tested (mock chain, localnet) | any RPC; free public endpoints are rate-limited | `maxRetries=0` (engine rebroadcasts), preflight on, base64 |
| Jito Block Engine `POST /api/v1/transactions` | implemented, tested vs mock only | default sends need no auth key; optional `x-jito-auth` UUID; 429 on limit | always skip_preflight; `bundleOnly=true` = revert protection; min tip 1,000 lamports (bundles); tip accounts via `getTipAccounts` |
| Helius Sender `/fast` | implemented, tested vs mock only | keyless 1 req/s, 50 TPS with key; no credits consumed | requires `skipPreflight=true`, `maxRetries=0` and a tip to a designated account: ≥0.001 SOL (Sender Max) or ≥0.000005 SOL with `?swqos_only=true` |
| Direct TPU/QUIC (Yellowstone Jet 0.8.1) | **not implemented** | free to send, but **no stake-weighted QoS** without stake or peering | stub reports `NotSent`; the crate targets newer Solana crates (see ADR 0005) |
| Jito FullSend / bundles / ShredStream | not used | require approval and/or tips | |

Sources: docs/DEPENDENCIES_AND_SOURCES.md.

## Sender contract (`crates/router/src/lib.rs`)
`TransactionSender::submit(&[u8], &SendContext) -> SendOutcome` and `health()`.
Outcomes: `Accepted` (ack only!), `AlreadyProcessed`, `Rejected` (definitive,
not forwarded), `RateLimited`, `Ambiguous` (timeout/transport/5xx — may have
been forwarded), `NotSent` (breaker/limit/unimplemented). An endpoint returning
a different signature is downgraded to `Ambiguous` and counted unhealthy.

RPC error mapping (`sender-rpc::classify`): `-32002` preflight failure,
`-32003` signature verification failure, `-32602` invalid params → Rejected;
`AlreadyProcessed` → AlreadyProcessed; 429 → RateLimited; `-32005`, 5xx,
timeouts, transport → Ambiguous.

## Router policy
- Candidates = requested senders ∩ configured ∩ allowed by wallet policy ∩ breaker not open.
- Deterministic score = p50 ack latency (default 50 ms when unknown) + 100 × failure-rate penalty; ties by sender kind. Missing data → neutral score (safe fallback). Leader identity/region is **not** an input (not implemented).
- Fan-out: all chosen senders receive the identical bytes concurrently; per-sender timeout (5 s default) and token-bucket rate limits (Jito default 1 rps, Helius 1 rps — configurable, not assumed).
- Circuit breaker: 5 consecutive bad outcomes → open 10 s → single half-open trial.
- Tips: see EXECUTION_ENGINE.md; the tip is paid once, by the transaction, whichever path lands it.

## Connection handling
`reqwest` clients per endpoint with keep-alive pools (90 s idle, TCP_NODELAY, keepalive 30 s). No per-trade connection bootstrap. Blockhash refreshed every 500 ms (`confirmed`), block height every ~2 s.

## Failure handling summary
| failure | behaviour |
|---|---|
| 429 from single sender | REJECTED, no fee (nothing forwarded) |
| 5xx / timeout after possibly forwarding | SUBMITTED; observer resolves; rebroadcast same bytes |
| sender dropped tx | rebroadcast every 2 s while blockhash valid |
| blockhash expired, never seen | EXPIRED (finalized height check + history search) |
| RPC node disagreement / fork | OBSERVED → SUBMITTED after 3 missing polls; re-observed later |
| process restart | UNKNOWN_RECONCILE → observed to a terminal state; never rebroadcast/re-signed |

## Free-tier reality
Public RPC endpoints rate-limit aggressively and do not provide priority
forwarding. No paid RPC, gRPC, shred feed or staked connection is assumed or
configured. Landing competitiveness on mainnet is therefore unknown and must be
measured with the authorized micro-trade protocol (RUNBOOK.md).
