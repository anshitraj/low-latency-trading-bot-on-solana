#!/usr/bin/env bash
# Start a LOCAL solana-test-validator with the official Pump program binaries
# (shipped in the MIT-licensed pump-rust-client 0.5.0 crate) and the Pump
# admin PDAs cloned from devnet by `trader-cli clone-pump-accounts`.
#
# Usage: scripts/localnet.sh [--fresh-clone]
# Windows: Agave 4.2's solana-test-validator cannot create a ledger on Windows
# ("Access is denied" unpacking genesis). Use scripts/localnet-wsl.sh, which
# runs this script inside WSL2 (Linux validator); WSL forwards 127.0.0.1:8899.
# Requires: solana-test-validator on PATH, cargo registry containing pump-rust-client 0.5.0.
# Costs: nothing. Reads public devnet RPC once (clone step). No mainnet access.
set -euo pipefail
cd "$(dirname "$0")/.."

ART="${PUMP_ARTIFACTS:-$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/pump-rust-client-0.5.0/artifacts 2>/dev/null | head -1)}"
if [[ -z "${ART}" ]]; then
  echo "pump-rust-client 0.5.0 not in cargo registry; run: cargo fetch" >&2
  exit 1
fi

if [[ "${1:-}" == "--fresh-clone" || ! -f localnet/accounts/manifest.json ]]; then
  cargo run -q -p trader-cli -- clone-pump-accounts --rpc "${DEVNET_RPC:-https://api.devnet.solana.com}" --out localnet/accounts
fi

ARGS=(--reset --ledger "${LEDGER:-localnet/ledger}" --quiet --rpc-port "${LOCALNET_RPC_PORT:-8899}" --faucet-port "${LOCALNET_FAUCET_PORT:-9900}")
ARGS+=(--bpf-program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P "$ART/pump.so")
ARGS+=(--bpf-program pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA "$ART/pump_amm.so")
ARGS+=(--bpf-program pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ "$ART/pump_fees.so")
ARGS+=(--bpf-program MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e "$ART/mayhem_program.so")
ARGS+=(--bpf-program metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s "$ART/mpl_token_metadata.so")
ARGS+=(--bpf-program AgenTMiC2hvxGebTsgmsD4HHBa8WEcqGFf87iwRRxLo7 "$ART/pump_agent_payments.so")
for f in localnet/accounts/*.json; do
  [[ "$(basename "$f")" == "manifest.json" ]] && continue
  key="$(grep -m1 '"pubkey"' "$f" | sed -E 's/.*"pubkey": *"([^"]+)".*/\1/')"
  ARGS+=(--account "$key" "$f")
done

echo "starting solana-test-validator with $(( (${#ARGS[@]}) )) args (pump artifacts: $ART)"
exec solana-test-validator "${ARGS[@]}"
