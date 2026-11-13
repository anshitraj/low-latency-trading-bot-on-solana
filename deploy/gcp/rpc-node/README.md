# Own Solana RPC node (test) — `trader-rpc-node`

Non-voting Agave 4.4.1 RPC node + `tools/shred-observer`, project `vps-sandbox`,
`europe-west3-a`, `n2-highmem-64` (64 vCPU, 512 GB) + 8 × 375 GB local NVMe (RAID0).
RPC 8899/8900 reachable from trader-vm (10.30.0.2) only; Solana P2P ports 8000–8030 open.
Cost ~US$4.5/hour while running (GCP credits). Stop only when the owner says so.

## What survives a stop

| what | where | survives stop? |
|---|---|---|
| agave binary, observer binary, configs, node identity, systemd units | boot disk (100 GB pd-balanced) | yes |
| **snapshot archives** (1 full ~116 GB + 2 incrementals) | persistent disk `trader-rpc-node-snapshots` (300 GB pd-balanced, ~US$36/month) mounted at `/mnt/persist` | **yes** |
| ledger, accounts | local NVMe RAID `/mnt/solana` | no (wiped, rebuilt from the snapshot) |
| boot-disk backup | snapshot `trader-rpc-node-boot-20261113` (~4.5 GB stored) | yes, even if the VM is deleted |

`snapsync.timer` copies the newest full/incremental archive to `/mnt/persist/snap` every
10 minutes (keeps 1 full + 2 incrementals), so the backup stays fresh while the node runs.

## Start after a stop (no manual steps)

```
gcloud compute instances start trader-rpc-node --project vps-sandbox --zone europe-west3-a
```
On boot: `node-prep.service` rebuilds the NVMe RAID and mounts the persistent disk →
`sol.service` starts agave with `--full-snapshot-archive-path /mnt/persist/snap` →
`shred-observer.service` starts. Agave reuses the local snapshot if it is within
15,000 slots (~50 min) of the newest one on the network; otherwise, if the network's
latest full snapshot has the same base, it downloads only the incremental (~0.4 GB)
instead of 116 GB. Then: unpack + account index (~10 min) and catch-up.
Stop (wipes the NVMe; the persistent disk stays attached):
```
gcloud compute instances stop trader-rpc-node --project vps-sandbox --zone europe-west3-a --discard-local-ssd=true
```

Not yet verified end to end: a stop → start using the backup (the owner asked to keep the
node running). Expected: well under the ~35–40 min of a cold start.

## Files

- `node-setup.sh` — first-time packages, RAID, sysctl, user, Agave CLI tools, identity
- `node-build.sh` — build `agave-validator` from the tag
- `node-start.sh` — start script + `sol.service`
- `node-prep.sh` / `node-units.sh` — boot-time disk prep + systemd units (prep, sol,
  shred-observer, snapsync timer)
- `snapsync.sh` — keep the persistent snapshot backup fresh
- `run-observer.sh` — shred observer launcher (`--emit 10.30.0.2:9900 --watch-bind 0.0.0.0:9901`)
