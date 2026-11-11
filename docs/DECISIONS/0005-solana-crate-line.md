# ADR 0005 — Solana 2.x component crates (for now)
Date: 2026-11-11 · Status: accepted, revisit

The official pump-rust-client 0.5.0 declares anchor-lang ">=0.31,<2". Cargo
initially resolved anchor 1.2.2 (Solana 4.x/5.x types), which would have mixed
three Solana type lines in one binary. We pinned anchor-lang/anchor-spl 0.31.1
and solana-program 2.3 in Cargo.lock so every Pubkey/Instruction type is the
same, and use only Solana 2.x component crates (no solana-sdk/solana-client).
Cost: cargo-audit advisories on ed25519-dalek 1.0.1 / curve25519-dalek 3.2.0
(not reachable through our matched-keypair signing). Revisit: migrate the
workspace to the anchor 1.x / newer Solana line, re-run all tests + localnet smoke.
