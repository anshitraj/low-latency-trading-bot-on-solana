# ADR 0003 — Sender routing: same bytes everywhere, deterministic scoring
Date: 2026-11-11 · Status: accepted

All senders receive the identical signed bytes; nothing is ever re-signed while
a signature may land. Outcomes distinguish definitive refusal from ambiguity;
only all-definitive refusals reject an order. Routing score is deterministic
(health + latency); leader-aware and experimental routing stay off until data
justifies them. Tip planning refuses sender sets with incompatible tip rules.
TPU/QUIC is not built: without stake/peering it offers no QoS advantage that we
could verify, and the maintained client targets a newer Solana crate line.
