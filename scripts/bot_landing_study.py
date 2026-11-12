#!/usr/bin/env python3
"""Read-only on-chain comparison of trading platforms' transactions (2026-11-12).

For each platform, sample its most recent transactions through its public fee
wallets (DefiLlama dimension-adapters fees/*.ts), then measure from chain data:

- failure rate (getSignaturesForAddress err != null; failed txs still list the wallet)
- BLOCKHASH AGE = landing slot - slot of the block whose hash the tx used as
  recentBlockhash. The only "when was it built" stamp on chain. It is an UPPER
  BOUND on build->land: it also contains how stale the platform's cached
  blockhash was at signing. Durable-nonce txs are excluded (their hash is a nonce).
- compute-unit price, Jito tip, fee, CU used.

Our own canary transactions (benchmarks/results/mainnet-canary-send-*.json) go
through the same code, so the comparison is like for like.

Sends nothing. Providers: HELIUS_API_KEY, ALCHEMY_API_KEY, RPC_URL from the
environment; only hosts are printed. Usage:
    set -a; . ./.env; set +a; python scripts/bot_landing_study.py [--sample 120]
"""
import argparse
import glob
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

PLATFORMS = {
    # DefiLlama fees/gmgnai.ts SOLANA_FEE_WALLETS ("verified on-chain 2026-06").
    "GMGN": [
        "BB5dnY55FXS1e1NXqZDwCzgdYJdMCj3B92PU6Q5Fb6DT", "7sHXjs1j7sDJGVSMSPjD1b4v3FD6uRSvRWfhRdfv5BiA",
        "HeZVpHj9jLwTVtMMbzQRf6mLtFPkWNSg11o68qrbUBa3", "ByRRgnZenY6W2sddo1VJzX9o4sMU4gPDUkcmgrpGBxRy",
        "DXfkEGoo6WFsdL7x6gLZ7r6Hw2S6HrtrAQVPWYx2A1s9", "3t9EKmRiAUcQUYzTZpNojzeGP1KBAVEEbDNmy6wECQpK",
        "DymeoWc5WLNiQBaoLuxrxDnDRvLgGZ1QGsEoCAM7Jsrx", "dBhdrmwBkRa66XxBuAK4WZeZnsZ6bHeHCCLXa3a8bTJ",
        "6TxjC5wJzuuZgTtnTMipwwULEbMPx5JPW3QwWkdTGnrn",
    ],
    # DefiLlama fees/bullx.ts (address used since 2024-11-16).
    "BullX": ["9RYJ3qr5eU5xAooqVcbmdeusjcViL5Nkiq7Gske3tiKq"],
    # DefiLlama fees/axiom.ts feeWallets.
    "Axiom": [
        "7LCZckF6XXGQ1hDY6HFXBKWAtiUgL9QY5vj1C4Bn1Qjj", "4V65jvcDG9DSQioUVqVPiUcUY9v6sb6HKtMnsxSKEz5S",
        "CeA3sPZfWWToFEBmw5n1Y93tnV66Vmp8LacLzsVprgxZ", "AaG6of1gbj1pbDumvbSiTuJhRCRkkUNaWVxijSbWvTJW",
        "7oi1L8U9MRu5zDz5syFahsiLUric47LzvJBQX6r827ws", "9kPrgLggBJ69tx1czYAbp7fezuUmL337BsqQTKETUEhP",
        "DKyUs1xXMDy8Z11zNsLnUg3dy9HZf6hYZidB6WodcaGy", "4FobGn5ZWYquoJkxMzh2VUAWvV36xMgxQ3M7uG1pGGhd",
        "76sxKrPtgoJHDJvxwFHqb3cAXWfRHFLe3VpKcLCAHSEf", "H2cDR3EkJjtTKDQKk8SJS48du9mhsdzQhy8xJx5UMqQK",
        "8m5GkL7nVy95G4YVUbs79z873oVKqg2afgKRmqxsiiRm", "4kuG6NsAFJNwqEkac8GFDMMheCGKUPEbaRVHHyFHSwWz",
        "8vFGAKdwpn4hk7kc1cBgfWZzpyW3MEMDATDzVZhddeQb", "86Vh4XGLW2b6nvWbRyDs4ScgMXbuvRCHT7WbUT3RFxKG",
        "DZfEurFKFtSbdWZsKSDTqpqsQgvXxmESpvRtXkAdgLwM", "5L2QKqDn5ukJSWGyqR4RPvFvwnBabKWqAqMzH4heaQNB",
        "DYVeNgXGLAhZdeLMMYnCw1nPnMxkBN7fJnNpHmizTrrF", "Hbj6XdxX6eV4nfbYTseysibp4zZJtVRRPn2J3BhGRuK9",
        "846ah7iBSu9ApuCyEhA5xpnjHHX7d4QJKetWLbwzmJZ8", "5BqYhuD4q1YD3DMAYkc1FeTu9vqQVYYdfBAmkZjamyZg",
    ],
    # DefiLlama fees/photon.ts.
    "Photon": ["AVUCZyuT35YSuj4RH7fwiyPu82Djn2Hfg7y2ND2XcnZH"],
}
# Jito getTipAccounts (Frankfurt block engine, 2026-11-12).
JITO_TIP = {
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT", "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5", "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt", "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL", "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
}
SYSTEM = "11111111111111111111111111111111"
COMPUTE_BUDGET = "ComputeBudget111111111111111111111111111111"
SLOT_MS = 210  # measured on mainnet 2026-11-12 (BENCHMARKS.md 2g)
MAX_AGE = 200

B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58decode(s):
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    raw = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    return b"\x00" * (len(s) - len(s.lstrip("1"))) + raw


class Provider:
    def __init__(self, url, rps):
        self.url, self.host = url, urllib.parse.urlparse(url).hostname
        self.gap, self.next, self.cool_until = 1.0 / rps, 0.0, 0.0
        self.lock = threading.Lock()
        self.calls = self.errors = 0

    def wait(self):
        with self.lock:
            now = time.monotonic()
            t = max(now, self.next, self.cool_until)
            self.next = t + self.gap
        if t > now:
            time.sleep(t - now)


class Rpc:
    def __init__(self, providers):
        self.p = providers
        self.i = 0
        self.lock = threading.Lock()

    def call(self, method, params, tries=8):
        last = None
        for k in range(tries):
            with self.lock:
                p = self.p[self.i % len(self.p)]
                self.i += 1
            p.wait()
            body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
            req = urllib.request.Request(p.url, body, {"content-type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=20) as r:
                    v = json.loads(r.read())
                p.calls += 1
                if "error" in v:
                    code = v["error"].get("code")
                    if code in (-32007, -32009, -32004):  # skipped / missing slot
                        return None
                    if code == 429 or code == -32429:
                        raise urllib.error.HTTPError(p.url, 429, "rate", None, None)
                    raise RuntimeError(f"{method}: {v['error'].get('message', '')[:120]}")
                return v["result"]
            except urllib.error.HTTPError as e:
                p.errors += 1
                last = f"{p.host} http {e.code}"
                p.cool_until = time.monotonic() + min(0.25 * 2 ** k, 4)
            except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
                p.errors += 1
                last = f"{p.host} {type(e).__name__}"
        raise RuntimeError(f"{method} failed: {last}")


def providers_from_env():
    out = []
    if os.environ.get("HELIUS_API_KEY"):
        out.append(Provider(f"https://mainnet.helius-rpc.com/?api-key={os.environ['HELIUS_API_KEY'].strip()}", 8))
    if os.environ.get("ALCHEMY_API_KEY"):
        out.append(Provider(f"https://solana-mainnet.g.alchemy.com/v2/{os.environ['ALCHEMY_API_KEY'].strip()}", 5))
    u = os.environ.get("RPC_URL", "").strip()
    if u.startswith("http") and all(urllib.parse.urlparse(u).hostname != p.host for p in out):
        out.append(Provider(u, 8))
    return out


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, int(-(-len(v) * q // 1)) - 1))] if v else None


def summary(v):
    if not v:
        return {"n": 0}
    return {"n": len(v), "min": min(v), "p50": pct(v, 0.5), "p90": pct(v, 0.9), "max": max(v),
            "mean": round(statistics.mean(v), 2)}


def decode_tx(sig, platform, tx):
    msg = tx["transaction"]["message"]
    meta = tx.get("meta") or {}
    keys = list(msg["accountKeys"])
    la = meta.get("loadedAddresses") or {}
    keys_all = keys + la.get("writable", []) + la.get("readonly", [])
    cu_price, cu_limit, nonce = 0, None, False
    for j, ix in enumerate(msg["instructions"]):
        prog = keys[ix["programIdIndex"]]
        data = b58decode(ix["data"]) if ix.get("data") else b""
        if prog == COMPUTE_BUDGET and data:
            if data[0] == 3 and len(data) >= 9:
                cu_price = int.from_bytes(data[1:9], "little")
            elif data[0] == 2 and len(data) >= 5:
                cu_limit = int.from_bytes(data[1:5], "little")
        if j == 0 and prog == SYSTEM and len(data) >= 4 and int.from_bytes(data[:4], "little") == 4:
            nonce = True
    pre, post = meta.get("preBalances", []), meta.get("postBalances", [])
    tip = sum(max(0, post[i] - pre[i]) for i, k in enumerate(keys_all) if k in JITO_TIP and i < len(post))
    cfg = msg.get("transactionConfig") or {}
    if cfg.get("computeUnitLimit"):
        cu_limit = cfg["computeUnitLimit"]
    signers = msg["header"]["numRequiredSignatures"]
    fee = meta.get("fee")
    return {
        "platform": platform, "signature": sig, "slot": tx["slot"], "block_time": tx.get("blockTime"),
        "version": tx.get("version"),
        # Version-agnostic: v0 sets a per-CU price, v1 a total priorityFee in lamports.
        "priority_lamports": (fee - 5000 * signers) if fee is not None else None,
        "recent_blockhash": msg["recentBlockhash"], "durable_nonce": nonce,
        "err": meta.get("err") is not None, "fee": meta.get("fee"),
        "cu_used": meta.get("computeUnitsConsumed"), "cu_price": cu_price, "cu_limit": cu_limit,
        "jito_tip": tip, "signers": signers,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample", type=int, default=120, help="transactions per platform")
    ap.add_argument("--sigs-per-wallet", type=int, default=200)
    ap.add_argument("--report", default=None)
    a = ap.parse_args()
    prov = providers_from_env()
    if not prov:
        sys.exit("set HELIUS_API_KEY and/or ALCHEMY_API_KEY (or RPC_URL)")
    print("providers:", [p.host for p in prov], file=sys.stderr)
    rpc = Rpc(prov)
    pool = ThreadPoolExecutor(max_workers=12)
    now_slot = rpc.call("getSlot", [{"commitment": "confirmed"}])

    # 1. recent signatures per platform
    listed, chosen = {}, {}
    for name, wallets in PLATFORMS.items():
        def sigs_of(w):
            try:
                return rpc.call("getSignaturesForAddress",
                                [w, {"limit": a.sigs_per_wallet, "commitment": "confirmed"}]) or []
            except RuntimeError as e:
                print(f"  {name} {w[:6]}: {e}", file=sys.stderr)
                return []
        res = list(pool.map(sigs_of, wallets))
        seen = {}
        for lst in res:
            for s in lst:
                seen.setdefault(s["signature"], s)
        rows = sorted(seen.values(), key=lambda s: -s["slot"])
        listed[name] = {
            "listed": len(rows), "failed": sum(1 for s in rows if s.get("err") is not None),
            "newest_slot_behind_now": (now_slot - rows[0]["slot"]) if rows else None,
            "oldest_slot_behind_now": (now_slot - rows[-1]["slot"]) if rows else None,
        }
        chosen[name] = [s["signature"] for s in rows[: a.sample]]
        print(f"{name}: {len(rows)} signatures listed", file=sys.stderr)
    ours = []
    for f in sorted(glob.glob("benchmarks/results/mainnet-canary-send-*.json")):
        try:
            d = json.load(open(f, encoding="utf-8"))
        except Exception:
            continue
        for r in d.get("rows", d.get("results", [])):
            if isinstance(r, dict) and r.get("signature"):
                ours.append(r["signature"])
    chosen["ours (canary)"] = sorted(set(ours))

    # 2. transactions
    def get(item):
        name, sig = item
        try:
            tx = rpc.call("getTransaction", [sig, {"encoding": "json", "maxSupportedTransactionVersion": 1,
                                               "commitment": "confirmed"}])
        except RuntimeError as e:
            print(f"  getTransaction {sig[:8]}: {e}", file=sys.stderr)
            return None
        try:
            tx = tx and decode_tx(sig, name, tx)
        except Exception as e:  # never let one odd tx abort the study
            print(f"  decode {sig[:8]}: {e}", file=sys.stderr)
            return None
        return tx or None
    items = [(n, s) for n, sigs in chosen.items() for s in sigs]
    print(f"fetching {len(items)} transactions ...", file=sys.stderr)
    txs = [t for t in pool.map(get, items) if t]

    # 3. blockhash age: walk back from the landing slot, one slot per round for
    # every unresolved tx at once (shared cache: txs in the same window share slots).
    cache = {}
    todo = [t for t in txs if not t["durable_nonce"]]
    for t in todo:
        t["blockhash_age_slots"] = None
    walking = lambda t: t["blockhash_age_slots"] is None and not t.get("age_gap")

    def block(s):
        try:
            return rpc.call("getBlock", [s, {"transactionDetails": "none", "rewards": False,
                                             "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}])
        except RuntimeError:
            return "retry"
    for k in range(1, MAX_AGE + 1):
        need = sorted({t["slot"] - k for t in todo if walking(t)} - cache.keys())
        for _ in range(3):  # failed fetches are retried, never skipped
            for s, b in zip(need, pool.map(block, need)):
                if b != "retry":
                    cache[s] = b["blockhash"] if b else None
            need = [s for s in need if s not in cache]
            if not need:
                break
        for t in todo:
            if not walking(t):
                continue
            s = t["slot"] - k
            if s not in cache:
                t["age_gap"] = True  # a block we could not read: unresolved, not guessed
            elif cache[s] == t["recent_blockhash"]:
                t["blockhash_age_slots"] = k
        if k % 20 == 0:
            left = sum(1 for t in todo if walking(t))
            print(f"  age walk {k} slots back, {left} unresolved, {len(cache)} blocks", file=sys.stderr)
        if not any(walking(t) for t in todo):
            break

    # 4. summary
    out = {}
    for name in chosen:
        rows = [t for t in txs if t["platform"] == name]
        ages = [t["blockhash_age_slots"] for t in rows if t.get("blockhash_age_slots") is not None]
        ok = [t for t in rows if not t["err"]]
        out[name] = {
            **listed.get(name, {}),
            "sampled": len(rows),
            "sample_failed": sum(1 for t in rows if t["err"]),
            "durable_nonce_share": round(sum(t["durable_nonce"] for t in rows) / len(rows), 3) if rows else None,
            "blockhash_age_slots": summary(ages),
            "blockhash_age_ms_approx": {k: (v * SLOT_MS if isinstance(v, (int, float)) and k != "n" else v)
                                        for k, v in summary(ages).items()},
            "age_unresolved": sum(1 for t in rows if not t["durable_nonce"] and t.get("blockhash_age_slots") is None),
            "share_age_le_2_slots": round(sum(1 for x in ages if x <= 2) / len(ages), 3) if ages else None,
            "jito_tip_share": round(sum(1 for t in ok if t["jito_tip"] > 0) / len(ok), 3) if ok else None,
            "jito_tip_lamports": summary([t["jito_tip"] for t in ok if t["jito_tip"] > 0]),
            "versions": {str(v): sum(1 for t in rows if t["version"] == v) for v in {t["version"] for t in rows}},
            "priority_fee_lamports": summary([t["priority_lamports"] for t in ok if t["priority_lamports"] is not None]),
            "fee_lamports": summary([t["fee"] for t in ok if t["fee"] is not None]),
        }
    report = {
        "kind": "bot_landing_study",
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "now_slot": now_slot,
        "note": "read-only. blockhash age = landing slot - slot of recentBlockhash's block: an upper bound on "
                "build->land that includes blockhash-cache staleness; ms = slots x 210.",
        "providers": [{"host": p.host, "calls": p.calls, "errors": p.errors} for p in prov],
        "summary": out,
        "transactions": txs,
    }
    path = a.report or time.strftime("benchmarks/results/bot-landing-study-%Y%m%dT%H%M%SZ.json", time.gmtime())
    with open(path, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=1)
    print(json.dumps({"summary": out}, indent=1))
    print(json.dumps({"report": path}))


if __name__ == "__main__":
    main()
