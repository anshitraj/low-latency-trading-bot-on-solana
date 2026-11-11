# 30-day delivery plan (with objective gates)

| days | work | pass/fail gate | status (day 1) |
|---|---|---|---|
| 1–3 | docs verification, repo, domain types, mock chain, CI, threat model | `cargo test` green; CI file present; ADRs | **done** |
| 4–8 | Pump adapter, sign-once, RPC sender, observer, idempotency, localnet | real `pump.so` buy+sell FINALIZED with verified balances | **done (localnet)** |
| 4–8 | PgStore | `cargo test -p trader-api --test pg` green on Postgres | **blocked: no Postgres** |
| 9–12 | Telegram flows, callbacks, notifications | mock Bot API tests incl. replay/spoof | **done (mock)**; real bot pending token |
| 9–12 | devnet run | ≥10 devnet trades FINALIZED via public RPC | not started |
| 13–17 | Jito/Helius live, TPU evaluation, dynamic fees, WebSocket state | sender health + measured ack/landing per sender on devnet/mainnet micro | senders mock-tested only |
| 13–17 | benchmark harness | JSON results with p50/p95/p99 | **done** (offline/mock/localnet) |
| 18–21 | Mini App | Playwright desktop+mobile green | **done (mock)** |
| 18–21 | PumpSwap adapter | localnet post-migration buy/sell | not started |
| 22–25 | tracking alerts, copy trading (caps, kill switch), TP/SL | only if gates above are green | not started |
| 26–28 | reliability: 24 h soak, restore drill, security review, cost audit | zero duplicate executions; restore drill passes | fault tests done; soak/restore not run |
| 29–30 | private beta: allowlisted testers, authorized micro-trades | ≤ $20 cash, report with sample sizes | not started |
