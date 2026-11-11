# ADR 0002 — Wallet custody: managed test wallets with envelope encryption
Date: 2026-11-11 · Status: accepted (private beta only)

Context: one-tap trading needs a server-side signer; external-wallet signing
adds a round trip per trade and is not built yet.
Decision: per-user managed keypairs; seed sealed with AES-256-GCM under a
per-wallet DEK wrapped by a KEK (Secret Manager); wallet id as AAD; decrypted
signer cached in memory. No export, no withdrawals.
Consequences: custodial risk (VM compromise = key compromise). Before any wider
beta: KMS-wrapped KEK (option B) and evaluation of per-user KMS Ed25519 keys
(option C, latency to be measured). See SECURITY.md.
