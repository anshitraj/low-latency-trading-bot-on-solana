#!/usr/bin/env bash
# Run the local validator inside WSL2 Ubuntu (see localnet.sh header).
# Requires solana-test-validator installed inside the WSL distro.
set -euo pipefail
cd "$(dirname "$0")/.."
to_wsl() { # /e/foo or E:/foo -> /mnt/e/foo
  local p="${1//\\//}"
  if [[ "$p" =~ ^([A-Za-z]):/(.*)$ ]]; then echo "/mnt/${BASH_REMATCH[1],,}/${BASH_REMATCH[2]}"; return; fi
  if [[ "$p" =~ ^/([A-Za-z])/(.*)$ ]]; then echo "/mnt/${BASH_REMATCH[1],,}/${BASH_REMATCH[2]}"; return; fi
  echo "$p"
}
REPO_WSL="$(to_wsl "$(pwd)")"
ART_WIN="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/pump-rust-client-0.5.0/artifacts | head -1)"
ART_WSL="$(to_wsl "$ART_WIN")"
[[ -f localnet/accounts/manifest.json ]] || cargo run -q -p trader-cli -- clone-pump-accounts --rpc "${DEVNET_RPC:-https://api.devnet.solana.com}" --out localnet/accounts
echo "repo=$REPO_WSL artifacts=$ART_WSL"
exec wsl -d "${WSL_DISTRO:-Ubuntu}" -- bash -lc "cd '$REPO_WSL' && PUMP_ARTIFACTS='$ART_WSL' LEDGER=/root/trader-localnet-ledger bash scripts/localnet.sh"
