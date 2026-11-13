#!/usr/bin/env python3
"""Generate the animated README images (light + dark variants) in docs/assets/.

Numbers come from docs/SPEED.md / docs/BENCHMARKS.md (mainnet, 2026-11-12/13).
Animations are SMIL (play inside GitHub's README <img>/<picture>).
Run: python scripts/make_readme_svgs.py
"""
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "docs" / "assets"
FONT = "-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif"

# Reference palette (validated): one series hue + neutral inks per surface.
THEMES = {
    "light": dict(surface="#fcfcfb", card="#f1f0ec", border="#d9d8d2", text="#0b0b0b",
                  text2="#52514e", muted="#8a8984", accent="#2a78d6", grid="#e4e3dd"),
    "dark": dict(surface="#1a1a19", card="#262624", border="#3a3a37", text="#f0efec",
                 text2="#c3c2b7", muted="#8f8e86", accent="#3987e5", grid="#2e2e2c"),
}


def speed_chart(t):
    """Horizontal bars: median send -> trade first seen, per setup (single series)."""
    rows = [
        ("Helius WebSocket notification", "baseline", 180.6),
        ("Own node + our shred observer", "BENCHMARKS 2n", 136.7),
        ("+ watch list (first shred w/ our sig)", "BENCHMARKS 2p", 99.3),
        ("+ GetBlock pro shred feed", "BENCHMARKS 2r", 54.2),
    ]
    W, H = 880, 330
    x0, x1 = 300, 820          # plot area
    scale = (x1 - x0) / 200.0  # 0..200 ms
    top, bh, gap = 92, 34, 22
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="100%" role="img" '
         f'aria-label="Median time from send to trade seen in the block: 181, 137, 99 and 54 milliseconds">',
         f'<rect width="{W}" height="{H}" rx="14" fill="{t["surface"]}" stroke="{t["border"]}"/>',
         f'<text x="28" y="40" font-family="{FONT}" font-size="20" font-weight="700" fill="{t["text"]}">'
         'Send → our trade seen in the block (median, mainnet)</text>',
         f'<text x="28" y="64" font-family="{FONT}" font-size="13" fill="{t["text2"]}">'
         '162 verified Pump.fun trades · lower is faster · best single trade 13.0 ms</text>']
    # grid + axis ticks (recessive)
    for ms in (0, 50, 100, 150, 200):
        x = x0 + ms * scale
        s.append(f'<line x1="{x:.1f}" y1="{top-10}" x2="{x:.1f}" y2="{top + 4*(bh+gap) - gap + 6}" '
                 f'stroke="{t["grid"]}" stroke-width="1"/>')
        s.append(f'<text x="{x:.1f}" y="{top + 4*(bh+gap) - gap + 24}" text-anchor="middle" '
                 f'font-family="{FONT}" font-size="12" fill="{t["muted"]}">{ms} ms</text>')
    # 40 ms target reference line
    xt = x0 + 40 * scale
    s.append(f'<line x1="{xt:.1f}" y1="{top-16}" x2="{xt:.1f}" y2="{top + 4*(bh+gap) - gap + 6}" '
             f'stroke="{t["text2"]}" stroke-width="1.5" stroke-dasharray="4 4"/>')
    s.append(f'<text x="{xt+6:.1f}" y="{top-6}" font-family="{FONT}" font-size="12" '
             f'fill="{t["text2"]}">40 ms target</text>')
    for i, (label, src, ms) in enumerate(rows):
        y = top + i * (bh + gap)
        w = ms * scale
        begin = f"{0.3 + i*0.35:.2f}s"
        s.append(f'<text x="{x0-14}" y="{y + bh/2 - 2:.1f}" text-anchor="end" font-family="{FONT}" '
                 f'font-size="14" font-weight="600" fill="{t["text"]}">{label}</text>')
        s.append(f'<text x="{x0-14}" y="{y + bh/2 + 14:.1f}" text-anchor="end" font-family="{FONT}" '
                 f'font-size="11" fill="{t["muted"]}">{src}</text>')
        # bar: square at baseline, 4px rounded data-end (clip a rounded rect)
        s.append(f'<clipPath id="c{i}"><rect x="{x0}" y="{y}" width="{w:.1f}" height="{bh}"/></clipPath>')
        s.append(f'<g clip-path="url(#c{i})"><rect x="{x0-4}" y="{y}" width="0" height="{bh}" rx="4" '
                 f'fill="{t["accent"]}"><animate attributeName="width" from="0" to="{w+4:.1f}" '
                 f'begin="{begin}" dur="0.9s" fill="freeze" calcMode="spline" keySplines="0.2 0.8 0.2 1" '
                 f'keyTimes="0;1"/></rect></g>')
        s.append(f'<text x="{x0 + w + 10:.1f}" y="{y + bh/2 + 5:.1f}" font-family="{FONT}" font-size="15" '
                 f'font-weight="700" fill="{t["text"]}" opacity="0">{ms:.0f} ms'
                 f'<animate attributeName="opacity" from="0" to="1" begin="{float(begin[:-1])+0.7:.2f}s" '
                 f'dur="0.4s" fill="freeze"/></text>')
    s.append('</svg>')
    return "\n".join(s)


def box(t, x, y, w, h, title, sub, accent=False):
    stroke = t["accent"] if accent else t["border"]
    sw = 2 if accent else 1
    return (f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{t["card"]}" stroke="{stroke}" '
            f'stroke-width="{sw}"/>'
            f'<text x="{x + w/2}" y="{y + h/2 - 4}" text-anchor="middle" font-family="{FONT}" font-size="14" '
            f'font-weight="700" fill="{t["text"]}">{title}</text>'
            f'<text x="{x + w/2}" y="{y + h/2 + 15}" text-anchor="middle" font-family="{FONT}" font-size="12" '
            f'fill="{t["text2"]}">{sub}</text>')


def pipeline(t):
    """Animated flow: order -> engine -> 3 routes (same bytes) -> leader -> shreds -> observer."""
    W, H = 880, 300
    cyc = 5.0  # seconds per loop
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="100%" role="img" '
         f'aria-label="Trade pipeline: engine signs in 0.2 ms, the same signed bytes go to direct TPU, '
         f'Helius staked and RPC in about 2 ms, the leader packs the block, and our shred observer sees the '
         f'trade about 50 ms after sending">',
         f'<rect width="{W}" height="{H}" rx="14" fill="{t["surface"]}" stroke="{t["border"]}"/>',
         f'<text x="28" y="38" font-family="{FONT}" font-size="20" font-weight="700" fill="{t["text"]}">'
         'How one trade travels</text>',
         f'<text x="28" y="60" font-family="{FONT}" font-size="13" fill="{t["text2"]}">'
         'sign once · same signed bytes on every route · read it straight out of the block data</text>']
    # layout
    eng = (40, 120, 150, 64)
    routes = [(270, 88, 150, 46, "Direct TPU", "QUIC → leaders"),
              (270, 150, 150, 46, "Helius staked", "SWQoS · ~$0.0004"),
              (270, 212, 150, 46, "RPC", "backup")]
    lead = (500, 120, 150, 64)
    obs = (700, 120, 150, 64)
    paths = []
    ex, ey = eng[0] + eng[2], eng[1] + eng[3] / 2
    lx, ly = lead[0], lead[1] + lead[3] / 2
    for (rx, ry, rw, rh, *_rest) in routes:
        cy = ry + rh / 2
        p_in = f"M{ex},{ey} C{ex+40},{ey} {rx-40},{cy} {rx},{cy}"
        p_out = f"M{rx+rw},{cy} C{rx+rw+40},{cy} {lx-40},{ly} {lx},{ly}"
        paths += [p_in, p_out]
        for p in (p_in, p_out):
            s.append(f'<path d="{p}" fill="none" stroke="{t["border"]}" stroke-width="2"/>')
    p_ls = f"M{lead[0]+lead[2]},{ly} L{obs[0]},{ly}"
    s.append(f'<path d="{p_ls}" fill="none" stroke="{t["border"]}" stroke-width="2"/>')
    # boxes
    s.append(box(t, *eng, "Rust engine", "quote · guard · sign 0.2 ms", accent=True))
    for (rx, ry, rw, rh, title, sub) in routes:
        s.append(box(t, rx, ry, rw, rh, title, sub))
    s.append(box(t, *lead, "Block leader", "packs the trade"))
    s.append(box(t, *obs, "Shred observer", "sees it ~50 ms", accent=True))
    # time labels
    s.append(f'<text x="{(ex+270)/2:.0f}" y="{ey-70}" text-anchor="middle" font-family="{FONT}" font-size="12" '
             f'font-weight="600" fill="{t["text2"]}">~2 ms on the wire</text>')
    s.append(f'<text x="{(lead[0]+lead[2]+obs[0])/2:.0f}" y="{ly-12}" text-anchor="middle" font-family="{FONT}" '
             f'font-size="12" font-weight="600" fill="{t["text2"]}">shreds</text>')
    # moving dots (one per route, same start = same signed bytes)
    for i in range(3):
        p_in, p_out = paths[2 * i], paths[2 * i + 1]
        full = p_in + " " + p_out.replace("M", "L", 1) + " " + p_ls.replace("M", "L", 1)
        s.append(f'<circle r="6" fill="{t["accent"]}" opacity="0">'
                 f'<animateMotion dur="{cyc}s" repeatCount="indefinite" path="{full}" '
                 f'keyPoints="0;1;1" keyTimes="0;0.7;1" calcMode="linear"/>'
                 f'<animate attributeName="opacity" values="0;1;1;0;0" keyTimes="0;0.05;0.68;0.74;1" '
                 f'dur="{cyc}s" repeatCount="indefinite"/></circle>')
    # observer "seen" pulse
    ox, oy = obs[0] + obs[2] / 2, ly
    s.append(f'<circle cx="{ox}" cy="{oy}" r="40" fill="none" stroke="{t["accent"]}" stroke-width="2" opacity="0">'
             f'<animate attributeName="r" values="34;34;56" keyTimes="0;0.7;1" dur="{cyc}s" repeatCount="indefinite"/>'
             f'<animate attributeName="opacity" values="0;0;0.9;0" keyTimes="0;0.69;0.72;1" dur="{cyc}s" '
             f'repeatCount="indefinite"/></circle>')
    s.append(f'<text x="{W-28}" y="{H-18}" text-anchor="end" font-family="{FONT}" font-size="11" '
             f'fill="{t["muted"]}">timings: mainnet medians, docs/SPEED.md</text>')
    s.append('</svg>')
    return "\n".join(s)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for name, t in THEMES.items():
        (OUT / f"speed-{name}.svg").write_text(speed_chart(t), encoding="utf-8")
        (OUT / f"pipeline-{name}.svg").write_text(pipeline(t), encoding="utf-8")
    print("wrote", sorted(p.name for p in OUT.glob("*.svg")))


if __name__ == "__main__":
    main()
