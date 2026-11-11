"use client";
import { useCallback, useState } from "react";
import { api, ApiError, newIdempotencyKey, type Order, type UserSettings } from "@/lib/api";
import { isLikelyMint, isValidSolInput, short, sol } from "@/lib/money";
import { ConfirmSheet } from "./ConfirmSheet";
import { OrderTracker } from "./OrderTracker";
import { Badge, Btn, ErrorBox, Panel, Row, Skeleton } from "./ui";

type Risk = { flags: { code: string; level: "info" | "warn" | "block"; message: string }[]; top10_holder_bps: number | null; source: string };
type TokenInfo = { mint: string; supported: boolean; adapter: string | null; market: Record<string, unknown> | null; mint_info: Record<string, unknown> | null; risk: Risk | null; unsupported_reason: string | null; disclaimer: string };

export function Trade({ settings, initialMint, hasWallet }: { settings: UserSettings | null; initialMint?: string; hasWallet: boolean }) {
  const [mint, setMint] = useState(initialMint ?? "");
  const [info, setInfo] = useState<TokenInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [custom, setCustom] = useState("");
  const [pending, setPending] = useState<Order | null>(null);
  const [busy, setBusy] = useState(false);
  const [tracking, setTracking] = useState<string | null>(null);

  const lookup = useCallback(async (m: string) => {
    setErr(null);
    setInfo(null);
    if (!isLikelyMint(m)) {
      setErr("That is not a valid Solana mint address (base58, 32–44 chars).");
      return;
    }
    setLoading(true);
    try {
      setInfo(await api<TokenInfo>("GET", `/v1/tokens/${m.trim()}`));
    } catch (e) {
      setErr(e instanceof ApiError ? e.message : "Network error");
    } finally {
      setLoading(false);
    }
  }, []);

  const order = async (body: { side: "buy" | "sell"; amount_sol?: string; percent?: number }) => {
    if (!info) return;
    setErr(null);
    setBusy(true);
    try {
      const r = await api<{ order: Order }>("POST", "/v1/orders", { mint: info.mint, ...body }, { "idempotency-key": newIdempotencyKey() });
      if (r.order.state === "AWAITING_AUTH") setPending(r.order);
      else if (r.order.state === "REJECTED") setErr(`Rejected: ${r.order.reject_reason ?? "unknown"}`);
      else setTracking(r.order.id);
    } catch (e) {
      setErr(e instanceof ApiError ? e.message : "Network error");
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    if (!pending) return;
    setBusy(true);
    try {
      await api("POST", `/v1/orders/${pending.id}/approve`);
      setTracking(pending.id);
      setPending(null);
    } catch (e) {
      setErr(e instanceof ApiError ? e.message : "Network error");
    } finally {
      setBusy(false);
    }
  };

  const cancel = async () => {
    if (!pending) return;
    try {
      await api("POST", `/v1/orders/${pending.id}/cancel`);
    } catch {
      /* ignore */
    }
    setPending(null);
  };

  const liq = info?.market?.["liquidity_sol_lamports"] as string | undefined;
  return (
    <div className="space-y-4">
      <Panel title="Token">
        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            lookup(mint);
          }}
        >
          <label htmlFor="mint" className="sr-only">
            Token mint address
          </label>
          <input id="mint" value={mint} onChange={(e) => setMint(e.target.value)} placeholder="Paste token mint address" autoComplete="off" spellCheck={false} className="min-h-11 flex-1 rounded-xl border border-line bg-panel-2 px-3 text-sm outline-none placeholder:text-muted focus:border-accent" />
          <Btn type="submit" kind="primary">
            Load
          </Btn>
        </form>
        <p className="mt-2 text-xs text-muted">Tokens are identified by mint address only; names and tickers are not trusted.</p>
      </Panel>
      <ErrorBox error={err} />
      {loading && (
        <Panel>
          <Skeleton lines={4} />
        </Panel>
      )}
      {info && (
        <Panel title="Market" right={info.supported ? <Badge tone="accent">{info.adapter}</Badge> : <Badge tone="sell">unsupported</Badge>}>
          <Row k="Mint" v={<code className="text-xs">{short(info.mint, 6)}</code>} />
          {info.supported ? (
            <>
              <Row k="Liquidity" v={liq ? `${sol(liq, 3)} SOL` : "unknown"} />
              <Row k="Source" v={String(info.market?.["source"] ?? "unknown")} />
            </>
          ) : (
            <p className="text-sm text-sell">{info.unsupported_reason}</p>
          )}
          <div className="mt-3 space-y-1" data-testid="risk-flags">
            {info.risk ? (
              info.risk.flags.map((f) => (
                <p key={f.code + f.message} className={`text-xs ${f.level === "block" ? "text-sell" : f.level === "warn" ? "text-warn" : "text-muted"}`}>
                  {f.level === "block" ? "⛔" : f.level === "warn" ? "⚠" : "ℹ"} {f.message}
                </p>
              ))
            ) : (
              <p className="text-xs text-muted">Mint risk data: Unknown (not available)</p>
            )}
            <p className="text-xs text-muted">Top-10 holders: {info.risk?.top10_holder_bps != null ? `${(info.risk.top10_holder_bps / 100).toFixed(0)}% (includes curve/pool)` : "Unknown"}</p>
            <p className="text-xs italic text-muted">{info.disclaimer}</p>
          </div>
        </Panel>
      )}
      {info?.supported && (
        <Panel title="Quick trade">
          {!hasWallet && <p className="mb-2 text-sm text-warn">Create a wallet in Settings first.</p>}
          <div className="grid grid-cols-3 gap-2">
            {(settings?.buy_presets ?? []).map((l) => (
              <Btn key={l} kind="buy" disabled={busy || !hasWallet} onClick={() => order({ side: "buy", amount_sol: sol(l, 9).replace(/,/g, "") })}>
                Buy {sol(l, 9)}
              </Btn>
            ))}
          </div>
          <div className="mt-2 flex gap-2">
            <label htmlFor="custom" className="sr-only">
              Custom SOL amount
            </label>
            <input id="custom" inputMode="decimal" value={custom} onChange={(e) => setCustom(e.target.value)} placeholder="SOL amount" className="min-h-11 flex-1 rounded-xl border border-line bg-panel-2 px-3 text-sm outline-none focus:border-accent" />
            <Btn kind="buy" disabled={busy || !hasWallet || !isValidSolInput(custom)} onClick={() => order({ side: "buy", amount_sol: custom.trim() })}>
              Buy
            </Btn>
          </div>
          <div className="mt-3 grid grid-cols-4 gap-2">
            {(settings?.sell_presets_bps ?? []).map((b) => (
              <Btn key={b} kind="sell" disabled={busy || !hasWallet} onClick={() => order({ side: "sell", percent: b / 100 })}>
                Sell {b / 100}%
              </Btn>
            ))}
          </div>
        </Panel>
      )}
      {tracking && <OrderTracker orderId={tracking} />}
      {pending && <ConfirmSheet order={pending} busy={busy} onConfirm={confirm} onCancel={cancel} />}
    </div>
  );
}
