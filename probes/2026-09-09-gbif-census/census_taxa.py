"""Rung 6, pass 2: how big the taxonomy's largest artifacts would be, from a sample of parts.

The `layers` stage holds the largest single artifact's members resident while it sorts them
(`merge_member_runs`), so the level-0 artifact — a kingdom over billions of rows — is what sets
the build's peak. Sampled rather than scanned: 200 of 8,369 parts, extrapolated by row count.
"""
import collections, glob, json, random, sys, time
import pyarrow.parquet as pq
import concurrent.futures as cf

PARTS = sorted(glob.glob('/mnt/nas/joe/tessera/datasets/gbif/2026-06-01/occurrence.parquet/*'))
random.seed(0)
SAMPLE = random.sample(PARTS, 200)
COLS = ['kingdom', 'phylum', 'class', 'family', 'species', 'basisofrecord', 'countrycode', 'year']

counts = {c: collections.Counter() for c in COLS}
rows = 0
t0 = time.time()

def one(p):
    t = pq.read_table(p, columns=COLS)
    return t.num_rows, {c: collections.Counter(t.column(c).to_pylist()) for c in COLS}

with cf.ThreadPoolExecutor(max_workers=8) as pool:
    for i, (n, per) in enumerate(pool.map(one, SAMPLE)):
        rows += n
        for c in COLS:
            counts[c].update(per[c])
        if (i + 1) % 50 == 0:
            print(f'  {i+1}/200 parts, {rows:,} rows, {time.time()-t0:.0f}s', file=sys.stderr, flush=True)

TOTAL = 3_654_488_638
scale = TOTAL / rows
out = {'sampled_rows': rows, 'scale': scale, 'seconds': round(time.time()-t0, 1), 'columns': {}}
for c in COLS:
    ranked = counts[c].most_common()
    nonnull = sum(v for k, v in ranked if k is not None)
    out['columns'][c] = {
        'distinct_in_sample': len([k for k, _ in ranked if k is not None]),
        'null_fraction': 1 - nonnull / rows,
        'top': [(str(k), int(v * scale)) for k, v in ranked[:8]],
    }
print(json.dumps(out, indent=1))
