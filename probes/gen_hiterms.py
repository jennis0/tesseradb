"""High terms-per-item config for the scaled corpus.

Separation of concerns: `surnames` stresses **dictionary scale** (10M-117M
distinct terms via the fold knob); this stresses **per-item breadth** —
each item carries 10 to 1000 terms, log-uniformly distributed (mean
~215), drawn from a Zipf vocabulary so head and tail terms coexist.

Two knobs give a spectrum rather than a point:

  --max-entity  how far up the entity space the config reaches (pair
                volume — the binding cost at this breadth)
  --overlap     0.0 to 1.0: the fraction of an item's terms drawn from
                its *profile band* rather than independently. 0 gives
                independent draws and near-unique permission signatures;
                1 confines items to a narrow shared band, so items in a
                profile share nearly every term and signature groups are
                strong. This is the only config that asks whether
                signature grouping (scaling analysis 5.3) survives at
                high terms-per-item — surnames is near-unique and
                categories is low-t, so neither can answer it.

Why it is entity-capped. At 10-1000 terms/item the pair relation is the
binding cost: 10^9 items would be ~2x10^11 pairs (~1.7 TB). The config
therefore covers `entity_id < --max-entity` (default 10^7 => ~2.15x10^9
pairs, the same order as surnames) and declares that limit in its
manifest. This fits the prefix model: the 250,000 and 2,422,486 scales
are covered in full, and the config simply does not reach 250M/1B.

Every item overflows the design's 64-term cap by construction, which is
the point: at t >= 100 the cap stops being a safety gate and becomes a
policy parameter that has to scale with the encoding (see the item-side
inflation addendum in stage1-results.md).

Rows are written sorted by (term_id, entity_id) within each row group so
DELTA_BINARY_PACKED applies — ~4x smaller and ~3x faster to read.

Usage: gen_hiterms.py <outdir> [--max-entity 10000000] [--vocab 1000000]
                      [--t-min 10] [--t-max 1000] [--zipf-s 1.1] [--seed 0]
"""

import argparse
import json
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

ap = argparse.ArgumentParser()
ap.add_argument("outdir")
ap.add_argument("--max-entity", type=int, default=10_000_000)
ap.add_argument("--vocab", type=int, default=1_000_000)
ap.add_argument("--t-min", type=int, default=10)
ap.add_argument("--t-max", type=int, default=1000)
ap.add_argument("--zipf-s", type=float, default=1.1)
ap.add_argument("--overlap", type=float, default=0.0)
ap.add_argument("--profiles", type=int, default=10_000)
ap.add_argument("--name", default=None)
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--chunk", type=int, default=100_000)
args = ap.parse_args()

out = Path(args.outdir)
(out / "pairs").mkdir(parents=True, exist_ok=True)
rng = np.random.default_rng(args.seed)
t0 = time.perf_counter()

# Zipf CDF over the vocabulary; searchsorted on uniforms is far faster
# than np.random.zipf and lets us bound the vocabulary exactly.
w = 1.0 / np.arange(1, args.vocab + 1, dtype=np.float64) ** args.zipf_s
cdf = np.cumsum(w / w.sum())

name = args.name or (f"hiterms-ov{args.overlap:g}" if args.overlap else "hiterms")
band = max(1, args.vocab // args.profiles)   # a profile's private slice
f = out / "pairs" / f"{name}.pairs.parquet"
schema = pa.schema([("entity_id", pa.uint32()), ("term_id", pa.uint32())])
writer = pq.ParquetWriter(f, schema, compression="zstd", use_dictionary=False,
                          column_encoding={"entity_id": "DELTA_BINARY_PACKED",
                                           "term_id": "DELTA_BINARY_PACKED"})
tot_pairs = 0
tmin, tmax = 0, 0
t_hist = np.zeros(args.t_max + 1, dtype=np.int64)
seen_terms = np.zeros(args.vocab, dtype=bool)

for lo in range(0, args.max_entity, args.chunk):
    hi = min(lo + args.chunk, args.max_entity)
    n = hi - lo
    # terms per item: log-uniform over [t_min, t_max]
    t = np.exp(rng.uniform(np.log(args.t_min), np.log(args.t_max + 1), n)
               ).astype(np.int64).clip(args.t_min, args.t_max)
    t_hist += np.bincount(t, minlength=args.t_max + 1)
    total = int(t.sum())
    ent = np.repeat(np.arange(lo, hi, dtype=np.uint32), t)
    tid = np.searchsorted(cdf, rng.random(total), side="right"
                          ).astype(np.uint32).clip(0, args.vocab - 1)
    if args.overlap > 0:
        # each item belongs to a profile owning a contiguous band of the
        # vocabulary; `overlap` of its terms come from that band, so items
        # sharing a profile share most of their term set
        prof = rng.integers(0, args.profiles, n)
        prof_of_pair = np.repeat(prof, t)
        from_band = rng.random(total) < args.overlap
        in_band = (prof_of_pair * band + (tid % band)).astype(np.uint32)
        tid = np.where(from_band, np.minimum(in_band, args.vocab - 1), tid)
    # an item must not carry the same term twice; drop duplicates per item
    order = np.lexsort((tid, ent))
    ent, tid = ent[order], tid[order]
    keep = np.empty(len(ent), dtype=bool)
    keep[0] = True
    keep[1:] = (ent[1:] != ent[:-1]) | (tid[1:] != tid[:-1])
    ent, tid = ent[keep], tid[keep]
    seen_terms[tid] = True
    # write sorted by (term_id, entity_id) so delta encoding applies
    o2 = np.lexsort((ent, tid))
    writer.write_table(pa.table({"entity_id": pa.array(ent[o2], pa.uint32()),
                                 "term_id": pa.array(tid[o2], pa.uint32())},
                                schema=schema))
    tot_pairs += len(ent)
    if (lo // args.chunk) % 20 == 0:
        print(f"[{time.perf_counter() - t0:7.1f}s] entity {hi:,}/{args.max_entity:,}, "
              f"{tot_pairs:,} pairs", flush=True)
writer.close()

nz = np.flatnonzero(t_hist)
counts = t_hist[nz]
c = np.cumsum(counts)
med = nz[np.searchsorted(c, c[-1] * 0.5)]
p99 = nz[np.searchsorted(c, c[-1] * 0.99)]
print(f"\n[{time.perf_counter() - t0:7.1f}s] wrote {f} ({f.stat().st_size / 1e9:.2f} GB)")
print(f"  entities {args.max_entity:,}, pairs {tot_pairs:,} "
      f"({tot_pairs / args.max_entity:.1f} terms/item mean)")
print(f"  terms/item: min {nz[0]}, median {med}, p99 {p99}, max {nz[-1]}; "
      f"over the 64-term cap: {100 * counts[nz > 64].sum() / counts.sum():.1f}%")
print(f"  distinct terms used: {int(seen_terms.sum()):,} / {args.vocab:,} "
      f"(overlap {args.overlap:g}, {args.profiles:,} profiles, band {band})")

(out / "pairs" / f"{name}.json").write_text(json.dumps({
    "config": name, "entity_id_limit": args.max_entity,
    "overlap": args.overlap, "profiles": args.profiles, "band": band,
    "pairs": tot_pairs, "terms": int(seen_terms.sum()),
    "terms_per_item": {"min": int(nz[0]), "median": int(med), "p99": int(p99),
                       "max": int(nz[-1]), "mean": tot_pairs / args.max_entity},
    "vocab": args.vocab, "zipf_s": args.zipf_s, "seed": args.seed,
    "note": "covers entity_id < limit only; the 250,000 and 2,422,486 scales "
            "are complete, 250M and 1B are not reached (pair volume)",
}, indent=2) + "\n")
