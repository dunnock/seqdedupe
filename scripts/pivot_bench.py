import csv,sys,collections
rows=list(csv.DictReader(open(sys.argv[1])))
main=[r for r in rows if r['experiment']=='main']
ths=sorted({int(r['threads']) for r in main})
print("| workload | target | "+" | ".join(f"{t}T Mops/s (p50/p99 ns, retr/op, New/Dup/Old%)" for t in ths)+" |")
print("|---|---|"+"---|"*len(ths))
d={(r['workload'],r['target'],int(r['threads'])):r for r in main}
for wl in ['Seq','Gaps','Late','Dups']:
    for tg in ['WatermarkOnly','Bits64','Bits128','Bits256','Bits512','naive-scc/64','naive-scc/512']:
        cells=[]
        for t in ths:
            r=d.get((wl,tg,t))
            if not r: cells.append('-'); continue
            cells.append(f"{float(r['mops']):.1f} ({float(r['p50_ns']):.0f}/{float(r['p99_ns']):.0f}, {float(r['retries_per_op']):.2f}, {float(r['new_pct']):.0f}/{float(r['dup_pct']):.0f}/{float(r['old_pct']):.0f})")
        print(f"| {wl} | {tg} | "+" | ".join(cells)+" |")
print()
for exp in ['shift','delta1','load','preprotect','protection','recheck','best','batch']:
    ex=[r for r in rows if r['experiment']==exp]
    if not ex: continue
    print(f"\n#### {exp}\n\n| target | variant | workload | thr | Mops/s | ns/op/thr | p50 | p99 | retries/op |\n|---|---|---|---|---|---|---|---|---|")
    for r in ex:
        print(f"| {r['target']} | {r['variant']} | {r['workload']} | {r['threads']} | {float(r['mops']):.1f} | {float(r['ns_per_op']):.1f} | {float(r['p50_ns']):.0f} | {float(r['p99_ns']):.0f} | {float(r['retries_per_op']):.2f} |")
