#!/usr/bin/env bash
# Install + start the non-voting mainnet RPC node as a systemd service. Run as root.
# RPC/WS bind 0.0.0.0 but the GCP firewall only lets trader-vm (10.30.0.2) reach
# 8899-8900; --private-rpc keeps the RPC port out of gossip.
set -euo pipefail
cat >/home/sol/start-node.sh <<'EOF'
#!/usr/bin/env bash
exec /home/sol/bin/agave-validator \
  --identity /home/sol/node-identity.json \
  --no-voting \
  --ledger /mnt/solana/ledger \
  --accounts /mnt/solana/accounts \
  --snapshots /mnt/solana/snapshots \
  --full-snapshot-archive-path /mnt/persist/snap \
  --incremental-snapshot-archive-path /mnt/persist/snap \
  --maximum-local-snapshot-age 15000 \
  --log /home/sol/agave.log \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint2.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint3.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint4.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint5.mainnet-beta.solana.com:8001 \
  --known-validator 7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2 \
  --known-validator GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ \
  --known-validator DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ \
  --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S \
  --only-known-rpc \
  --expected-genesis-hash 5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d \
  --dynamic-port-range 8000-8030 \
  --gossip-port 8001 \
  --rpc-port 8899 \
  --rpc-bind-address 0.0.0.0 \
  --private-rpc \
  --full-rpc-api \
  --wal-recovery-mode skip_any_corrupted_record \
  --maximum-full-snapshots-to-retain 1 \
  --maximum-incremental-snapshots-to-retain 2
EOF
chmod +x /home/sol/start-node.sh
chown sol:sol /home/sol/start-node.sh
cat >/etc/systemd/system/sol.service <<'EOF'
[Unit]
Description=Solana mainnet non-voting RPC node (test)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=sol
LimitNOFILE=1000000
LimitMEMLOCK=2000000000
# Agave 4.4 refuses to start without these (network setup); granted to this unit only.
AmbientCapabilities=CAP_NET_RAW CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN
Restart=on-failure
RestartSec=5
ExecStart=/home/sol/start-node.sh

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl restart sol
sleep 5
systemctl is-active sol
