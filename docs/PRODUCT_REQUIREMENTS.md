# Product requirements

Original Solana-only Telegram trading bot + Mini App for a **private beta**.
Inspired by the workflow of popular Telegram trading terminals (paste a mint,
one-tap presets, sell percentages, positions, tracked wallets) with an original
UI and no copied branding, assets or claims.

## Users
- **Tester (private beta)**: allowlisted Telegram account, small test funds, wants fast manual trades on Pump bonding-curve coins with clear max-spend and honest status.
- **Operator**: deploys, monitors, pauses, reconciles, controls mainnet enablement and budget.

## User stories & acceptance (P0)
| story | acceptance | status |
|---|---|---|
| Start the bot and understand risks | `/start` shows custody + risk disclosure, never asks for seed | done (tested) |
| Get a test wallet | button creates managed wallet; address shown; no key in chat | done (tested) |
| Paste a mint → panel | market, liquidity, authorities, Token-2022 extensions, holder concentration or "Unknown", disclaimer, preset buttons | done (tested in mock; real mint path depends on RPC) |
| Buy preset with confirmation | quote shows expected/min out, impact, every fee, rent, **max total spend**; confirm executes those thresholds | done (tested) |
| Sell 25/50/75/100% | uses spendable on-chain balance minus in-flight reservations | done (tested) |
| Honest status | preparing → submitted (signature) → processed → confirmed/finalized/failed/expired/unknown; never "success" from an ack | done (tested) |
| Idempotent buttons | double taps, replays, webhook retries → one order, one swap | done (tested) |
| Positions & PnL | weighted average incl. fees; unrealized only with an executable quote | done (tested) |
| Settings & kill switch | presets, slippage, fees, confirm toggle, pause | done (tested) |
| Mini App | same flows, server-validated initData, confirmation sheet | done (tested, mock mode) |
| Operator safety | mainnet off by default; caps; allowlist; budget alerts; stop-all | done (config tested; GCP not provisioned) |

## P1 (not done in this build)
Wallet tracking alerts (watchlist only now), opt-in copy trading with caps, TP/SL best-effort watcher, PumpSwap (post-migration), token discovery list. External-wallet signing.

## P2 (not done)
Charts, multi-wallet, advanced analytics, referrals/fee sharing, sniping/new-pool streams, leader-aware routing, premium feeds, TPU sender.

## Explicit exclusions
Multi-chain; public custodial launch; withdrawals to arbitrary addresses; key export; any fake volume, wash trading, sandwiching or deceptive KOL features; speed claims without comparable measurements.
