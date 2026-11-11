"use client";
import { useEffect, useState } from "react";
import { api, ApiError, type Order, type UserSettings, type Wallet } from "@/lib/api";
import { formatUnits, isLikelyMint, isValidSolInput, short, sol } from "@/lib/money";
import { Badge, Btn, ErrorBox, Panel, Row, Skeleton } from "./ui";

function useLoad<T>(path: string, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [n, setN] = useState(0);
  useEffect(() => {
    let live = true;
    setErr(null);
    api<T>("GET", path)
      .then((d) => live && setData(d))
      .catch((e) => live && setErr(e instanceof ApiError ? e.message : "Network error"));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, n, ...deps]);
  return { data, err, reload: () => setN((x) => x + 1) };
}

type Pos = { mint: string; qty_raw: string; cost_basis_lamports: string; realized_pnl_lamports: string; unrealized_pnl_lamports: string | null; price_source: string; fees_paid_lamports: string; buys: number; sells: number };
type Portfolio = { cost_basis_method: string; wallets: { wallet: Wallet; sol_lamports: string | null; positions: Pos[] }[] };

function signedSol(v: string | null) {
  if (v === null) return "unavailable";
  const b = BigInt(v);
  return `${b < 0n ? "-" : "+"}${sol(b < 0n ? -b : b)} SOL`;
}

export function PortfolioTab() {
  const { data, err } = useLoad<Portfolio>("/v1/portfolio");
  if (err) return <ErrorBox error={err} />;
  if (!data) return <Panel><Skeleton /></Panel>;
  return (
    <div className="space-y-4">
      {data.wallets.length === 0 && <Panel><p className="text-sm text-muted">No wallet yet. Create one in Settings.</p></Panel>}
      {data.wallets.map((w) => (
        <Panel key={w.wallet.id} title="Wallet" right={<Badge>{w.wallet.network}</Badge>}>
          <Row k="Address" v={<code className="text-xs">{short(w.wallet.pubkey, 6)}</code>} />
          <Row k="SOL" v={w.sol_lamports === null ? "unavailable" : `${sol(w.sol_lamports)} SOL`} strong />
          <p className="mt-3 text-xs text-muted">Cost basis: {data.cost_basis_method.replace("_", " ")} (fees included; rent tracked separately). Unrealized PnL uses an executable sell quote; shown as unavailable when no quote exists.</p>
          <div className="mt-3 space-y-3">
            {w.positions.length === 0 && <p className="text-sm text-muted">No positions.</p>}
            {w.positions.map((p) => (
              <div key={p.mint} className="rounded-xl border border-line bg-panel-2 p-3" data-testid="position">
                <Row k="Token" v={<code className="text-xs">{short(p.mint, 5)}</code>} />
                <Row k="Quantity (raw)" v={formatUnits(p.qty_raw, 0)} />
                <Row k="Cost basis" v={`${sol(p.cost_basis_lamports)} SOL`} />
                <Row k="Realized PnL" v={signedSol(p.realized_pnl_lamports)} />
                <Row k="Unrealized PnL" v={signedSol(p.unrealized_pnl_lamports)} />
                <Row k="Fees paid" v={`${sol(p.fees_paid_lamports, 9)} SOL`} />
              </div>
            ))}
          </div>
        </Panel>
      ))}
    </div>
  );
}

export function OrdersTab() {
  const { data, err, reload } = useLoad<{ orders: Order[] }>("/v1/orders");
  if (err) return <ErrorBox error={err} />;
  if (!data) return <Panel><Skeleton /></Panel>;
  return (
    <Panel title="Orders" right={<Btn kind="ghost" onClick={reload}>Refresh</Btn>}>
      {data.orders.length === 0 && <p className="text-sm text-muted">No orders yet.</p>}
      <ul className="divide-y divide-line">
        {data.orders.map((o) => (
          <li key={o.id} className="py-2 text-sm" data-testid="order-row">
            <div className="flex justify-between">
              <span className={o.intent.side === "buy" ? "text-buy" : "text-sell"}>{o.intent.side?.toUpperCase()} {short(o.intent.mint ?? "", 4)}</span>
              <Badge tone={o.state === "FINALIZED" ? "buy" : o.state === "REJECTED" || o.state === "FAILED_ONCHAIN" ? "sell" : "muted"}>{o.state}</Badge>
            </div>
            {o.signed && <code className="text-xs text-muted">{short(o.signed.signature, 8)}</code>}
            {o.reject_reason && o.state === "REJECTED" && <p className="text-xs text-muted">{o.reject_reason}</p>}
          </li>
        ))}
      </ul>
    </Panel>
  );
}

export function TrackTab() {
  const { data, err, reload } = useLoad<{ tracked: { address: string; label: string | null }[]; alerts: string }>("/v1/tracked-wallets");
  const [addr, setAddr] = useState("");
  const [e2, setE2] = useState<string | null>(null);
  const add = async () => {
    setE2(null);
    if (!isLikelyMint(addr)) return setE2("Invalid wallet address");
    try {
      await api("POST", "/v1/tracked-wallets", { address: addr.trim() });
      setAddr("");
      reload();
    } catch (e) {
      setE2(e instanceof ApiError ? e.message : "Network error");
    }
  };
  return (
    <div className="space-y-4">
      <Panel title="Wallet tracker">
        <p className="mb-2 text-xs text-warn">Watchlist only. Live activity alerts and copy trading are not implemented in this build.</p>
        <div className="flex gap-2">
          <label htmlFor="track" className="sr-only">Wallet address</label>
          <input id="track" value={addr} onChange={(e) => setAddr(e.target.value)} placeholder="Public wallet address" className="min-h-11 flex-1 rounded-xl border border-line bg-panel-2 px-3 text-sm outline-none focus:border-accent" />
          <Btn kind="primary" onClick={add}>Track</Btn>
        </div>
      </Panel>
      <ErrorBox error={err ?? e2} />
      <Panel title="Tracked">
        {!data ? <Skeleton /> : data.tracked.length === 0 ? <p className="text-sm text-muted">Nothing tracked.</p> : (
          <ul className="divide-y divide-line">
            {data.tracked.map((t) => (
              <li key={t.address} className="flex items-center justify-between py-2 text-sm">
                <code className="text-xs">{short(t.address, 6)}</code>
                <Btn kind="ghost" onClick={async () => { await api("DELETE", "/v1/tracked-wallets", { address: t.address }); reload(); }}>Remove</Btn>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

type SettingsResp = { settings: UserSettings; policy: Record<string, unknown> | null; available_senders: string[] };

export function SettingsTab({ wallets, onWallet, mode }: { wallets: Wallet[]; onWallet: () => void; mode: string }) {
  const { data, err, reload } = useLoad<SettingsResp>("/v1/settings", [wallets.length]);
  const [msg, setMsg] = useState<string | null>(null);
  const [e2, setE2] = useState<string | null>(null);
  const [presets, setPresets] = useState("");
  const [slip, setSlip] = useState("");
  useEffect(() => {
    if (data) {
      setPresets(data.settings.buy_presets.map((l) => sol(l, 9)).join(","));
      setSlip(String(data.settings.slippage));
    }
  }, [data]);
  const save = async (patch: Partial<UserSettings> | null, paused?: boolean) => {
    setMsg(null);
    setE2(null);
    try {
      const body: Record<string, unknown> = {};
      if (patch && data) body.settings = { ...data.settings, ...patch };
      if (paused !== undefined) body.paused = paused;
      await api("POST", "/v1/settings", body);
      setMsg("Saved");
      reload();
    } catch (e) {
      setE2(e instanceof ApiError ? e.message : "Network error");
    }
  };
  const savePresets = () => {
    const parts = presets.split(",").map((s) => s.trim());
    if (!parts.every(isValidSolInput)) return setE2("Presets must be SOL amounts like 0.05,0.1");
    // Exact conversion of decimal strings to lamports (no floats).
    const lam = parts.map((p) => {
      const [i, f = ""] = p.split(".");
      return (BigInt(i || "0") * 1_000_000_000n + BigInt((f + "000000000").slice(0, 9))).toString();
    });
    save({ buy_presets: lam });
  };
  const wallet = wallets[0];
  const testnet = mode === "mock" || mode === "localnet" || mode === "devnet";
  const paused = Boolean(data?.policy?.["paused"]);
  return (
    <div className="space-y-4">
      <Panel title="Wallet">
        {wallet ? (
          <>
            <Row k="Address" v={<code className="text-xs" data-testid="wallet-address">{short(wallet.pubkey, 6)}</code>} />
            <Row k="Custody" v="Managed by this service (private beta)" />
            <p className="mt-2 text-xs text-warn">Keys are envelope-encrypted at rest and decrypted in server memory to sign. Not hardware isolation. Export is disabled. Fund only small amounts.</p>
            {testnet && (
              <div className="mt-3">
                <Btn onClick={async () => { await api("POST", "/v1/dev/airdrop"); setMsg("Test SOL requested"); }}>Get test SOL</Btn>
              </div>
            )}
          </>
        ) : (
          <Btn kind="primary" onClick={async () => { try { await api("POST", "/v1/wallets"); onWallet(); } catch (e) { setE2(e instanceof ApiError ? e.message : "Network error"); } }}>
            Create test trading wallet
          </Btn>
        )}
      </Panel>
      <ErrorBox error={err ?? e2} />
      {msg && <p role="status" className="text-sm text-buy">{msg}</p>}
      {data && (
        <>
          <Panel title="Kill switch">
            <Row k="Trading" v={paused ? <Badge tone="sell">paused</Badge> : <Badge tone="buy">active</Badge>} />
            <div className="mt-2">
              <Btn kind={paused ? "buy" : "sell"} disabled={!wallet} onClick={() => save(null, !paused)}>{paused ? "Resume trading" : "Pause all trading"}</Btn>
            </div>
          </Panel>
          <Panel title="Presets & slippage">
            <label className="mb-1 block text-xs text-muted" htmlFor="presets">Buy presets (SOL, comma-separated)</label>
            <input id="presets" value={presets} onChange={(e) => setPresets(e.target.value)} className="mb-2 min-h-11 w-full rounded-xl border border-line bg-panel-2 px-3 text-sm outline-none focus:border-accent" />
            <Btn onClick={savePresets}>Save presets</Btn>
            <label className="mt-3 mb-1 block text-xs text-muted" htmlFor="slip">Slippage (bps)</label>
            <input id="slip" inputMode="numeric" value={slip} onChange={(e) => setSlip(e.target.value)} className="mb-2 min-h-11 w-full rounded-xl border border-line bg-panel-2 px-3 text-sm outline-none focus:border-accent" />
            <Btn onClick={() => { const n = Number.parseInt(slip, 10); if (!Number.isInteger(n) || n < 1 || n > 10000) return setE2("Slippage must be 1–10000 bps"); save({ slippage: n }); }}>Save slippage</Btn>
            <p className="mt-2 text-xs text-muted">Per-wallet hard limits always apply (max order {data.policy ? sol(String(data.policy["max_order"])) : "?"} SOL, max slippage {String(data.policy?.["max_slippage"] ?? "?")} bps, max tip {data.policy ? sol(String(data.policy["max_tip"]), 9) : "?"} SOL).</p>
          </Panel>
        </>
      )}
    </div>
  );
}

export function MoreTab() {
  const items = [
    ["Discover", "Token discovery / trending is not implemented (P2). No market data is fabricated."],
    ["Copy trading", "Not available in this build (P1, disabled by default)."],
    ["Referrals", "Not implemented (P2). No rewards program is running."],
  ];
  return (
    <div className="space-y-4">
      {items.map(([t, d]) => (
        <Panel key={t} title={t}>
          <p className="text-sm text-muted">{d}</p>
        </Panel>
      ))}
    </div>
  );
}
