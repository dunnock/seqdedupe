"""Shared palette and SVG style for the chart scripts (light + dark)."""

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
