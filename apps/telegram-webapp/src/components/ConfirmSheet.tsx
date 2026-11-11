"use client";
import { useEffect, useState } from "react";
import type { Order } from "@/lib/api";
import { bps, formatUnits, sol } from "@/lib/money";
import { Btn, Row } from "./ui";

/** Bottom sheet showing the server's exact quote. Confirming approves THIS quote's thresholds. */
export function ConfirmSheet({ order, onConfirm, onCancel, busy }: { order: Order; onConfirm: () => void; onCancel: () => void; busy: boolean }) {
  const q = order.quote;
  const [left, setLeft] = useState<number>(0);
  useEffect(() => {
    if (!q) return;
    const t = () => setLeft(Math.max(0, Math.round((new Date(q.expires_at).getTime() - Date.now()) / 1000)));
    t();
    const id = setInterval(t, 500);
    return () => clearInterval(id);
  }, [q]);
  if (!q) return null;
  const buy = q.side === "buy";
  const src = q.source.kind === "mock" ? "MOCK pool (not real)" : q.source.kind === "on_chain" ? `on-chain @ slot ${q.context_slot} (${q.state_age_ms} ms old)` : q.source.provider ?? "external";
  return (
    <div role="dialog" aria-modal="true" aria-labelledby="confirm-title" className="fixed inset-0 z-50 flex items-end bg-black/60">
      <div className="mx-auto w-full max-w-lg rounded-t-3xl border border-line bg-panel p-5 pb-8">
        <h3 id="confirm-title" className="mb-3 text-lg font-semibold">
          Confirm {buy ? "buy" : "sell"}
        </h3>
        <Row k="You pay" v={buy ? `${sol(q.in_amount, 9)} SOL` : `${formatUnits(q.in_amount, q.in_decimals)} tokens`} />
        <Row k="Expected" v={buy ? `${formatUnits(q.expected_out, q.out_decimals)} tokens` : `${sol(q.expected_out, 9)} SOL`} />
        <Row k="Minimum received" v={buy ? `${formatUnits(q.min_out, q.out_decimals)} tokens` : `${sol(q.min_out, 9)} SOL`} />
        <Row k="Price impact" v={bps(q.price_impact_bps)} />
        <Row k="Network + priority fee" v={`${sol(BigInt(q.costs.base_fee) + BigInt(q.costs.priority_fee), 9)} SOL`} />
        <Row k="Tip" v={`${sol(q.costs.tip, 9)} SOL`} />
        <Row k="Protocol/creator fee (est.)" v={`${sol(q.costs.protocol_fee, 9)} SOL`} />
        <Row k="Rent deposit (refundable)" v={`${sol(q.costs.rent_deposit, 9)} SOL`} />
        <div className="my-2 border-t border-line" />
        <Row k="Max total spend" v={<span data-testid="max-spend">{sol(q.max_total_debit, 9)} SOL</span>} strong />
        <p className="mt-2 text-xs text-muted">Quote source: {src}. Slippage protection is enforced on-chain; a failed transaction still pays the network fee.</p>
        {q.warnings.map((w) => (
          <p key={w} className="mt-1 text-xs text-warn">
            ⚠ {w}
          </p>
        ))}
        <p className="mt-2 text-xs text-muted" aria-live="polite">
          {left > 0 ? `Quote valid ~${left}s (re-quoted if expired; refused if worse than this minimum).` : "Quote expired — confirming will re-quote."}
        </p>
        <div className="mt-4 grid grid-cols-2 gap-3">
          <Btn kind="ghost" onClick={onCancel} disabled={busy}>
            Cancel
          </Btn>
          <Btn kind="primary" onClick={onConfirm} disabled={busy} label="Confirm trade">
            {busy ? "Submitting…" : "Confirm"}
          </Btn>
        </div>
      </div>
    </div>
  );
}
