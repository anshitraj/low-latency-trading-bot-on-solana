# Dependencies and sources (verified 2026-11-11)

## External APIs / docs consulted
| source | what we used | status / constraints |
|---|---|---|
| https://core.telegram.org/bots/webapps | initData HMAC: `secret=HMAC_SHA256("WebAppData", bot_token)`, `hash=hex(HMAC_SHA256(secret, data_check_string))`, sorted `k=v` joined by `\n`, check `auth_date`; Ed25519 `signature` alternative for third parties (not used) | implemented in `crates/api/src/auth.rs` |
| https://core.telegram.org/bots/api | sendMessage, editMessageText, answerCallbackQuery, setWebhook (`secret_token`), `X-Telegram-Bot-Api-Secret-Token`, `retry_after` | implemented; not exercised against real Telegram |
| https://docs.jito.wtf/lowlatencytxnsend/ | `POST /api/v1/transactions` sendTransaction (base64; base58 deprecated), `bundleOnly=true`, always skip_preflight, min tip 1000 lamports for bundles, `getTipAccounts`, `x-jito-auth` optional, 429 on limits | `crates/sender-jito` (mock-tested only) |
| https://www.helius.dev/docs/sending-transactions/sender | `…/fast` endpoints, skipPreflight=true + maxRetries=0, tip to designated accounts (10 listed), Sender Max ≥0.001 SOL, `?swqos_only=true` ≥0.000005 SOL, keyless 1 rps / 50 TPS with key | `crates/sender-rpc` Helius mode (mock-tested only) |
| https://developers.jup.ag/docs/swap | Swap API v2 at `https://api.jup.ag/swap/v2`: `/order`+`/execute` (meta-aggregator) or `/build`+`/submit` (router, raw instructions); quotes keyless; V1 (Metis) deprecated | **not implemented** |
| https://github.com/pump-fun/pump-public-docs | v3 trades (17 accounts), fees kept on curve, holder-rewards, end of cashback, negative virtual quote reserves (PumpSwap), synthetic migration, SDK versions | Pump adapter targets v3 via the official Rust client |
| https://docs.cloud.google.com/kms/docs/algorithms | `EC_SIGN_ED25519` (PureEdDSA, raw data input) | researched; not implemented |
| https://docs.cloud.google.com/billing/docs/how-to/budgets | budgets/alerts do not cap spend | Terraform budget alerts |
| https://gcloud-compute.com/europe-west3.html (third party) | europe-west3 VM hourly prices | used for **estimates** only |
| https://solana.com/docs (fees, transactions, landing) | base fee 5000/sig, prioritization fee = ceil(CU limit × CU price / 1e6), rent formula, confirmed-commitment blockhash, lastValidBlockHeight | `crates/domain/src/fees.rs`, `crates/execution/src/blockhash.rs` |

## Rust crates (pinned in Cargo.lock)
| crate | version | why |
|---|---|---|
| pump-rust-client | =0.5.0 (MIT) | official Pump/PumpSwap builders + quoting; also ships `pump.so` etc. used in the local validator |
| anchor-lang / anchor-spl | 0.31.1 | unified with pump-rust-client (lockfile pinned; see ADR 0005) |
| solana-pubkey / instruction / message / transaction / keypair / signer / hash / signature | 2.x (2.2–2.4) | component crates matching anchor 0.31 types (no heavy solana-sdk/client) |
| solana-compute-budget-interface, solana-system-interface | 2.2, 1.0 | instruction builders |
| tokio 1, axum 0.8, tower-http 0.6, reqwest 0.12 (rustls) | | runtime/HTTP |
| sqlx 0.8 (postgres, runtime queries, embedded migrations) | 0.9 exists; 0.8 chosen for a stable known API | Postgres |
| aes-gcm 0.10, hmac 0.12, sha2 0.10, subtle 2.6, zeroize 1.8 | | crypto |
| hdrhistogram 7.5 | | benchmarks |
| yellowstone-jet-tpu-client | 0.8.1 (Apache-2.0) | evaluated for TPU sender; **not integrated** (targets Solana 3.x crates) |
| quinn / rustls / rcgen | 0.11 / 0.23 / 0.13 (MIT/Apache-2.0) | our own minimal TPU QUIC client (`crates/sender-tpu`) |

## JS (apps/telegram-webapp, pnpm-lock.yaml)
next 16.4.6, react/react-dom 19.3.1, tailwindcss 4.3.5, typescript 5.9.3 (7.x exists; 5.9 chosen for tooling compatibility), openapi-typescript 7.13.0, @playwright/test 1.65.0.

## Tooling on the build host
rustc/cargo 1.92.0, Node 24.11, pnpm 10.28, Agave CLI 4.2.0 (Windows) + Linux solana-test-validator in WSL2, Docker 29.5 CLI (daemon unavailable this session), gcloud 589.0.0, cargo-audit 0.22.2.
