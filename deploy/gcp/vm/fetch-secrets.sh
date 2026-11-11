#!/usr/bin/env bash
# Writes /run/trader/env (tmpfs, 0600) from Secret Manager using the VM's
# service account (metadata server token; no static keys).
set -euo pipefail
umask 077
out=/run/trader/env
get() { gcloud secrets versions access latest --secret="trader-$1"; }
{
  echo "TRADER_MODE=${TRADER_MODE:-devnet}"
  echo "RPC_URL=$(get rpc-url)"
  echo "TELEGRAM_BOT_TOKEN=$(get telegram-bot-token)"
  echo "TELEGRAM_WEBHOOK_SECRET=$(get telegram-webhook-secret)"
  echo "KEK_BASE64=$(get kek-base64)"
  echo "ADMIN_TOKEN=$(get admin-token)"
  echo "POSTGRES_PASSWORD=$(get db-password)"
  echo "DATABASE_URL=postgres://trader:$(get db-password)@postgres:5432/trader"
  echo "PUBLIC_URL=${PUBLIC_URL:-}"
  echo "BETA_ALLOWLIST_TELEGRAM_IDS=${BETA_ALLOWLIST_TELEGRAM_IDS:-}"
  echo "TELEGRAM_SET_WEBHOOK=true"
  # Mainnet trading stays disabled unless the operator edits this file deliberately.
  echo "MAINNET_TRADING_ENABLED="
} > "$out"
chmod 600 "$out"
