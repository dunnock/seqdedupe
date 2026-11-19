#!/usr/bin/env python3
"""Render the latency benchmark CSVs as static SVG charts (no dependencies).

usage: scripts/latency_charts.py bench-results/latency-HOST [out_dir]

Reads <prefix>.csv and <prefix>-curves.csv (written by benches/latency.rs)
and writes <out_dir>/latency-tails.svg and <out_dir>/latency-p99.svg.

Palette: the validated categorical order (blue, orange, aqua, yellow,
magenta, green), light and dark steps; dark mode via prefers-color-scheme.
Each series also has its own marker shape, so identity never rests on color
alone. Marks carry <title> tooltips when the SVG is opened directly.
"""
import csv
import math
import os
import sys
from collections import defaultdict

prefix = sys.argv[1]
out_dir = sys.argv[2] if len(sys.argv) > 2 else os.path.dirname(prefix) or "."

TARGETS = ["WatermarkOnly", "Bits64", "Bits128", "Bits256", "Bits512", "naive-scc/512"]
LABEL = {"naive-scc/512": "naive scc (N=512)"}
SHAPES = ["circle", "square", "diamond", "triangle", "tri-down", "cross"]
QUANTS = [(0.5, "p50"), (0.9, "p90"), (0.99, "p99"), (0.999, "p99.9"), (0.9999, "p99.99")]

LIGHT = dict(surface="#fcfcfb", text1="#0b0b0b", text2="#52514e", muted="#898781",
             grid="#e1e0d9", axis="#c3c2b7",
             s1="#2a78d6", s2="#eb6834", s3="#1baf7a", s4="#eda100", s5="#e87ba4", s6="#008300",
             r1="#86b6ef", r2="#2a78d6", r3="#104281")
DARK = dict(surface="#1a1a19", text1="#f0efec", text2="#c3c2b7", muted="#898781",
            grid="#2c2c2a", axis="#383835",
            s1="#3987e5", s2="#d95926", s3="#199e70", s4="#c98500", s5="#d55181", s6="#0ca30c",
            r1="#184f95", r2="#3987e5", r3="#86b6ef")


def rules(t):
    # Plain classes (no CSS variables) so every SVG renderer gets the colors.
    out = [f".bg{{fill:{t['surface']}}}", f".title{{fill:{t['text1']}}}", f".ptitle{{fill:{t['text1']}}}",
           f".sub,.leg,.val{{fill:{t['text2']}}}", f".tick{{fill:{t['muted']}}}",
           f".grid{{stroke:{t['grid']}}}", f".axis{{stroke:{t['axis']}}}", f".k-surface{{stroke:{t['surface']}}}"]
    for k in [f"s{i}" for i in range(1, 7)] + ["r1", "r2", "r3"]:
        out.append(f".f-{k}{{fill:{t[k]}}} .k-{k}{{stroke:{t[k]}}}")
    return "\n  ".join(out)


STYLE = f"""
<style>
  text {{ font-family: system-ui, -apple-system, "Segoe UI", sans-serif; }}
  .title {{ font-size: 15px; font-weight: 600; }}
  .sub {{ font-size: 12px; }}
  .tick {{ font-size: 11px; font-variant-numeric: tabular-nums; }}
  .ptitle {{ font-size: 12px; font-weight: 600; }}
  .leg {{ font-size: 12px; }}
  .val {{ font-size: 10px; font-variant-numeric: tabular-nums; }}
  .grid, .axis {{ stroke-width: 1; }}
  {rules(LIGHT)}
  @media (prefers-color-scheme: dark) {{
  {rules(DARK)}
  }}
</style>
"""


def series_var(i):
    return f"s{i + 1}"


def marker(shape, x, y, color, r=4.5):
    ring = f'class="f-{color} k-surface" stroke-width="2"'
    if shape == "circle":
        return f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" {ring}/>'
    if shape == "square":
        return f'<rect x="{x - r:.1f}" y="{y - r:.1f}" width="{2 * r}" height="{2 * r}" rx="1.5" {ring}/>'
    if shape == "diamond":
        d = r * 1.3
        return f'<path d="M{x:.1f},{y - d:.1f} L{x + d:.1f},{y:.1f} L{x:.1f},{y + d:.1f} L{x - d:.1f},{y:.1f} Z" {ring}/>'
    if shape == "triangle":
        d = r * 1.3
        return f'<path d="M{x:.1f},{y - d:.1f} L{x + d:.1f},{y + d * 0.8:.1f} L{x - d:.1f},{y + d * 0.8:.1f} Z" {ring}/>'
    if shape == "tri-down":
        d = r * 1.3
        return f'<path d="M{x:.1f},{y + d:.1f} L{x + d:.1f},{y - d * 0.8:.1f} L{x - d:.1f},{y - d * 0.8:.1f} Z" {ring}/>'
    # cross
    d = r * 1.1
    return (
        f'<path d="M{x - d:.1f},{y - d:.1f} L{x + d:.1f},{y + d:.1f} M{x + d:.1f},{y - d:.1f} L{x - d:.1f},{y + d:.1f}" '
        f'class="k-surface" fill="none" stroke-width="5" stroke-linecap="round"/>'
        f'<path d="M{x - d:.1f},{y - d:.1f} L{x + d:.1f},{y + d:.1f} M{x + d:.1f},{y - d:.1f} L{x - d:.1f},{y + d:.1f}" '
        f'class="k-{color}" fill="none" stroke-width="2.5" stroke-linecap="round"/>'
    )


def legend(x, y, items, gap=24):
    out = []
    for i, (name, color, shape) in enumerate(items):
        out.append(f'<line x1="{x}" y1="{y}" x2="{x + 22}" y2="{y}" class="k-{color}" stroke-width="2"/>')
        out.append(marker(shape, x + 11, y, color, r=4))
        out.append(f'<text class="leg" x="{x + 28}" y="{y + 4}">{name}</text>')
        x += 28 + 7.2 * len(name) + gap
    return out


# ---------------------------------------------------------------- data
curves = defaultdict(dict)  # (target, rate) -> {q: ns}
for r in csv.DictReader(open(prefix + "-curves.csv")):
    curves[(r["target"], float(r["rate_kps"]))][float(r["quantile"])] = float(r["ns"])
summary = {(r["target"], float(r["rate_kps"])): r for r in csv.DictReader(open(prefix + ".csv"))}
rates = sorted({k[1] for k in curves})
targets = [t for t in TARGETS if any((t, rt) in curves for rt in rates)]


def fmt_rate(r):
    return f"{r:g}k msg/s per stream"


# ---------------------------------------------------- chart 1: tail curves
W, H = 980, 420
left, top, panel_w, panel_h, gutter = 56, 122, 270, 240, 36
ymin, ymax = 10, 3000
ticks = [10, 30, 100, 300, 1000, 3000]


def ypos(v):
    v = min(max(v, ymin), ymax)
    return top + panel_h - (math.log10(v) - math.log10(ymin)) / (math.log10(ymax) - math.log10(ymin)) * panel_h


svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
       f'aria-label="register() latency percentiles per deduper mode, one panel per message rate">', STYLE,
       f'<rect class="bg" width="{W}" height="{H}" rx="8"/>',
       f'<text class="title" x="{left}" y="30">register() latency by percentile, 4 redundant streams</text>',
       f'<text class="sub" x="{left}" y="50">ns per call (log scale); Poisson bursts shared across streams, per-stream jitter; lower is better</text>']
items = [(LABEL.get(t, t), series_var(TARGETS.index(t)), SHAPES[TARGETS.index(t)]) for t in targets]
svg += legend(left, 76, items)
for pi, rate in enumerate(rates):
    x0 = left + pi * (panel_w + gutter)
    svg.append(f'<text class="ptitle" x="{x0}" y="{top - 8}">{fmt_rate(rate)}</text>')
    for t in ticks:
        y = ypos(t)
        svg.append(f'<line class="grid" x1="{x0}" y1="{y:.1f}" x2="{x0 + panel_w}" y2="{y:.1f}"/>')
        if pi == 0:
            lab = f"{t // 1000}µs" if t >= 1000 else f"{t}"
            svg.append(f'<text class="tick" x="{x0 - 8}" y="{y + 4:.1f}" text-anchor="end">{lab}</text>')
    svg.append(f'<line class="axis" x1="{x0}" y1="{top + panel_h}" x2="{x0 + panel_w}" y2="{top + panel_h}"/>')
    xs = [x0 + 14 + i * (panel_w - 28) / (len(QUANTS) - 1) for i in range(len(QUANTS))]
    for x, (_, name) in zip(xs, QUANTS):
        svg.append(f'<text class="tick" x="{x:.1f}" y="{top + panel_h + 18}" text-anchor="middle">{name}</text>')
    for t in targets:
        i = TARGETS.index(t)
        c = curves.get((t, rate))
        if not c:
            continue
        pts = [(x, ypos(c[q]), c[q], name) for x, (q, name) in zip(xs, QUANTS)]
        d = " ".join(f"{'M' if k == 0 else 'L'}{x:.1f},{y:.1f}" for k, (x, y, _, _) in enumerate(pts))
        svg.append(f'<path d="{d}" fill="none" class="k-{series_var(i)}" stroke-width="2" stroke-linejoin="round"/>')
        for x, y, v, name in pts:
            svg.append(f'<g><title>{LABEL.get(t, t)}, {fmt_rate(rate)}: {name} = {v:.0f} ns</title>'
                       f'{marker(SHAPES[i], x, y, series_var(i))}</g>')
svg.append('</svg>')
open(os.path.join(out_dir, "latency-tails.svg"), "w").write("\n".join(svg))

# ---------------------------------------------------- chart 2: p99 bars
W2, H2 = 980, 360
left2, top2, plot_w, plot_h = 56, 96, 900, 200
vals = {(t, r): float(summary[(t, r)]["p99_ns"]) for t in targets for r in rates if (t, r) in summary}
vmax = max(vals.values())
step = 50 if vmax <= 300 else 100
ymax2 = math.ceil(vmax * 1.1 / step) * step


def y2(v):
    return top2 + plot_h - v / ymax2 * plot_h


svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W2} {H2}" width="{W2}" height="{H2}" role="img" '
       f'aria-label="p99 register() latency per deduper mode at each message rate">', STYLE,
       f'<rect class="bg" width="{W2}" height="{H2}" rx="8"/>',
       f'<text class="title" x="{left2}" y="30">p99 register() latency per mode and rate</text>',
       f'<text class="sub" x="{left2}" y="50">ns per call, 4 redundant streams; lower is better. Rate barely matters, the mode does.</text>']
rate_colors = ["r1", "r2", "r3"]
lx = left2
for k, r in enumerate(rates):
    svg.append(f'<rect x="{lx}" y="68" width="12" height="12" rx="3" class="f-{rate_colors[k]}"/>')
    svg.append(f'<text class="leg" x="{lx + 18}" y="78">{fmt_rate(r)}</text>')
    lx += 18 + 7.2 * len(fmt_rate(r)) + 24
v = 0
while v <= ymax2:
    y = y2(v)
    svg.append(f'<line class="{"axis" if v == 0 else "grid"}" x1="{left2}" y1="{y:.1f}" x2="{left2 + plot_w}" y2="{y:.1f}"/>')
    svg.append(f'<text class="tick" x="{left2 - 8}" y="{y + 4:.1f}" text-anchor="end">{v}</text>')
    v += step
group_w = plot_w / len(targets)
bar_w = 26
for gi, t in enumerate(targets):
    gx = left2 + gi * group_w + group_w / 2
    svg.append(f'<text class="tick" x="{gx:.1f}" y="{top2 + plot_h + 20}" text-anchor="middle">{LABEL.get(t, t)}</text>')
    n = len(rates)
    for k, r in enumerate(rates):
        if (t, r) not in vals:
            continue
        val = vals[(t, r)]
        x = gx - (n * bar_w + (n - 1) * 2) / 2 + k * (bar_w + 2)
        y = y2(val)
        h = top2 + plot_h - y
        rr = min(4, h / 2)
        # Rounded data end at the top, square at the baseline.
        d = (f"M{x:.1f},{top2 + plot_h:.1f} L{x:.1f},{y + rr:.1f} Q{x:.1f},{y:.1f} {x + rr:.1f},{y:.1f} "
             f"L{x + bar_w - rr:.1f},{y:.1f} Q{x + bar_w:.1f},{y:.1f} {x + bar_w:.1f},{y + rr:.1f} "
             f"L{x + bar_w:.1f},{top2 + plot_h:.1f} Z")
        svg.append(f'<g><title>{LABEL.get(t, t)}, {fmt_rate(r)}: p99 = {val:.0f} ns</title>'
                   f'<path d="{d}" class="f-{rate_colors[k]}"/></g>')
    # One value label per group: the highest bar.
    worst = max(vals[(t, r)] for r in rates if (t, r) in vals)
    svg.append(f'<text class="val" x="{gx:.1f}" y="{y2(worst) - 6:.1f}" text-anchor="middle">{worst:.0f} ns</text>')
svg.append('</svg>')
open(os.path.join(out_dir, "latency-p99.svg"), "w").write("\n".join(svg))
print("wrote", os.path.join(out_dir, "latency-tails.svg"), os.path.join(out_dir, "latency-p99.svg"))
