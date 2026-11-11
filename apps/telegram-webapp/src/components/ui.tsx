"use client";
import type { ReactNode } from "react";

export function Panel({ title, children, right }: { title?: string; children: ReactNode; right?: ReactNode }) {
  return (
    <section className="rounded-2xl border border-line bg-panel p-4">
      {title && (
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-semibold tracking-wide text-muted uppercase">{title}</h2>
          {right}
        </div>
      )}
      {children}
    </section>
  );
}

export function Btn({ children, onClick, kind = "default", disabled, label, type = "button" }: { children: ReactNode; onClick?: () => void; kind?: "buy" | "sell" | "default" | "primary" | "ghost"; disabled?: boolean; label?: string; type?: "button" | "submit" }) {
  const cls = {
    buy: "bg-buy/15 text-buy border-buy/40 hover:bg-buy/25",
    sell: "bg-sell/15 text-sell border-sell/40 hover:bg-sell/25",
    primary: "bg-accent text-bg border-accent hover:brightness-110 font-semibold",
    ghost: "bg-transparent text-muted border-line hover:text-text",
    default: "bg-panel-2 text-text border-line hover:border-accent",
  }[kind];
  return (
    <button type={type} aria-label={label} disabled={disabled} onClick={onClick} className={`min-h-11 rounded-xl border px-3 py-2 text-sm transition disabled:cursor-not-allowed disabled:opacity-40 ${cls}`}>
      {children}
    </button>
  );
}

export function Badge({ children, tone = "muted" }: { children: ReactNode; tone?: "muted" | "warn" | "buy" | "sell" | "accent" }) {
  const cls = { muted: "text-muted border-line", warn: "text-warn border-warn/50", buy: "text-buy border-buy/50", sell: "text-sell border-sell/50", accent: "text-accent border-accent/50" }[tone];
  return <span className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs ${cls}`}>{children}</span>;
}

export function Skeleton({ lines = 3 }: { lines?: number }) {
  return (
    <div aria-busy="true" aria-label="Loading" className="space-y-2">
      {Array.from({ length: lines }).map((_, i) => (
        <div key={i} className="h-4 animate-pulse rounded bg-panel-2" style={{ width: `${90 - i * 15}%` }} />
      ))}
    </div>
  );
}

export function ErrorBox({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <div role="alert" data-testid="error" className="rounded-xl border border-sell/40 bg-sell/10 p-3 text-sm text-sell">
      {error}
    </div>
  );
}

export function Row({ k, v, strong }: { k: string; v: ReactNode; strong?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-3 py-1 text-sm">
      <span className="text-muted">{k}</span>
      <span className={`num text-right ${strong ? "font-semibold" : ""}`}>{v}</span>
    </div>
  );
}
