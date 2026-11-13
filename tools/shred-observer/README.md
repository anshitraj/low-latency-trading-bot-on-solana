# trader-shred-observer

Read-only sidecar for our own RPC node: copies the shreds the node already
receives, rebuilds each entry batch with Agave's own code, and sends every new
transaction signature (+ slot) over UDP to trader-vm the moment its batch is
complete — before replay and before any RPC notification.

Build on the node (inside the Agave checkout at the node's tag, so versions match):

```
cp -r tools/shred-observer ~/agave/trader-shred-observer
cd ~/agave && sed -i 's/^members = \[/members = [\n    "trader-shred-observer",/' Cargo.toml
cargo build --release -p trader-shred-observer
sudo ./target/release/trader-shred-observer --emit 10.30.0.2:9900
```

Collector: `trader-cli mainnet-canary ... --shred-feed 0.0.0.0:9900` adds
`shred_seen_ms_after_signed` per trade; `scripts/logs_race.py` compares the feed
with Helius for all Pump.fun transactions (read-only).

Not yet: Reed-Solomon recovery, leader signature verification (measurement only).
