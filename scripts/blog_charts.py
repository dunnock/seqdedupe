#!/usr/bin/env python3
"""Charts for docs/blog.md (static SVG, no dependencies).

usage: scripts/blog_charts.py [bench-results dir] [out dir]

Writes <out>/blog-watermark.svg: time per register() call (p50 / p99 /
p99.9) of the first `Latest` design, with a plain load of the watermark,
under saturated arbitration (Dups, 1/2/4 threads, the `load` experiment's
`plain` variant in epyc-9845-experiments.csv) and in the paced live feed
(2 and 4 streams at 50k msg/s, target LatestPlain in latency-epyc-9845.csv).

Writes <out>/blog-latest-locked.svg: the same, plain vs locked load, mean /
p99 / p99.9 at 2 and 4 threads (Dups) and 2 and 4 streams (50k msg/s).

Writes <out>/blog-hashset.svg: Latest vs HashSet64, mean / p99 / p99.9 for
Gaps on 1 thread and Dups on 2 and 4 threads (epyc-9845.csv).

Writes <out>/blog-last64.svg: HashSet64 vs Last64, Dups on 1, 2 and 4 threads.
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


# --- Paired comparison charts: two series side by side per statistic ---
STATS = [("mean", "mean_ns"), ("p99", "p99_ns"), ("p99.9", "p999_ns")]


def short(v):
    return f"&lt;{10 if v <= 10 else 20}" if v <= 20 else f"{v:,.0f}"


def pair_chart(fname, title, sub, series, groups, panels, footer):
    """series: [(key, name, color)]; groups: [(label, {key: row})];
    panels: [(first group index, caption)]."""
    W, H = 760, 400
    left, top, plot_w, plot_h = 64, 120, 672, 200
    vmax = max(float(g[s][k]) for _, g in groups for s, _, _ in series for _, k in STATS)
    step = next(s for s in (25, 50, 100, 250, 500, 1000) if vmax * 1.15 / s <= 6)
    ymax = math.ceil(vmax * 1.15 / step) * step

    def yv(v):
        return top + plot_h - v / ymax * plot_h

    names = " vs ".join(n for _, n, _ in series)
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
           f'aria-label="{title}: mean, p99 and p99.9 per register() call, {names}, for '
           f'{", ".join(l for l, _ in groups)}">', STYLE,
           f'<rect class="bg" width="{W}" height="{H}" rx="8"/>',
           f'<text class="title" x="{left}" y="30">{title}</text>',
           f'<text class="sub" x="{left}" y="50">{sub}</text>']
    lx = left
    for _, name, color in series:
        svg.append(f'<rect x="{lx}" y="68" width="12" height="12" rx="3" class="f-{color}"/>')
        svg.append(f'<text class="leg" x="{lx + 18}" y="78">{name}</text>')
        lx += 18 + 7.2 * len(name) + 24
    v = 0
    while v <= ymax:
        yy = yv(v)
        svg.append(f'<line class="{"axis" if v == 0 else "grid"}" x1="{left}" y1="{yy:.1f}" x2="{left + plot_w}" y2="{yy:.1f}"/>')
        svg.append(f'<text class="tick" x="{left - 8}" y="{yy + 4:.1f}" text-anchor="end">{v:,}</text>')
        v += step
    group_w = plot_w / len(groups)
    pair_w = group_w / len(STATS)
    bar_w, gap = 18, 2
    for gi, (label, rows) in enumerate(groups):
        g0 = left + gi * group_w
        svg.append(f'<text class="ptitle" x="{g0 + group_w / 2:.1f}" y="{top + plot_h + 38}" text-anchor="middle">{label}</text>')
        for si, (sname, key) in enumerate(STATS):
            px = g0 + si * pair_w + pair_w / 2
            svg.append(f'<text class="tick" x="{px:.1f}" y="{top + plot_h + 18}" text-anchor="middle">{sname}</text>')
            x0 = px - (len(series) * bar_w + gap) / 2
            for k, (var, vname, color) in enumerate(series):
                val = float(rows[var][key])
                x = x0 + k * (bar_w + gap)
                # Values below the ~10 ns timer resolution get a visible stub.
                yt = min(yv(val), top + plot_h - 2)
                rr = min(4, (top + plot_h - yt) / 2)
                d = (f"M{x:.1f},{top + plot_h:.1f} L{x:.1f},{yt + rr:.1f} Q{x:.1f},{yt:.1f} {x + rr:.1f},{yt:.1f} "
                     f"L{x + bar_w - rr:.1f},{yt:.1f} Q{x + bar_w:.1f},{yt:.1f} {x + bar_w:.1f},{yt + rr:.1f} "
                     f"L{x + bar_w:.1f},{top + plot_h:.1f} Z")
                svg.append(f'<g><title>{label}, {sname}, {vname}: {short(val)} ns</title><path d="{d}" class="f-{color}"/></g>')
                svg.append(f'<text class="val" x="{x + bar_w / 2:.1f}" y="{yt - 4:.1f}" text-anchor="middle">{short(val)}</text>')
    for gi, caption in panels:
        x = left + gi * group_w
        if gi:
            svg.append(f'<line class="axis" x1="{x:.1f}" y1="{top - 14}" x2="{x:.1f}" y2="{top + plot_h + 44}" stroke-dasharray="3 3"/>')
        svg.append(f'<text class="ptitle" x="{x + (4 if gi == 0 else 10):.1f}" y="{top - 4}">{caption}</text>')
    svg.append(f'<text class="sub" x="{left}" y="{top + plot_h + 66}">{footer}</text>')
    svg.append('</svg>')
    path = os.path.join(out_dir, fname)
    open(path, "w").write("\n".join(svg))
    print("wrote", path)


exp = list(csv.DictReader(open(os.path.join(res, "epyc-9845-experiments.csv"))))
main = list(csv.DictReader(open(os.path.join(res, "epyc-9845.csv"))))
lat = list(csv.DictReader(open(os.path.join(res, "latency-epyc-9845.csv"))))

# blog-latest-locked.svg: Latest with a plain vs a locked watermark load.
ld = {(r["variant"], int(r["threads"])): r for r in exp
      if (r["experiment"], r["target"], r["workload"]) == ("load", "Latest", "Dups")}
lv = {("plain" if r["target"] == "LatestPlain" else "locked", int(r["streams"])): r for r in lat
      if r["target"] in ("Latest", "LatestPlain") and float(r["rate_kps"]) == 50}
LOADS = [("plain", "Plain load", "s2"), ("locked", "Locked load", "s1")]
pair_chart(
    "blog-latest-locked.svg", "Window::Latest: plain vs locked load of the watermark",
    "ns per register() call on an EPYC 9845; lower is better.", LOADS,
    [(f"{t} threads", {v: ld[(v, t)] for v, _, _ in LOADS}) for t in (2, 4)]
    + [(f"{s} streams", {v: lv[(v, s)] for v, _, _ in LOADS}) for s in (2, 4)],
    [(0, "Saturated arbitration (Dups)"), (2, "Live feed, 50k msg/s per stream")],
    "Throughput, all threads, plain → locked: "
    + ", ".join(f'{float(ld[("plain", t)]["mops"]):.0f} → {float(ld[("locked", t)]["mops"]):.0f}{" M ops/s" if t == 1 else ""} '
                f'{"on 1 thread" if t == 1 else f"on {t}"}' for t in (1, 2, 4)) + ".")

# blog-hashset.svg: Latest vs HashSet64 (main matrix).
mm = {(r["target"], r["workload"], int(r["threads"])): r for r in main}
HS = [("Latest", "Latest", "s1"), ("HashSet64", "HashSet64", "s2")]
cases = [("Gaps", 1), ("Dups", 2), ("Dups", 4)]
pair_chart(
    "blog-hashset.svg", "Window::Latest vs a bounded hash set (HashSet64)",
    "ns per register() call on an EPYC 9845; lower is better.", HS,
    [(f"{w}, {t} thread{'s' if t > 1 else ''}", {k: mm[(k, w, t)] for k, _, _ in HS}) for w, t in cases],
    [(0, "Saturated: Gaps on one thread, Dups (every thread sees every id) on 2 and 4")],
    "Saturated throughput, all threads: "
    + ", ".join(f'{w} {t}T {float(mm[("Latest", w, t)]["mops"]):.0f} vs {float(mm[("HashSet64", w, t)]["mops"]):.0f} M ops/s'
                for w, t in cases) + ".")

# blog-last64.svg: HashSet64 vs Last64, Dups (main matrix).
L64 = [("HashSet64", "HashSet64", "s2"), ("Last64", "Last64", "s3")]
pair_chart(
    "blog-last64.svg", "Window::Last64 vs a bounded hash set (HashSet64)",
    "ns per register() call on an EPYC 9845; lower is better.", L64,
    [(f"Dups, {t} thread{'s' if t > 1 else ''}", {k: mm[(k, "Dups", t)] for k, _, _ in L64}) for t in (1, 2, 4)],
    [(0, "Saturated arbitration (Dups): every thread sees every id")],
    "Throughput, all threads, HashSet64 vs Last64: "
    + ", ".join(f'{float(mm[("HashSet64", "Dups", t)]["mops"]):.0f} vs {float(mm[("Last64", "Dups", t)]["mops"]):.0f}'
                f'{" M ops/s" if t == 1 else ""} {"on 1 thread" if t == 1 else f"on {t}"}' for t in (1, 2, 4)) + ".")
