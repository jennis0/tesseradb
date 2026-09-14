"""Final tables for the identity-bands write-up.

Reference (R) and render (G) figures come from the first full run (results.json); band-route
(B) figures from the final band-only pass (argument 1). Both over the same bundle, principals,
cases and cut.
"""
import json
import sys

D = __import__('os').path.dirname(__file__) + '/'
first = json.load(open(D + 'results-all-arms.json'))
final = json.load(open(D + (sys.argv[1] if len(sys.argv) > 1 else 'results-band-only.json')))

R = {(p['principal'], c['case']): c for p in first['principals'] for c in p['cases']}
B = {(p['principal'], c['case']): c for p in final['principals'] for c in p['cases']}
P = [p['principal'] for p in final['principals']]
LABEL = {'p1': '1%', 'p5': '5%', 'p10': '10%', 'p25': '25%', 'p50': '50%', 'p100': '100%'}


def s(x, nd=2):
    return f"{x:.{nd}f}"


print("## Whole map, k = 30 (the battery's request)\n")
print("| principal | reference cold | reference hot | reference read, hot | band cold | band hot | band read, cold |")
print("|---|---|---|---|---|---|---|")
for p in P:
    r = R[(p, 'whole_k30')]['arms']; b = B[(p, 'whole_k30')]['arms']
    print(f"| {LABEL[p]} | {s(r['R.cold']['wall_s'])} s | {s(r['R.hot']['wall_s'])} s | {s(r['R.hot']['read_bytes']/1e9,1)} GB | {s(b['B.cold']['wall_s'],3)} s | {s(b['B.hot']['wall_s'],3)} s | {s(b['B.cold']['read_bytes']/1e9,2)} GB |")

print("\n## Whole map, 2,000,000-mark budget (depth 9)\n")
print("| principal | served | reference cold | reference hot | reference CPU, hot | reference read, hot | band cold | band hot | band CPU, hot | band read, cold | identity reads |")
print("|---|---|---|---|---|---|---|---|---|---|---|")
for p in P:
    r = R[(p, 'whole_budget')]['arms']; b = B[(p, 'whole_budget')]['arms']
    print(f"| {LABEL[p]} | {b['B.hot']['served_total']:,} | {s(r['R.cold']['wall_s'],1)} s | {s(r['R.hot']['wall_s'],1)} s | {s(r['R.hot']['cpu_s'],1)} s | {s(r['R.hot']['read_bytes']/1e9,1)} GB | {s(b['B.cold']['wall_s'])} s | {s(b['B.hot']['wall_s'],3)} s | {s(b['B.hot']['cpu_s'],3)} s | {s(b['B.cold']['read_bytes']/1e9,2)} GB | {b['B.hot']['column_reads']:,} |")

print("\n## The shape across zooms, 100% and 50% principals, budget cases, hot\n")
print("| principal | view | depth | band | served | reference wall | reference CPU | band wall | band CPU | source |")
print("|---|---|---|---|---|---|---|---|---|---|")
VIEW = {'whole_budget': 'whole map', 'zoom_2': 'densest depth-2 tile', 'zoom_4': 'densest depth-4 tile', 'zoom_6': 'densest depth-6 tile', 'zoom_8': 'densest depth-8 tile'}
for p in ('p100', 'p50'):
    for cse in ('whole_budget', 'zoom_2', 'zoom_4', 'zoom_6', 'zoom_8'):
        r = R[(p, cse)]; b = B[(p, cse)]; bh = b['arms']['B.hot']; rh = r['arms']['R.hot']
        th = b.get('threshold'); j = th.get('j') if isinstance(th, dict) else th
        src = 'list' if bh.get('list_bytes', 0) > 0 and bh.get('lz_bytes', 0) == 0 else ('leading-zero column' if bh.get('lz_bytes', 0) > 0 else 'scan')
        print(f"| {LABEL[p]} | {VIEW[cse]} | {b['zoom']} | {j} | {bh['served_total']:,} | {s(rh['wall_s'])} s | {s(rh['cpu_s'])} s | {s(bh['wall_s'],3)} s | {s(bh['cpu_s'],3)} s | {src} |")

print("\n## The band route's own reads at the whole-map budget, hot\n")
print("| principal | tiles | candidates | list bytes | leading-zero bytes | identity reads | floor tiles | settled by band / list / column | search split (candidates / count / floor) | position |")
print("|---|---|---|---|---|---|---|---|---|---|")
for p in P:
    b = B[(p, 'whole_budget')]['arms']['B.hot']
    print(f"| {LABEL[p]} | {b['tiles']:,} | {b['s_total']:,} | {s(b['list_bytes']/1e6,1)} MB | {s(b['lz_bytes']/1e6,1)} MB | {b['column_reads']:,} | {b['floor_widened_tiles']:,} | {b['floor_settled_by_band']:,} / {b['floor_settled_by_list']:,} / {b['floor_settled_by_column']:,} | {s(b.get('candidates_s',0)*1000,0)} / {s(b.get('count_s',0)*1000,0)} / {s(b.get('floor_s',0)*1000,0)} ms | {s(b['position_wall_s']*1000,1)} ms |")

print("\n## The render at the whole-map budget: two columns against the cut index (first run)\n")
print("| principal | rows | columns, cold | columns, hot | columns pages (modelled) | cut index, cold | cut index, hot | cut index pages (modelled) |")
print("|---|---|---|---|---|---|---|---|")
for p in P:
    g = R[(p, 'whole_budget')]['arms']
    gc = g.get('G.cold', {}); gh = g.get('G.hot', {})
    if not gc:
        continue
    cc, ch = gc['columns'], gh['columns']; xc, xh = gc['cells'], gh['cells']
    print(f"| {LABEL[p]} | {gc['rows']:,} | {s(cc['wall_s'])} s | {s(ch['wall_s'])} s | {s((cc['morton_pages']+cc['residual_pages'])*4096/1e9,2)} GB | {s(xc['wall_s'],3)} s | {s(xh['wall_s'],3)} s | {s((xc['cell_code_pages']+xc['cut_index_pages'])*4096/1e9,3)} GB |")

print("\n## Quantised counts against the exact one, whole-map budget (first run)\n")
print("| principal | exact | band j | ratio | band j+1 | ratio | fp16 | tie reads |")
print("|---|---|---|---|---|---|---|---|")
for p in P:
    b = R[(p, 'whole_budget')]['arms']['B.hot']
    ex = b['exact_banded'] or 1
    print(f"| {LABEL[p]} | {b['exact_banded']:,} | {b['s_total']:,} | {s(b['s_total']/ex,3)} | {b['band_above_total']:,} | {s(b['band_above_total']/ex,3)} | {b['fp16_total']:,} | {b['fp16_tie_reads']:,} |")

print("\n## Session costs (final pass)\n")
print("| principal | visible | projection build wall | projection CPU | occupancy ladder to depth 16 |")
print("|---|---|---|---|---|")
for p in final['principals']:
    print(f"| {LABEL[p['principal']]} | {p['visible_total']:,} | {s(p['projection_build']['wall_s'],1)} s | {s(p['projection_build']['cpu_s'],1)} s | {s(p['occupancy_cost']['wall_s'],2)} s |")
