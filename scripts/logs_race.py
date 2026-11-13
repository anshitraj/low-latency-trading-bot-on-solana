#!/usr/bin/env python3
"""Read-only: which provider reports the same Pump.fun transactions first.

logsSubscribe {mentions: [Pump program]} at `processed` on every provider at
once; for each signature seen by >= 2 providers, record each provider's delay
behind the first one. Prints hosts only (keys live in the URL path/query).
Usage: python3 logs_race.py SECONDS label=wss://... label=ws://... [label=udp:0.0.0.0:9900]
A `udp:` source is the tools/shred-observer feed ([64-byte sig][u64 slot]
records); it reports ALL transactions, so only signatures that a WebSocket
source also reported (Pump.fun) are compared.
"""
import asyncio, json, sys, time, urllib.parse
import websockets

PUMP = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58encode(b):
    n = int.from_bytes(b, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(b) - len(b.lstrip(bytes(1)))) + out


async def watch_udp(label, bind, seen, stop):
    host, port = bind.rsplit(":", 1)
    loop = asyncio.get_running_loop()
    count = [0]

    class P(asyncio.DatagramProtocol):
        def datagram_received(self, data, addr):
            t = time.monotonic_ns()
            for i in range(0, len(data) - 71, 72):
                sig = b58encode(data[i:i + 64])
                slot = int.from_bytes(data[i + 64:i + 72], "little")
                count[0] += 1
                seen.setdefault(sig, {}).setdefault(label, (t, slot))

    tr, _ = await loop.create_datagram_endpoint(P, local_addr=(host, int(port)))
    try:
        await asyncio.sleep(max(0.0, stop - time.monotonic()))
    finally:
        tr.close()
    return label, count[0]


async def watch(label, url, seen, stop):
    sub = {"jsonrpc": "2.0", "id": 1, "method": "logsSubscribe",
           "params": [{"mentions": [PUMP]}, {"commitment": "processed"}]}
    n = 0
    try:
        async with websockets.connect(url, max_size=None, ping_interval=20) as ws:
            await ws.send(json.dumps(sub))
            while time.monotonic() < stop:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=max(0.1, stop - time.monotonic()))
                except asyncio.TimeoutError:
                    break
                t = time.monotonic_ns()
                v = json.loads(msg)
                r = v.get("params", {}).get("result", {})
                sig = r.get("value", {}).get("signature")
                if sig:
                    n += 1
                    seen.setdefault(sig, {}).setdefault(label, (t, r.get("context", {}).get("slot")))
                elif "error" in v:
                    print(f"{label}: error {str(v['error'])[:120]}", file=sys.stderr)
    except Exception as e:
        print(f"{label}: {type(e).__name__} {str(e)[:120]}", file=sys.stderr)
    return label, n


def pct(v, q):
    v = sorted(v)
    return round(v[min(len(v) - 1, int(len(v) * q))], 2) if v else None


async def main():
    secs = float(sys.argv[1])
    provs = [a.split("=", 1) for a in sys.argv[2:]]
    seen = {}
    stop = time.monotonic() + secs
    counts = await asyncio.gather(*(
        watch_udp(l, u[4:], seen, stop) if u.startswith("udp:") else watch(l, u, seen, stop)
        for l, u in provs))
    ws_labels = {l for l, u in provs if not u.startswith("udp:")}
    # Compare only signatures a WebSocket (Pump.fun logs) source reported.
    seen = {k: v for k, v in seen.items() if ws_labels & v.keys()}
    labels = [l for l, _ in provs]
    lag = {l: [] for l in labels}
    first = {l: 0 for l in labels}
    missed = {l: 0 for l in labels}
    both = 0
    for sig, d in seen.items():
        if len(d) < 2:
            continue
        both += 1
        t0 = min(t for t, _ in d.values())
        for l in labels:
            if l in d:
                ms = (d[l][0] - t0) / 1e6
                lag[l].append(ms)
                if d[l][0] == t0:
                    first[l] += 1
            else:
                missed[l] += 1
    out = {"window_s": secs, "signatures_seen_by_2plus": both,
           "per_provider": {l: {"notifications": dict(counts)[l], "first": first[l], "missed": missed[l],
                                "lag_behind_first_ms": {"p50": pct(lag[l], .5), "p90": pct(lag[l], .9),
                                                        "p99": pct(lag[l], .99), "n": len(lag[l])}}
                            for l in labels}}
    # head to head for the first two labels
    if len(labels) >= 2:
        a, b = labels[0], labels[1]
        dd = [(d[b][0] - d[a][0]) / 1e6 for d in seen.values() if a in d and b in d]
        out["head_to_head"] = {"pair": f"{b} minus {a} (ms; negative = {b} earlier)", "n": len(dd),
                               "p10": pct(dd, .1), "p50": pct(dd, .5), "p90": pct(dd, .9),
                               f"{b}_earlier_share": round(sum(1 for x in dd if x < 0) / len(dd), 3) if dd else None}
    print(json.dumps(out, indent=1))

asyncio.run(main())
