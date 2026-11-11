# Security: threat model, custody and key management

**Status: private beta, unaudited, custodial test wallets. Do not accept meaningful deposits.**

## Custody model (as implemented)
- Each user gets a *managed* (custodial) Solana keypair per network, created server-side.
- Seed (32 bytes) → AES-256-GCM under a random per-wallet DEK; DEK → AES-256-GCM under the KEK; both bound to `wallet:<id>` as AAD (`crates/wallets/src/keystore.rs`). Stored in `wallet_key_metadata`; plaintext never written to DB, logs, metrics, Telegram or the browser.
- KEK: `KEK_BASE64` from Secret Manager (GCP) or env (local). **Not** KMS-wrapped yet.
- To sign, the seed is decrypted into process memory and the signer is cached (`crates/api/src/providers.rs`). Zeroization covers temporary buffers, not the cached keypair. **This is not hardware isolation.** A root compromise of the VM can extract keys.
- Users cannot export keys in this build (`/export` explains it is disabled). A key that lives only in non-exportable KMS could never be user-exportable; the current design keeps that option open but it is disabled until reviewed.
- External-wallet (user-signs) mode is not implemented.

### KMS options (researched, not implemented)
GCP Cloud KMS supports `EC_SIGN_ED25519` (PureEdDSA, raw data input — https://docs.cloud.google.com/kms/docs/algorithms, accessed 2026-11-11), so Solana message bytes could be signed without the key ever leaving KMS.
| option | pros | cons |
|---|---|---|
| A. Envelope seed + env/Secret Manager KEK (current) | fast (in-process sign ≈ 37 µs incl. compile/guard), simple | key in memory; KEK compromise = all wallets |
| B. Envelope seed + KMS-wrapped KEK/DEKs | KEK never in plaintext at rest; IAM-audited unwrap | key still in memory after unwrap; +1 KMS call per cold wallet |
| C. Per-user KMS Ed25519 key, `asymmetricSign` per trade | key never leaves KMS; strongest | network RTT per trade (NOT MEASURED), per-key cost, non-exportable, rate quotas |
Recommendation: B before any mainnet beta; evaluate C for automation wallets after measuring latency.

## Hard protections (enforced server-side)
- Per-wallet policy: max order (slippage-inclusive), daily turnover, trades/day, SOL reserve, max slippage, max priority fee, max tip, max price impact, allowed senders, `paused` (user kill switch), `locked` (operator), `mainnet_enabled` (default false). Mainnet also needs `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`, Postgres and a tester allowlist.
- Pre-sign guard on the compiled message: payer = wallet, exactly 1 signer, no lookup tables, allowlisted programs only (ComputeBudget, System, ATA, Pump), System transfers only to the planned tip account and ≤ cap, ATA creation only for the wallet, priority fee ≤ cap, Pump discriminators + user position.
- Network binding: signer refuses networks outside its scope; RPC genesis hash checked at startup; every DB row carries its network.
- No withdrawals, no transfers to arbitrary addresses, no key export.

## Threat model

| threat | mitigation (implemented) | residual risk / not implemented |
|---|---|---|
| Stolen Telegram account | allowlist, per-wallet caps, kill switch, confirmation step, no withdrawals/export | attacker can trade within caps; no step-up auth |
| Replayed / forged inline callbacks | server-side random tokens bound to user+chat, expiry, single-use atomic consume, replays mapped to same idempotency key (tested) | — |
| Webhook spoofing | secret-token header (constant-time), update_id dedupe | secret leakage → forged updates (rotate secret) |
| Stolen/replayed initData | HMAC validation, 5-min age, future-date check, one session per initData hash, duplicate-key rejection | stolen fresh initData within window before first use |
| Session theft | 30-min bearer tokens, hash-only storage, no cookies (no CSRF), CORS restricted | XSS in Mini App would expose token (React escaping, no `dangerouslySetInnerHTML`) |
| Horizontal privilege escalation | every resource scoped by user id; foreign ids return 404 (tested) | — |
| Compromised VM / process | IAP-only SSH, OS Login, shielded VM, least-privilege SA, tmpfs secrets | full key exposure (custodial) |
| Log exfiltration | secrets never logged (Debug impls redact), URL redaction, Caddy strips query/auth headers, 7-day retention | operator error |
| Malicious/misleading mint | mint identity only (no tickers), owner checks, risk screen (authorities, Token-2022 extensions, concentration), unsupported → refused | rug pulls, dumps, honeypots not detectable |
| Spoofed pool / malicious program | PDA derivation from mint, owner + discriminator checks, program allowlist in guard | RPC returning forged data (single RPC trust) |
| Quote manipulation / sandwich / MEV | on-chain min-out/max-in, slippage caps, impact cap, Jito path optional | sandwiching within slippage tolerance |
| Oversized slippage | policy max slippage (15% default) | user can still choose high values ≤ cap |
| Feed lag / stale state | fail closed on state >2 s, blockhash >30 s | — |
| RPC deception | genesis check, signature equality check, independent observer | colluding RPC can hide/withhold; multi-RPC cross-check not implemented |
| Concurrent orders / races | per-wallet lock, reservations, CAS transitions (tested with 20–50 concurrent requests) | — |
| Duplicate transaction | one signature per order, same-bytes rebroadcast, no replacement txs (tested under fault injection) | — |
| Lost blockhash | background refresh, staleness gate, expiry detection via finalized height | — |
| Fake deposit notification | balances read from chain, never from user/notification input | — |
| Supply-chain compromise | pinned versions, `Cargo.lock`, `pnpm-lock.yaml`, cargo-audit, gitleaks in CI | no SBOM/sigstore verification; Pump `.so` artifacts used only in local validator |
| Rate-limit abuse | per-user order limit (30/min), per-Telegram-user message limit (40/min), body size 64 KiB, bounded queues | no per-IP limiter (Caddy/Cloud Armor not configured) |
| Withdrawal fraud | withdrawals not implemented | — |
| Key theft from DB backup | backups encrypted with offline passphrase; seeds sealed under KEK | KEK + backup together = all keys |

## Incident handling
See RUNBOOK.md: pause all trading (set every wallet `locked`, or stop the service), rotate Telegram token/webhook secret, rotate admin token, revoke sessions (`DELETE FROM telegram_sessions`), sweep managed wallets to a cold address (manual, operator-signed — tooling not implemented), notify testers.

## Before any public launch
External audit; KMS-backed key protection (option B/C); step-up auth for automation; multi-RPC cross-checks; per-IP rate limiting/WAF; withdrawal flow with address book, cooldowns and limits; legal review.
