# Known limitations (read before funding anything)

Status labels: **implemented/tested**, **implemented/untested**, **gated**, **not implemented**.

## Feature matrix

| feature | status | notes |
|---|---|---|
| Exact money/fee/rent math, slippage thresholds | implemented/tested | integer only; JSON amounts are strings |
| Order state machine + journal | implemented/tested | 13 states; DB trigger mirrors the table (verified on Postgres) |
| Idempotent order creation | implemented/tested | MemStore and Postgres (20 concurrent creates -> 1 row) |
| Sign-once, same-bytes fan-out, rebroadcast, block-height expiry | implemented/tested | mock chain + localnet |
| Restart reconciliation | implemented/tested (mock) | recovered orders are observed, never rebroadcast |
| Pump bonding-curve buy/sell (v3) | implemented/tested on localnet | real `pump.so`; **no devnet/mainnet fill yet** |
| PumpSwap (post-migration AMM) | not implemented | migrated coins are refused (`Unsupported`) |
| Pump non-SOL quote mints, mayhem coins, legacy cashback coins | not supported | refused (fail closed) |
| Raydium CPMM adapter | not implemented | |
| Jupiter Swap V2 fallback | not implemented | |
| RPC sender | implemented/tested | `maxRetries=0`, preflight on by default |
| Jito `sendTransaction` sender | implemented/tested vs mock block engine | **never sent to the real block engine**; off unless `JITO_BLOCK_ENGINE_URL` set |
| Helius Sender | implemented/tested vs mock | tip-required; off unless configured |
| Direct TPU/QUIC (Yellowstone Jet) | **not implemented** | stub returns `NotSent`; no staked/SWQoS access exists |
| Leader-aware routing | not implemented | router scores by health/latency only; no leader schedule input |
| Dynamic priority fees | not implemented | user-configured CU price capped by policy; `getRecentPrioritizationFees` client exists but unused |
| WebSocket/gRPC market data | not implemented | state refreshed on demand via `getMultipleAccounts`; quotes fail closed when stale (>2 s) |
| Balance cache | not implemented | one RPC read per order (≈1 round trip on the hot path) |
| Telegram bot (commands, token panel, buttons, confirmations, notifications) | implemented/tested vs mock Bot API | **never connected to real Telegram** |
| Telegram deep link `/start <mint>` | implemented/tested | |
| Mini App (trade, portfolio, orders, tracker, settings, kill switch) | implemented/tested (Playwright, mock mode) | no charts; Discover/Copy/Referrals show "not available" |
| External-wallet signing | not implemented | managed (custodial) test wallets only |
| Wallet tracking alerts | not implemented | watchlist storage only |
| Copy trading, TP/SL, limit/trailing orders | not implemented | tables exist; API returns 501 |
| Referrals | not implemented | API returns 501 |
| Withdrawals to external addresses / key export | not implemented (deliberately disabled) | |
| PostgreSQL store | implemented/tested | 3 integration tests + full Playwright suite on Postgres 18.6; Supabase URLs parse, but no real Supabase project was used |
| Postgres hot-path latency | measured, **target missed** | intent->signed p50 5.3 ms / p99 11.6 ms (1 wallet), p50 109 ms at 100 wallets (16-connection pool saturates); ~5 sequential DB round trips before signing; see BENCHMARKS.md 2b |
| Redis | not used | nothing requires it yet |
| GCP KMS signing (EC_SIGN_ED25519) | not implemented | KEK comes from Secret Manager/env; KMS key provisioned in Terraform for future wrapping |
| Terraform / GCP scripts | implemented/untested | `terraform` not installed here; no project provided; never applied |
| Daily realized-loss limit | approximate | MemStore: cumulative realized loss; Postgres: losses on positions touched today |
| `t_webhook_received`, `t_auth_ok` stamps | not implemented | engine stages from `t_intent_accepted` on |

## Custody and security risks
- Managed wallets are **custodial**: the service can sign for users. Seeds are AES-256-GCM envelope-encrypted at rest (per-wallet DEK wrapped by a KEK, wallet-id AAD), but **decrypted into process memory to sign and cached there**. A compromised VM or process can steal funds. This is not hardware isolation.
- The KEK is an environment/Secret Manager value, not a KMS-wrapped key yet. Anyone with VM root or the secret can decrypt all wallets.
- No external security audit. Do not offer to the public or accept meaningful deposits.
- Telegram account takeover = trading access (bounded by per-wallet caps, kill switch, allowlist). No 2FA step-up exists.
- The risk screen detects mint/freeze authority, Token-2022 extensions and holder concentration only. It does not detect rugs, honeypots, wash trading or creator dumping.
- `skipPreflight` is refused on mainnet; Jito always skips preflight on its side.

## Dependency advisories (`cargo audit`)
- `ed25519-dalek 1.0.1` (RUSTSEC-2022-0093) and `curve25519-dalek 3.2.0` (RUSTSEC-2024-0344) arrive via the Solana 2.x crate line, pinned because the official `pump-rust-client` 0.5.0 was compiled against anchor-lang 0.31 / Solana 2.x types. Our signing path uses matched keypairs, so the public-key oracle is not reachable. Upgrade path: move to the Solana 3.x+ line with anchor-lang 1.x (the Pump crate accepts `<2`) — tracked in ADR 0005.
- `rsa` and `libsqlite3-sys` advisories are lockfile-only (unused sqlx drivers; not compiled).
- `bincode 1.x` unmaintained (Solana wire format dependency).

## Measurement gaps
See `docs/BENCHMARKS.md` "NOT MEASURED": no mainnet landing data, no sender comparison, no GCP region data, no Postgres latency, no Telegram end-to-end latency.

## Environment notes
- Agave 4.2 `solana-test-validator` cannot create a ledger on native Windows ("Access is denied" unpacking genesis); use WSL2/Linux (`scripts/localnet-wsl.sh`).
- Docker is not required: `scripts/postgres-wsl.sh` runs Postgres in WSL2. The compose file and Dockerfile were never exercised.
- WSL idle-terminates a distro ~60 s after the last `wsl.exe` client exits; the Postgres script keeps a hidden keepalive running until `scripts/postgres-wsl.sh down`. The keepalive does not survive a reboot, sleep or `wsl --shutdown` (observed overnight: tests then time out after 10 s); rerun `scripts/postgres-wsl.sh up` (idempotent).
