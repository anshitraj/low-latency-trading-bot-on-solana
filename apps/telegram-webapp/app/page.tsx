"use client";
import { useCallback, useEffect, useState } from "react";
import { api, ApiError, setToken, type User, type UserSettings, type Wallet } from "@/lib/api";
import { Trade } from "@/components/Trade";
import { MoreTab, OrdersTab, PortfolioTab, SettingsTab, TrackTab } from "@/components/Tabs";
import { ErrorBox, Skeleton } from "@/components/ui";

type Tg = { WebApp?: { initData?: string; ready?: () => void; expand?: () => void } };
declare global {
  interface Window {
    Telegram?: Tg;
  }
}

const TABS = ["Trade", "Portfolio", "Orders", "Track", "Settings", "More"] as const;
type Tab = (typeof TABS)[number];

export default function Home() {
  const [phase, setPhase] = useState<"loading" | "no-telegram" | "error" | "ready">("loading");
  const [err, setErr] = useState<string | null>(null);
  const [user, setUser] = useState<User | null>(null);
  const [mode, setMode] = useState("");
  const [settings, setSettings] = useState<UserSettings | null>(null);
  const [wallets, setWallets] = useState<Wallet[]>([]);
  const [tab, setTab] = useState<Tab>("Trade");
  const [startMint, setStartMint] = useState<string | undefined>();

  const refreshMe = useCallback(async () => {
    const me = await api<{ user: User; settings: UserSettings; mode: string }>("GET", "/v1/me");
    setUser(me.user);
    setSettings(me.settings);
    setMode(me.mode);
    const w = await api<{ wallets: Wallet[] }>("GET", "/v1/wallets");
    setWallets(w.wallets);
  }, []);

  useEffect(() => {
    const wa = window.Telegram?.WebApp;
    wa?.ready?.();
    wa?.expand?.();
    // Only the signed initData string is sent; initDataUnsafe is never used for auth.
    const initData = wa?.initData;
    if (!initData) {
      setPhase("no-telegram");
      return;
    }
    (async () => {
      try {
        const r = await api<{ token: string; start_param: string | null }>("POST", "/v1/auth/telegram", { init_data: initData });
        setToken(r.token);
        if (r.start_param) setStartMint(r.start_param);
        await refreshMe();
        setPhase("ready");
      } catch (e) {
        setErr(e instanceof ApiError ? `${e.message} (${e.code})` : "Network error");
        setPhase("error");
      }
    })();
  }, [refreshMe]);

  if (phase === "loading")
    return (
      <main className="mx-auto max-w-lg p-4">
        <Skeleton lines={5} />
      </main>
    );
  if (phase === "no-telegram")
    return (
      <main className="mx-auto max-w-lg p-6 text-center">
        <h1 className="mb-2 text-lg font-semibold">Open inside Telegram</h1>
        <p className="text-sm text-muted">This Mini App authenticates with Telegram&apos;s signed launch data. Open it from the bot&apos;s menu button.</p>
      </main>
    );
  if (phase === "error")
    return (
      <main className="mx-auto max-w-lg p-6">
        <h1 className="mb-2 text-lg font-semibold">Could not sign in</h1>
        <ErrorBox error={err} />
        <p className="mt-2 text-sm text-muted">Close and reopen the Mini App from Telegram to get fresh launch data.</p>
      </main>
    );

  const test = mode !== "mainnet";
  return (
    <div className="mx-auto flex min-h-dvh max-w-lg flex-col">
      <header className="sticky top-0 z-10 border-b border-line bg-bg/95 px-4 py-3 backdrop-blur">
        <div className="flex items-center justify-between">
          <h1 className="text-base font-semibold">Trader</h1>
          <span className="text-xs text-muted">{user?.username ? `@${user.username}` : `id ${user?.telegram_id}`}</span>
        </div>
        <p data-testid="mode-banner" className={`mt-1 text-xs ${test ? "text-warn" : "text-sell"}`}>
          {mode === "mock" ? "MOCK MODE — simulated chain, not real funds or prices" : test ? `${mode.toUpperCase()} — test network, no real value` : "MAINNET — real funds"}
        </p>
      </header>
      <main className="flex-1 space-y-4 p-4 pb-28">
        {tab === "Trade" && <Trade settings={settings} initialMint={startMint} hasWallet={wallets.length > 0} />}
        {tab === "Portfolio" && <PortfolioTab />}
        {tab === "Orders" && <OrdersTab />}
        {tab === "Track" && <TrackTab />}
        {tab === "Settings" && <SettingsTab wallets={wallets} mode={mode} onWallet={refreshMe} />}
        {tab === "More" && <MoreTab />}
      </main>
      <nav aria-label="Sections" className="fixed inset-x-0 bottom-0 z-20 border-t border-line bg-panel/95 backdrop-blur">
        <ul className="mx-auto grid max-w-lg grid-cols-6">
          {TABS.map((t) => (
            <li key={t}>
              <button aria-current={tab === t ? "page" : undefined} onClick={() => setTab(t)} className={`min-h-14 w-full text-xs ${tab === t ? "text-accent" : "text-muted"}`}>
                {t}
              </button>
            </li>
          ))}
        </ul>
      </nav>
    </div>
  );
}
