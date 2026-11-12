# RPC and routing

## Provider support matrix (verified 2026-11-11)

| path | status | access/cost | notes |
|---|---|---|---|
| Standard RPC `sendTransaction` | implemented/tested (mock chain, localnet) | any RPC; free public endpoints are rate-limited | `maxRetries=0` (engine rebroadcasts), preflight on, base64 |
| Jito Block Engine `POST /api/v1/transactions` | implemented, tested vs mock only | default sends need no auth key; optional `x-jito-auth` UUID; 429 on limit | always skip_preflight; `bundleOnly=true` = revert protection; min tip 1,000 lamports (bundles); tip accounts via `getTipAccounts` |
| Helius Sender `/fast` | implemented, tested vs mock only | keyless 1 req/s, 50 TPS with key; no credits consumed | requires `skipPreflight=true`, `maxRetries=0` and a tip to a designated account: ≥0.001 SOL (Sender Max) or ≥0.000005 SOL with `?swqos_only=true` |
| Direct TPU/QUIC (`crates/sender-tpu`, own client on quinn) | implemented; localnet-tested | free to send, but **no stake-weighted QoS** without stake or peering | leader schedule + gossip `tpuQuic`; pre-warmed QUIC connections; same bytes to the leaders of the next 2 slots |
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

## Fastest send path (measured from the GCP VM in Frankfurt, read-only)
Sign once, send the same bytes in parallel to a staked path near the VM and straight to
the leaders. Round trips (`benchmarks/results/gcp-vm/send-probe-20261112.json`):

| path | p50 | notes |
|---|---|---|
| Helius Sender `http://fra-sender.helius-rpc.com` | 1.43 ms | regional endpoints are plain HTTP; SWQoS-only tip ≥ 5,000 lamports |
| Jito block engine Frankfurt | 2.27 ms | tip ≥ 1,000 lamports; default 1 req/s per IP per region |
| TPU QUIC handshake, leader in Europe | ~2.2 ms | far leaders 85–232 ms (US/Asia) |
| public RPC `api.mainnet-beta.solana.com` | 28 ms | not for sending at speed |

Jito and Helius use different tip accounts, so a transaction tips one of them; the router
refuses incompatible combinations. Landing still waits for the leader's slot (mainnet
≈ 210 ms/slot, measured 2026-11-12) and finality since Alpenglow targets ~150 ms; nobody can
guarantee landing in a specific slot. First mainnet run: landed 1–2 slots after the pre-send
slot read (BENCHMARKS.md 2g).

Landing detection: each tracked signature gets a one-shot `signatureSubscribe` (processed);
the notification wakes the observer immediately (polling stays as fallback).

## WebSocket account stream (`crates/market-data/src/ws.rs`)
`accountSubscribe` (base64, `processed`) for each coin's Global, FeeConfig, bonding
curve, mint and curve token accounts, plus each wallet's SOL account, token accounts
and Pump `user_volume_accumulator`; `slotSubscribe` as heartbeat.
- Served only while live: connected **and** a slot notification within 1.5 s.
- An account is served only after its subscription is acknowledged on the current
  connection **and** seeded by an RPC read issued after that ack (no gap where an
  update could be missed); newer slots always win.
- On disconnect every entry is dropped; reconnect (250 ms → 10 s backoff) resubscribes
  and re-seeds. Until then reads fall back to RPC (fail closed to the slow path, never to stale data).
- Warm-up: first quote for a coin (Mini App "Load" → `/v1/tokens/{mint}/quote`) starts
  it in the background; `mainnet-canary` and `trader-bench live` warm explicitly.
- Env: `WS_URL` (default: `RPC_URL` with http→ws), `WS_STREAMING=false` to disable.

## Multi-provider pool (`crates/market-data/src/pool.rs`)
`RPC_URL` (or `--rpc`) is provider 0; extra providers come from `~/.trader/rpc_urls`
(override with `RPC_URLS_FILE`), one per line:

```
# <http-url> [<ws-url>] [rps=<n>]      — '#' comments ok; keep this file mode 600
https://solana-mainnet.g.alchemy.com/v2/KEY
https://rpc.ankr.com/solana/KEY  wss://rpc.ankr.com/solana/ws/KEY
https://lb.drpc.org/ogrpc?network=solana&dkey=KEY  wss://lb.drpc.org/ogws?network=solana&dkey=KEY
```
- The ws URL is derived (http→ws, same path) when omitted; Ankr and dRPC use a
  different WebSocket path, so give it explicitly.
- Every provider's genesis hash is checked at startup: a wrong cluster is fatal, an
  unreachable provider is skipped.
- Per-provider token bucket (default 8 req/s, `rps=` overrides); background calls
  cannot take the last 2 tokens, so polling never starves a trade read. 429 → that
  provider cools down (250 ms doubling to 10 s) and reads move to the next one.
  `sendTransaction` never fails over automatically (the sender layer owns it).
- One WebSocket per provider: provider 0 feeds account state and balances; **all**
  subscribe to each trade's signature and slots, so the observer stamps the earliest
  processed notification and the canary reports each provider separately.
- Logs and reports print only the host, never the path or query (where keys live).

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
