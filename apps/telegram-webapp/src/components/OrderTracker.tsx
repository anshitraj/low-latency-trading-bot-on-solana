"use client";
import { useEffect, useState } from "react";
import { api, TERMINAL, type Order } from "@/lib/api";
import { formatUnits, sol, short } from "@/lib/money";
import { Badge, Panel, Row } from "./ui";

const TONE: Record<string, "muted" | "warn" | "buy" | "sell" | "accent"> = {
  FINALIZED: "buy",
  CONFIRMED: "buy",
  FAILED_ONCHAIN: "sell",
  REJECTED: "sell",
  EXPIRED: "warn",
  UNKNOWN_RECONCILE: "warn",
};

/** Polls the order until terminal. Shows on-chain status only; never "success" from a send ack. */
export function OrderTracker({ orderId, onDone }: { orderId: string; onDone?: (o: Order) => void }) {
  const [order, setOrder] = useState<Order | null>(null);
  const [text, setText] = useState("");
  useEffect(() => {
    let stop = false;
    const tick = async () => {
      try {
        const r = await api<{ order: Order; status_text: string }>("GET", `/v1/orders/${orderId}`);
        if (stop) return;
        setOrder(r.order);
        setText(r.status_text);
        const done = TERMINAL.includes(r.order.state) && (r.order.state !== "FINALIZED" || r.order.fill);
        if (done) {
          onDone?.(r.order);
          return;
        }
      } catch {
        /* transient; keep polling */
      }
      if (!stop) setTimeout(tick, 600);
    };
    tick();
    return () => {
      stop = true;
    };
  }, [orderId, onDone]);

  if (!order) return null;
  const f = order.fill;
  const side = order.intent.side;
  return (
    <Panel title="Order status" right={<Badge tone={TONE[order.state] ?? "accent"}>{order.state}</Badge>}>
      <p data-testid="order-status" className="mb-2 text-sm">
        {text}
      </p>
      {order.signed && <Row k="Signature" v={<code className="text-xs">{short(order.signed.signature, 6)}</code>} />}
      {order.reject_reason && order.state === "REJECTED" && <Row k="Reason" v={order.reject_reason} />}
      {f && (
        <>
          <Row k="Slot" v={f.slot} />
          {f.success ? (
            <Row k={side === "buy" ? "Tokens received" : "Tokens sold"} v={formatUnits(Math.abs(f.token_delta_raw), f.token_decimals ?? 6)} strong />
          ) : (
            <Row k="Error" v={f.error ?? "unknown"} />
          )}
          <Row k={side === "buy" ? "SOL debited (incl. fees/rent)" : "SOL credited (net)"} v={`${sol(Math.abs(f.sol_delta_lamports))} SOL`} />
          <Row k="Network fee" v={`${sol(f.fee_lamports, 9)} SOL`} />
        </>
      )}
    </Panel>
  );
}
