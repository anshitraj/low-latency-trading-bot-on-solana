# GCP deployment (single VM, private beta)

**Nothing has been provisioned.** `terraform apply` and every script that
creates or deletes resources require explicit owner approval and prompt for
confirmation. Terraform was not run in this session (not installed; no project id).

## Topology
One Compute Engine VM (`trader-vm`, default `c4-standard-4`, Debian 12,
shielded VM, OS Login, 50 GB pd-balanced) in `europe-west3-a`:
Caddy (TLS, Let's Encrypt) on :443 → `trader-api` container on 127.0.0.1:8080 →
Postgres container (local disk, not exposed). Static IP. SSH only through IAP
(35.235.240.0/20). Secrets in Secret Manager, fetched at service start into
tmpfs (`/run/trader/env`, 0600) by the VM's service account (no key files).

## Files
| path | purpose |
|---|---|
| `deploy/gcp/terraform/*.tf` | APIs, service account (log/metric writer + per-secret accessor + KMS key user), secrets (empty), KMS key ring/key (90-day rotation), static IP, firewall (80/443 public, 22 IAP-only), VM, daily snapshot policy (7-day retention), `_Default` log bucket 7-day retention, billing budget (25/50/80/95/100% actual + 80% forecast) |
| `deploy/gcp/vm/startup.sh` | installs Docker, Caddy, chrony; bounds journald |
| `deploy/gcp/vm/trader-api.service` | systemd unit (restart limits, graceful 30 s stop) |
| `deploy/gcp/vm/fetch-secrets.sh` | Secret Manager → tmpfs env file; `MAINNET_TRADING_ENABLED` left empty |
| `deploy/gcp/vm/docker-compose.prod.yml` | Postgres + API with log rotation |
| `deploy/gcp/vm/Caddyfile` | TLS, blocks `/metrics` + `/v1/provider-health`, strips query strings/auth headers from logs |
| `deploy/gcp/scripts/deploy.sh` | build image, ship over IAP, keep `previous` tag, restart, health check; `ROLLBACK=1` |
| `deploy/gcp/scripts/stop-all.sh` | stop all `app=trader` VMs; `--delete` snapshots then deletes and releases the IP |
| `deploy/gcp/scripts/backup-db.sh` | encrypted `pg_dump` to local file; `--restore` drill into a scratch DB |
| `deploy/gcp/scripts/region-ab.sh` | temporary VMs in two regions, RTT to RPC/Jito endpoints, then deletes them |

## Procedure (owner-run)
1. Verify credits: Billing → Credits; note type, scope (eligible services) and **expiry**. Confirm Compute Engine is eligible.
2. `cd deploy/gcp/terraform && terraform init && terraform plan -var project_id=… -var project_number=… -var billing_account=…` — review.
3. With approval: `terraform apply …`.
4. Add secret values: `printf %s "$TOKEN" | gcloud secrets versions add trader-telegram-bot-token --data-file=-` (same for webhook secret, `openssl rand -base64 32` KEK, db password, admin token, RPC URL).
5. DNS A record → `terraform output vm_ip`; edit the Caddyfile domain.
6. `GCP_PROJECT=… deploy/gcp/scripts/deploy.sh` (starts in `TRADER_MODE=devnet` unless changed in `fetch-secrets.sh`).
7. Set `PUBLIC_URL` and `BETA_ALLOWLIST_TELEGRAM_IDS`; the service registers the webhook (`TELEGRAM_SET_WEBHOOK=true`).
8. Optional: `REGIONS="europe-west3-a asia-southeast1-b" deploy/gcp/scripts/region-ab.sh` and keep the better region (do not run two VMs permanently without data).

## Hardening checklist
- [x] No public SSH; IAP only. [x] Shielded VM, OS Login. [x] Least-privilege SA, no SA keys.
- [x] Secrets only in Secret Manager/tmpfs. [x] TLS + HSTS. [x] Private endpoints blocked at proxy and token-gated in app.
- [x] Short log retention; container log rotation; journald caps. [x] NTP (chrony) for wall-clock correlation.
- [x] Daily snapshots + encrypted logical backups + restore drill script.
- [ ] Cloud Armor / per-IP rate limiting (not configured). [ ] Uptime check + alerting policy (create in Cloud Monitoring; not in TF). [ ] KMS-wrapped KEK (not implemented).

## Rollback & migrations
`ROLLBACK=1 deploy.sh` re-tags `trader-api:previous`. Migrations are additive and
run at startup; to roll back a migration restore from the pre-deploy backup
(take one with `backup-db.sh` before every deploy). On restart, in-flight orders
are reconciled from persisted signatures; new orders are refused while the
engine queue is not running (503).

## Cost
See COST_AND_SHUTDOWN.md.
