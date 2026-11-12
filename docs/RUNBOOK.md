# Runbook

## Start / stop
- Local mock: `make dev` (or `TRADER_MODE=mock cargo run -p trader-api`) → http://127.0.0.1:8080.
- Localnet with real Pump program: `scripts/localnet.sh` (Windows: `scripts/localnet-wsl.sh`), then `TRADER_MODE=localnet RPC_URL=http://127.0.0.1:8899 KEK_BASE64=$(openssl rand -base64 32) cargo run -p trader-api`.
- VM: `sudo systemctl start|stop|restart trader-api`; logs `journalctl -u trader-api -f`; health `curl -s 127.0.0.1:8080/v1/ready`.

## Pause trading
- One user: `/pause` (Telegram) or Mini App kill switch → wallet policy `paused=true`.
- Everyone, fast: stop the service (`systemctl stop trader-api`). Signed-and-sent transactions can still land; on restart they are reconciled.
- Everyone, keep API up: `UPDATE risk_limits SET policy = jsonb_set(policy,'{locked}','true')` and insert defaults for wallets without a row (operator SQL); new signing is refused at execution time.

## Stalled / unknown orders
1. `GET /v1/provider-health` (admin) — blockhash age, sender breakers.
2. Orders stuck in SUBMITTED/UNKNOWN_RECONCILE resolve automatically to a terminal state within ~150 blocks (+ finalization) — do **not** re-submit; check the signature on an explorer.
3. If the observer itself is down (RPC outage), restart after RPC recovers; reconciliation uses persisted signatures and `lastValidBlockHeight`.

## Ambiguous send timeout
Expected behaviour: order stays SUBMITTED (never REJECTED), same bytes rebroadcast every 2 s while valid, observer decides. Users see "submitted — not yet seen on-chain" / "pending verification — do NOT retry".

## RPC outage / provider ban
Symptoms: `/v1/ready` 503 (`rpc_reachable=false` or stale blockhash), quotes fail closed. Action: switch `RPC_URL` (Secret Manager new version) and restart. Jito/Helius breakers open automatically after 5 failures and retry after 10 s.

## Key compromise suspected
1. Stop the service. 2. Rotate `TELEGRAM_BOT_TOKEN` (BotFather) and webhook secret; rotate `ADMIN_TOKEN`; revoke sessions: `DELETE FROM telegram_sessions`. 3. Treat all managed wallets as compromised: notify testers; move remaining funds with an offline-reviewed, operator-signed sweep (tooling **not implemented** — use `solana transfer` with the decrypted key on an isolated machine). 4. Rotate the KEK (re-seal all wallets — tooling not implemented). 5. Post-incident review using `audit_events` and `order_events`.

## Backup / restore
- Before every deploy: `deploy/gcp/scripts/backup-db.sh`.
- Monthly drill: `backup-db.sh --restore backups/<file>` (restores into `trader_restore`, prints row counts).
- Disk snapshots: daily, 7 days.

## Rollback
`ROLLBACK=1 deploy/gcp/scripts/deploy.sh`. If a migration must be reverted, restore the pre-deploy backup.

## Mainnet canary from the CLI (owner runs it; ~US$1 budget)
`trader-cli mainnet-canary` drives the real engine (PumpAdapter → RpcSender → observer)
from a disposable keypair file. No Telegram, database or KEK needed.

| step | command (PowerShell) | moves funds? |
|---|---|---|
| build | `cargo build --release -p trader-cli` | no |
| wallet | `.\target\release\trader-cli.exe keygen` → prints pubkey; file at `%USERPROFILE%\.trader\canary-keypair.json` | no |
| fund | send ~0.012 SOL (~US$1) to that pubkey from your own wallet/exchange | owner |
| RPC | `$env:RPC_URL = "https://api.mainnet-beta.solana.com"` (or a free Helius/QuickNode URL; it may contain an API key, keep it out of git and chat) | no |
| dry run | `.\target\release\trader-cli.exe mainnet-canary --mint <PUMP_MINT>` | **no**: full engine path, signature zeroed, `simulateTransaction` only |
| send | `$env:MAINNET_TRADING_ENABLED = "I_UNDERSTAND_REAL_FUNDS_AT_RISK"` then `... mainnet-canary --mint <PUMP_MINT> --iterations 3 --send`, type `SEND 3 MAINNET` | **yes** |

Hard caps in code: 0.001 SOL per buy (default 120,000 lamports ≈ US$0.01 at US$83/SOL),
10 round trips per run, priority ≤ 200,000 µlamports/CU, preflight always on, genesis
must be mainnet. Each round trip is buy → sell 100%; the run stops at the first trade
that is not FINALIZED with balances independently verified. Report:
`benchmarks/results/mainnet-canary-send-*.json` (contains the wallet address and signatures, which are public on-chain anyway).

Cost (measured on **mainnet**, 2026-11-12): the first buy with a new wallet locks 2,860,040
lamports of rent (Token-2022 account 1,513,840 + Pump `user_volume_accumulator` 1,346,200 at
5,080 lamports/byte; the canary reads the live rate) and each further coin adds one token
account; recoverable only by closing those accounts (not automated yet). A 120,000-lamport
round trip with `--senders tpu,jito` lost ~21,000 lamports (2 × 8,000 fee, 2 × 1,000 Jito
tip, Pump fees). First run: 6/6 FINALIZED (BENCHMARKS.md 2g). Helius RPC key: store it as
`RPC_URL` in `~/.trader/env` on the VM (mode 600), never in chat or the repo.

The mint must be a Pump coin still on its bonding curve (not migrated, not mayhem mode);
the adapter refuses others. `--localnet-test` runs the same command against a local
validator (airdrops, no confirmation; refuses public clusters).

## Enabling a tiny, allowlisted mainnet trial (owner action only)
Preconditions: PgStore integration tests passing against Postgres; security review of SECURITY.md residual risks; budget ≤ $20 cash approved in writing.
1. Fresh disposable wallet per tester; fund ≤ 0.05 SOL.
2. `TRADER_MODE=mainnet`, mainnet `RPC_URL`, `DATABASE_URL`, `BETA_ALLOWLIST_TELEGRAM_IDS=<ids>`, `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`.
3. For each tester wallet set policy `mainnet_enabled=true`, `max_order` ≤ 0.01 SOL, `max_daily_turnover` ≤ 0.05 SOL, `max_tip` 0 unless testing a tipped sender.
4. Only liquid, non-migrated Pump curves (adapter refuses others). Record every trade (signature, fees, slot delta) into `benchmarks/results/`.
5. Stop at the first anomaly or when cumulative cash spend reaches the approved limit. Report results with sample sizes; small samples are inconclusive.
