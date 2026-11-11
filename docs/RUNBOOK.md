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

## Enabling a tiny, allowlisted mainnet trial (owner action only)
Preconditions: PgStore integration tests passing against Postgres; security review of SECURITY.md residual risks; budget ≤ $20 cash approved in writing.
1. Fresh disposable wallet per tester; fund ≤ 0.05 SOL.
2. `TRADER_MODE=mainnet`, mainnet `RPC_URL`, `DATABASE_URL`, `BETA_ALLOWLIST_TELEGRAM_IDS=<ids>`, `MAINNET_TRADING_ENABLED=I_UNDERSTAND_REAL_FUNDS_AT_RISK`.
3. For each tester wallet set policy `mainnet_enabled=true`, `max_order` ≤ 0.01 SOL, `max_daily_turnover` ≤ 0.05 SOL, `max_tip` 0 unless testing a tipped sender.
4. Only liquid, non-migrated Pump curves (adapter refuses others). Record every trade (signature, fees, slot delta) into `benchmarks/results/`.
5. Stop at the first anomaly or when cumulative cash spend reaches the approved limit. Report results with sample sizes; small samples are inconclusive.
