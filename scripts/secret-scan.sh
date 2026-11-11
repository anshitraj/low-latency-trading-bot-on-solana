#!/usr/bin/env bash
# Lightweight secret scan over tracked files (CI also runs gitleaks).
# Fails if anything that looks like a real credential is committed.
set -euo pipefail
cd "$(dirname "$0")/.."
patterns=(
  '[0-9]{8,10}:AA[0-9A-Za-z_-]{33}'                # Telegram bot token
  '-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----'
  '"private_key":'                                  # GCP service-account JSON
  'AKIA[0-9A-Z]{16}'                                # AWS key id
  'api-key=[0-9a-f]{8}-[0-9a-f]{4}-'                # Helius-style key in URL
  '\[([0-9]{1,3},){63}[0-9]{1,3}\]'                 # 64-byte Solana keypair JSON array
)
found=0
for p in "${patterns[@]}"; do
  if git ls-files -z | xargs -0 grep -nEI -- "$p" -- 2>/dev/null | grep -v -E '^(scripts/secret-scan.sh|docs/)' ; then
    found=1
  fi
done
if [[ $found -ne 0 ]]; then
  echo "secret-scan: potential secrets found (see above)" >&2
  exit 1
fi
echo "secret-scan: clean ($(git ls-files | wc -l) tracked files)"
