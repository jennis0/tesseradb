"""Rung 6 (GBIF) census, pass 1: row counts and column bytes from the Parquet footers alone.

Reads no row data — `ParquetFile.metadata` is the footer — so the whole pass is 8,369 small reads
off the share. Answers the two questions that decide the rung: how much of the u32 entity space
3.50e9 rows actually take, and which columns would dominate a bundle.

    python3 gbif_census.py > census.json
"""
import concurrent.futures as cf, glob, json, sys, time
import pyarrow.parquet as pq

PARTS = sorted(glob.glob('/mnt/nas/joe/tessera/datasets/gbif/2026-06-01/occurrence.parquet/*'))


def one(path):
    md = pq.ParquetFile(path).metadata
    per_col = {}
    for rg in range(md.num_row_groups):
        g = md.row_group(rg)
        for c in range(g.num_columns):
            col = g.column(c)
            per_col[col.path_in_schema] = per_col.get(col.path_in_schema, 0) + col.total_compressed_size
    return md.num_rows, per_col


rows = 0
cols: dict[str, int] = {}
done = 0
t0 = time.time()
with cf.ThreadPoolExecutor(max_workers=16) as pool:
    for n, per_col in pool.map(one, PARTS):
        rows += n
        for k, v in per_col.items():
            cols[k] = cols.get(k, 0) + v
        done += 1
        if done % 500 == 0:
            print(f'  {done}/{len(PARTS)} parts, {rows:,} rows, {time.time()-t0:.0f}s', file=sys.stderr, flush=True)

U32 = 2**32
out = {
    'parts': len(PARTS),
    'rows': rows,
    'u32_ceiling': U32,
    'entity_space_used': rows / U32,
    'compressed_bytes_total': sum(cols.values()),
    'compressed_bytes_by_column': dict(sorted(cols.items(), key=lambda kv: -kv[1])),
    'seconds': round(time.time() - t0, 1),
}
print(json.dumps(out, indent=1))
