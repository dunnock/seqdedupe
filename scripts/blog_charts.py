#!/usr/bin/env python3
"""Charts for docs/blog.md (static SVG, no dependencies).

usage: scripts/blog_charts.py [bench-results dir] [out dir]

Writes <out>/blog-watermark.svg: time per register() call (p50 / p99 /
p99.9) of the first `Latest` design, with a plain load of the watermark,
under saturated arbitration (Dups, 1/2/4 threads, the `load` experiment's
`plain` variant in epyc-9845-experiments.csv) and in the paced live feed
(2 and 4 streams at 50k msg/s, target LatestPlain in latency-epyc-9845.csv).
"""
import csv
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from chartstyle import STYLE  # noqa: E402

res = sys.argv[1] if len(sys.argv) > 1 else "bench-results"
out_dir = sys.argv[2] if len(sys.argv) > 2 else "docs"

PCTS = [("p50", "p50_ns", "r1"), ("p99", "p99_ns", "r2"), ("p99.9", "p999_ns", "r3")]

sat = {}
for r in csv.DictReader(open(os.path.join(res, "epyc-9845-experiments.csv"))):
    if (r["experiment"], r["target"], r["variant"], r["workload"]) == ("load", "Latest", "plain", "Dups"):
        sat[int(r["threads"])] = r
live = {}
for r in csv.DictReader(open(os.path.join(res, "latency-epyc-9845.csv"))):
    if r["target"] == "LatestPlain" and float(r["rate_kps"]) == 50:
        live[int(r["streams"])] = r

groups = [(f"{t} thread{'s' if t > 1 else ''}", sat[t]) for t in (1, 2, 4)]
groups += [(f"{s} streams", live[s]) for s in (2, 4)]
n_sat = 3

W, H = 760, 380
left, top, plot_w, plot_h = 56, 120, 680, 190
vmax = max(float(g[k]) for _, g in groups for _, k, _ in PCTS)
step = 50 if vmax <= 400 else 100
ymax = math.ceil(vmax * 1.12 / step) * step


def y(v):
    return top + plot_h - v / ymax * plot_h


svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
       f'aria-label="Window::Latest with a plain load, register() time per call: p50, p99 and p99.9 under saturated arbitration '
       f'with 1, 2 and 4 threads, and in the paced live feed with 2 and 4 streams">', STYLE,
       f'<rect class="bg" width="{W}" height="{H}" rx="8"/>',
       f'<text class="title" x="{left}" y="30">Window::Latest with a plain load: time per register() call</text>',
       f'<text class="sub" x="{left}" y="50">ns per call on an EPYC 9845; lower is better. '
       f'The cost grows with the number of cores sharing the line.</text>']
lx = left
for name, _, color in PCTS:
    svg.append(f'<rect x="{lx}" y="68" width="12" height="12" rx="3" class="f-{color}"/>')
    svg.append(f'<text class="leg" x="{lx + 18}" y="78">{name}</text>')
    lx += 18 + 7.2 * len(name) + 24

v = 0
while v <= ymax:
    yy = y(v)
    svg.append(f'<line class="{"axis" if v == 0 else "grid"}" x1="{left}" y1="{yy:.1f}" x2="{left + plot_w}" y2="{yy:.1f}"/>')
    svg.append(f'<text class="tick" x="{left - 8}" y="{yy + 4:.1f}" text-anchor="end">{v}</text>')
    v += step

group_w = plot_w / len(groups)
bar_w, gap = 26, 2
def fmt_ns(v):
    # The timer reads in 10 ns steps, so 10 and 20 are upper bounds.
    if v <= 20:
        return f"&lt;{10 if v <= 10 else 20} ns"
    return f"{v:.0f} ns"


for gi, (label, r) in enumerate(groups):
    gx = left + gi * group_w + group_w / 2
    svg.append(f'<text class="tick" x="{gx:.1f}" y="{top + plot_h + 20}" text-anchor="middle">{label}</text>')
    x0 = gx - (len(PCTS) * bar_w + (len(PCTS) - 1) * gap) / 2
    for k, (name, key, color) in enumerate(PCTS):
        val = float(r[key])
        x = x0 + k * (bar_w + gap)
        # Values below the ~10 ns timer resolution get a visible stub.
        yt = min(y(val), top + plot_h - 2)
        h = top + plot_h - yt
        rr = min(4, h / 2)
        d = (f"M{x:.1f},{top + plot_h:.1f} L{x:.1f},{yt + rr:.1f} Q{x:.1f},{yt:.1f} {x + rr:.1f},{yt:.1f} "
             f"L{x + bar_w - rr:.1f},{yt:.1f} Q{x + bar_w:.1f},{yt:.1f} {x + bar_w:.1f},{yt + rr:.1f} "
             f"L{x + bar_w:.1f},{top + plot_h:.1f} Z")
        shown = fmt_ns(val)
        svg.append(f'<g><title>{label}: {name} = {shown}</title><path d="{d}" class="f-{color}"/></g>')
    # One label per group, above its tallest bar: the p99 value.
    p99 = float(r["p99_ns"])
    top_y = min(y(float(r[k])) for _, k, _ in PCTS)
    svg.append(f'<text class="val" x="{gx:.1f}" y="{top_y - 6:.1f}" text-anchor="middle">'
               f'p99 {fmt_ns(p99)}</text>')

# Separate the two measurement kinds, each with its own caption.
sep = left + n_sat * group_w
svg.append(f'<line class="axis" x1="{sep:.1f}" y1="{top - 14}" x2="{sep:.1f}" y2="{top + plot_h + 34}" stroke-dasharray="3 3"/>')
svg.append(f'<text class="ptitle" x="{left + 4}" y="{top - 4}">Saturated arbitration (every thread sees every id)</text>')
svg.append(f'<text class="ptitle" x="{sep + 10:.1f}" y="{top - 4}">Live feed, 50k msg/s per stream</text>')
svg.append(f'<text class="sub" x="{left}" y="{top + plot_h + 52}">Saturated throughput: '
           f'{float(sat[1]["mops"]):.0f}, {float(sat[2]["mops"]):.0f} and {float(sat[4]["mops"]):.0f} M ops/s '
           f'(all threads) at 1, 2 and 4 threads.</text>')
svg.append('</svg>')
path = os.path.join(out_dir, "blog-watermark.svg")
open(path, "w").write("\n".join(svg))
print("wrote", path)
