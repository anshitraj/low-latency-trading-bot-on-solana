# API

Full contract: [`openapi.yaml`](../openapi.yaml) (TypeScript types generated
into `apps/telegram-webapp/src/lib/openapi.d.ts` via `pnpm gen:types`).

## Conventions
- Amounts are integer base units as **decimal strings** (`"50000000"` lamports). Floats and exponents are rejected.
- Mints are identities; symbols/names are never trusted.
- Auth: `Authorization: Bearer <token>` from `POST /v1/auth/telegram`. Tokens expire in 30 minutes (`SESSION_TTL_SECS`).
- Errors: `{"error":{"code":"policy_violation","message":"order 0.3 SOL exceeds per-order max 0.25 SOL"}}` with HTTP 400/401/403/404/409/422/429/501/503.
- Another user's resources return **404** (existence is not revealed).

## Authentication
```http
POST /v1/auth/telegram
{"init_data": "<Telegram.WebApp.initData verbatim>"}

200 {"token":"Q2x…","expires_at":"2026-11-11T18:00:00Z","user":{"id":"9b0…","telegram_id":4242,"username":"alice","beta_allowed":true,"created_at":"…"},"start_param":null}
401 {"error":{"code":"invalid_init_data","message":"initData hash mismatch"}}
401 {"error":{"code":"init_data_replayed","message":"this initData was already exchanged; reopen the Mini App"}}
```

## Trading flow
Values below are taken from the localnet Pump smoke run where possible (amounts, fee, debit, expected/min out); ids, slots and ages are illustrative.
```http
POST /v1/orders/preview            (nothing persisted)
{"mint":"Gx…pump","side":"buy","amount_sol":"0.05"}
200 {"quote":{"side":"buy","in_amount":"50000000","expected_out":"151660771980837","min_out":"144077733381795",
      "costs":{"base_fee":"5000","priority_fee":"150","tip":"0","rent_deposit":"3918480","protocol_fee":"…(estimate from pump-fees tiers)"},
      "max_total_debit":"53923630","context_slot":51,"state_age_ms":12,"expires_at":"…","source":{"kind":"on_chain"},"warnings":[…]}}

POST /v1/orders                    Idempotency-Key: mini:7b1c…
{"mint":"Gx…pump","side":"buy","amount_sol":"0.05"}
201 {"order":{"id":"…","state":"AWAITING_AUTH","quote":{…},"signed":null,"fill":null}}
(repeat with the same key → same order)

POST /v1/orders/{id}/approve       (idempotent)
200 {"order":{"state":"SUBMITTED","signed":{"signature":"59gy…","last_valid_block_height":201,…}}}

GET /v1/orders/{id}
200 {"order":{"state":"FINALIZED","fill":{"slot":51,"success":true,"fee_lamports":5150,"sol_delta_lamports":-53923630,"token_delta_raw":151660771980837,…}},
     "status_text":"filled (finalized)","events":[{"from":null,"to":"CREATED",…},…]}

POST /v1/orders/{id}/cancel        (only before signing; afterwards a no-op)
```
Sell: `{"side":"sell","percent":50}` (of spendable balance) or `{"side":"sell","amount_raw":"1000000"}`.

Status semantics: `SUBMITTED` = handed to a sender (not landed); `OBSERVED` =
processed; success only at `CONFIRMED`/`FINALIZED`; `FAILED_ONCHAIN` = landed
with error (fee paid); `EXPIRED` = can no longer land (no fee);
`UNKNOWN_RECONCILE` = do not retry, being reconciled.

## Other endpoints
`GET /v1/health`, `GET /v1/ready`, `GET /v1/me`, `GET|POST /v1/wallets`,
`GET /v1/portfolio`, `GET /v1/tokens/{mint}`, `GET /v1/tokens/{mint}/quote?side=buy&amount=0.1`,
`GET /v1/orders`, `GET /v1/transactions/{signature}`,
`GET|POST|DELETE /v1/tracked-wallets`, `GET|POST /v1/settings` (`{"paused":true}` = kill switch),
`GET /v1/events` (SSE, `order` events), `POST /v1/dev/airdrop` (mock/localnet/devnet),
`GET|POST|DELETE /v1/copy-rules` and `GET /v1/referrals` → **501 not implemented**.
Private (ADMIN_TOKEN, otherwise 404): `GET /metrics`, `GET /v1/provider-health`.

## Telegram webhook
`POST /telegram/webhook` with `X-Telegram-Bot-Api-Secret-Token`. 401 on bad
secret; 200 for accepted or duplicate `update_id`; 503 when the bounded queue is
full (Telegram retries; nothing is marked seen).

## Telegram commands
`/start [mint]`, `/help`, `/wallet`, `/balance`, `/buy <mint> <SOL>`,
`/sell <mint> <percent>`, `/positions`, `/orders`, `/settings`,
`/set slippage|presets|confirm|priority|maxfee|tip <value>`, `/track <wallet>`,
`/untrack <wallet>`, `/pause`, `/resume`, `/status`, `/copy`, `/referrals`,
`/export`, `/support`. Pasting a mint shows a token panel with preset buy and
25/50/75/100% sell buttons (120 s, single-use).
