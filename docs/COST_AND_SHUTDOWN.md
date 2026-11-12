# Cost plan and shutdown

## Money buckets (never mixed)
| bucket | limit | pays for |
|---|---|---|
| GCP credits | $5,000 available; **plan ≤ $1,200** for 30 days | VM, disk, IP, snapshots, egress, Secret Manager, KMS, logging |
| Real cash | **$20 total**, owner-approved per use | SOL for mainnet micro-tests (fees, rent, tips) — none spent |
| SOL fees for end users | paid by each tester from their own wallet | network fees, priority fees, tips, rent |

**Actual spend:** GCP: `trader-vm` (c4-standard-4, europe-west3) running since 2026-11-12 ~09:26 UTC in `vps-sandbox`, paid from credits (~US$0.23/h estimate; check Billing → Reports). Cash: owner sent 0.013 SOL (~US$1) to the canary wallet. After the first 6 mainnet trades: 10,077,061 lamports left; 2,860,040 held as account rent (recoverable by closing the accounts) and 62,899 lamports actually consumed (~US$0.005).
Devnet reads (public RPC) and local validator runs cost nothing.

## Estimate (europe-west3, on-demand) — ESTIMATE, verify in the GCP Pricing Calculator
Prices from the third-party aggregator gcloud-compute.com (europe-west3 page,
dated 2026-11-01), accessed 2026-11-11; Google's own pricing page lists
c4-standard-4 at a different (region-dependent) rate, so treat as approximate.

| item | basis | ~30 days |
|---|---|---:|
| c4-standard-4 VM (4 vCPU, 15 GB) | $0.2333/h × 730 h | $170 |
| alt: e2-standard-4 | $0.1727/h | ($126) |
| pd-balanced 50 GB + 7 daily snapshots | ~ $0.12–0.13/GB-mo + incremental snapshots | $8–12 |
| static external IP (in use) | ~ $0.005/h | $4 |
| egress (Telegram, RPC, Mini App) | low single-digit GB | $1–5 |
| Secret Manager (6 secrets, low access) | | <$1 |
| Cloud KMS (1 key, few ops) | | <$1 |
| Logging (7-day retention, low volume) | free tier likely | $0–5 |
| region A/B test (`region-ab.sh`, 2× e2-standard-2 for ~10 min) | per run | <$0.10 |
| **total** | | **≈ $185–200** |

Even a second always-on VM for the full month stays under ≈ $400 — well inside
the $1,200 plan. Mainnet SOL costs (if authorized): ~5,000–10,000 lamports per
trade attempt in base+priority fees, plus refundable ATA rent (~0.002 SOL) and
pump's user volume accumulator rent (1,346,200 lamports at mainnet's 5,080 lamports/byte, first trade), plus optional
tips (Helius ≥0.000005 or ≥0.001 SOL; Jito ≥0.000001 SOL). Swap principal is
the tester's own money.

## Alerts (Terraform `google_billing_budget`)
Thresholds at 25%, 50%, 80%, 95%, 100% of `budget_usd` (default 1200) on
**gross usage before credits** (`EXCLUDE_ALL_CREDITS`, i.e. credit burn) plus an
80% forecasted-spend alert. **Budget alerts do not cap spending** and billing
data lags by hours. Optional hard stop: wire the budget's Pub/Sub topic to a
small function that runs the equivalent of `stop-all.sh` (not implemented here;
also investigate service-specific spend caps:
https://docs.cloud.google.com/billing/docs/how-to/budgets-spend-caps).

Daily check (5 min): Billing → Reports (group by SKU, last 7 days) · Credits
remaining · Compute → VM instances list (only `trader-vm`) · Snapshots count.

## Shutdown
```
GCP_PROJECT=my-project deploy/gcp/scripts/stop-all.sh            # stop compute billing
GCP_PROJECT=my-project deploy/gcp/scripts/stop-all.sh --delete   # snapshot, delete VMs, release IP
terraform destroy …                                               # everything except the KMS key (prevent_destroy)
```
Stopping the service does not stop transactions already signed and sent; they
can still land until their blockhash expires (~60–90 s). Restarting reconciles them.
