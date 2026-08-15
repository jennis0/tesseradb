"""Build ONE 10^9-point test corpus whose prefixes are smaller corpora.

The key property: **entity IDs are append-only, so a prefix of entity
space is a whole, coherent corpus** — the same relationship the design
gives ingest (§5.1, I9). One artifact therefore carries four test
corpora, and a probe selects a scale by filtering `entity_id < limit`:

    250,000        prefix of the real arXiv data (oldest by v1_created)
    2,422,486      the real arXiv data, entire — replica 0, coordinates
                   copied verbatim from the hashed geometry artifact
                   and re-quantised here. Its Morton codes are NOT the
                   `morton` column of that artifact: `build_geometry.py`
                   wrote those from `gx`/`gy`, and this script quantises
                   the way the engine does (see `quantise32`). Roughly a
                   quarter of points land in a different cell, so any
                   corpus built before that must be regenerated and
                   benchmarks against it re-baselined.
    250,000,000    + transformed replicas
    1,000,000,000  + transformed replicas (the design's target; 0.23
                   points per 2^16 cell per §5.2; u32 row IDs 23% used)

**Row space per scale.** Rows are stored sorted by (morton, entity_id).
Filtering a sorted array preserves relative order, so the rows surviving
`entity_id < limit` are *already* in that sub-corpus's Morton rank order
— its row_id is just the running position:

    m = pq.read_table(geo, columns=["entity_id", "morton"])
    keep = m["entity_id"].to_numpy() < limit          # the scale's rows
    row_id = np.arange(keep.sum())                    # its Morton ranks

The stored `row_id` column is the full-corpus rank; smaller scales
derive theirs as above. This is §5.1's "each slice assigns its own row
IDs; the permutation is per-slice" with scale standing in for slice.

**Replicas are not copies.** Each is an affine transform (rotate,
reflect, scale, translate) plus jitter, placed so the composite has
density spanning orders of magnitude and both overlapping and isolated
regions. Naive tiling would make the corpus *easier* than the base:
identical copies give every term N identical posting blocks and every
tile the same occupancy, so mask and spatial structure both go
degenerate. Replicas 0-4 pin deliberate edge cases:

  0  identity, real coordinates   — the hashed artifact's coordinates,
                                    re-quantised here, not its codes
  1  extreme compression          — 2.4M points into a few grid cells:
                                    Morton collisions, hot tiles, the
                                    intra-leaf priority tiebreak at load
  2  pinned to grid corner (0,0)      — quantisation clamp low
  3  pinned to grid corner (max,max)  — quantisation clamp high
  4  degenerate line (y collapsed)    — a tile one cell tall

**Terms.** A fraction of the base vocabulary stays *global* (one term
spanning every replica: huge, spatially diffuse postings), the rest are
*replica-local* (offset per replica: smaller, clustered). Pure-global is
degenerate; pure-local makes every mask one contiguous block.

Memory: (morton, entity_id) is packed into one u64 key and sorted in
place — no argsort, no index array. Peak ~12 GB at 10^9.

Usage:
  build_scaled_corpus.py <geometry.parquet> <pairs.parquet> <outdir>
      [--points 1000000000] [--seed 0] [--global-frac 0.3] [--skip-pairs]
"""

import argparse
import json
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

CHUNK = 8_000_000

ap = argparse.ArgumentParser()
ap.add_argument("geometry")
ap.add_argument("pairs")
ap.add_argument("outdir")
ap.add_argument("--points", type=int, default=1_000_000_000)
ap.add_argument("--seed", type=int, default=0)
ap.add_argument("--global-frac", type=float, default=0.3)
ap.add_argument("--skip-pairs", action="store_true")
ap.add_argument("--pairs-only", metavar="NAME",
                help="skip geometry; emit pairs/<NAME>.pairs.parquet against the "
                     "existing scales.json (geometry is label-independent, so all "
                     "configs share one geometry.parquet)")
args = ap.parse_args()

out = Path(args.outdir)
out.mkdir(parents=True, exist_ok=True)
rng = np.random.default_rng(args.seed)
t0 = time.perf_counter()


def log(m):
    print(f"[{time.perf_counter() - t0:7.1f}s] {m}", flush=True)


def interleave16(v):
    v = v.astype(np.uint64)
    v = (v | (v << 8)) & 0x00FF00FF00FF00FF
    v = (v | (v << 4)) & 0x0F0F0F0F0F0F0F0F
    v = (v | (v << 2)) & 0x3333333333333333
    v = (v | (v << 1)) & 0x5555555555555555
    return v


# 32 bits per axis, whose high half IS the 16-bit cell: `q >> 16 == floor(t * 65536)` clamped
# to 65535, which is the engine's `cell()` (contracts §2.5). Matching the engine's scale factor
# and rounding mode is what lets the residual be defined against a shared cell boundary — an
# earlier revision rounded against 65535 instead, a different quantiser that happened not to
# show because the Morton-input build path reads the stored code and never re-quantises.
Q_BITS = 32


def quantise32(t):
    """Normalised [0, 1] -> 32-bit fixed point, floor, clamped."""
    return np.clip(np.floor(t * 2.0**Q_BITS), 0, 2**Q_BITS - 1).astype(np.uint32)


def split_code(qx, qy):
    """(cell code, sub-cell residual) from a pair of 32-bit fixed-point axes.

    **x occupies the EVEN bit positions and y the odd ones**, which is contracts §2.5 and its
    worked example (x=6, y=3 -> 30). Earlier revisions of this script had the two axes the other
    way round, so every corpus built from them was a transpose of the coordinates it came from.
    Nothing caught it, for exactly the reason the note above `Q_BITS` gives about the scale
    factor: the Morton-input build path reads the stored code and never re-quantises, so a
    self-consistently wrong corpus agrees with itself at every later stage.
    """
    m = (interleave16(qx >> np.uint32(16))
         | interleave16(qy >> np.uint32(16)) << np.uint64(1))
    r = (interleave16(qx & np.uint32(0xFFFF))
         | interleave16(qy & np.uint32(0xFFFF)) << np.uint64(1))
    return m, r.astype(np.uint32)


def splitmix(x):
    h = (x + np.uint64(0x9E3779B97F4A7C15)).astype(np.uint64)
    h ^= h >> np.uint64(30); h *= np.uint64(0xBF58476D1CE4E5B9)
    h ^= h >> np.uint64(27); h *= np.uint64(0x94D049BB133111EB)
    return h ^ (h >> np.uint64(31))


# ---------------------------------------------------------------- base
if args.pairs_only:
    _m = json.load(open(out / "scales.json"))
    n_base, N, R = _m["base_points"], _m["points"], _m["replicas"]
    SCALES = [s_["entity_id_limit"] for s_ in _m["scales"]]
    scale_rows = {s_["entity_id_limit"]: s_["rows"] for s_ in _m["scales"]}
    log(f"pairs-only: {N:,} points, {R} replicas, base {n_base:,}")
else:
  # Read the *coordinates*, not the grid cells. Reading `gx`/`gy` discarded the sub-cell
  # position before this script had a chance to carry it, so no corpus built from those
  # columns holds more than 16 bits per axis and none of it is recoverable afterwards.
  base = pq.read_table(args.geometry, columns=["entity_id", "x", "y"])
  n_base = base.num_rows
  o = np.argsort(base.column("entity_id").to_numpy())
  base_x = base.column("x").to_numpy()[o].astype(np.float64)
  base_y = base.column("y").to_numpy()[o].astype(np.float64)
  del base, o

  # Normalise each axis onto [0, 1) by its own min/max — the mapping `build_geometry.py`
  # applies, reproduced here from the columns rather than carried, so the two agree without
  # a shared constant.
  def _unit(v):
      lo, hi = v.min(), v.max()
      return (v - lo) / (float(hi) - float(lo))

  tx, ty = _unit(base_x), _unit(base_y)
  del base_x, base_y
  base_qx, base_qy = quantise32(tx), quantise32(ty)

  # Normalised copy for the transformed replicas, taken from the continuous value rather than
  # from the grid cell: transforming an already-quantised coordinate would bake the base's
  # 16-bit step into every replica.
  bx = tx - tx.mean(); by = ty - ty.mean()
  del tx, ty
  half = max(np.abs(bx).max(), np.abs(by).max())
  bx /= 2 * half; by /= 2 * half

  N = int(args.points)
  R = -(-N // n_base)                       # replicas needed (last truncated)
  assert N < 2**32, f"{N:,} exceeds u32 row space"
  SCALES = [s for s in (250_000, n_base, 250_000_000, N) if s <= N]
  log(f"base {n_base:,}; target {N:,} points = {R} replicas "
      f"({100 * N / 2**32:.1f}% of u32); scales {SCALES}")

  # --------------------------------------------------- replica transforms
  hubs = rng.random((max(3, R // 20), 2))   # overlap attractors
  specs = []
  for r in range(R):
      if r == 0:
          specs.append(dict(identity=True))
      elif r == 1:
          specs.append(dict(s=3e-4, th=0.4, fx=1, cx=0.31, cy=0.62, jit=0.0))
      elif r == 2:
          specs.append(dict(s=0.08, th=0.0, fx=1, cx=-0.02, cy=-0.02, jit=1e-4))
      elif r == 3:
          specs.append(dict(s=0.08, th=0.0, fx=1, cx=1.02, cy=1.02, jit=1e-4))
      elif r == 4:
          specs.append(dict(s=0.5, th=0.0, fx=1, cx=0.5, cy=0.5, jit=0.0, flat=True))
      else:
          s = float(np.exp(rng.uniform(np.log(0.02), np.log(0.55))))
          c = (hubs[rng.integers(len(hubs))] + rng.normal(0, 0.03, 2)
               if rng.random() < 0.45 else rng.random(2))
          specs.append(dict(s=s, th=float(rng.uniform(0, 2 * np.pi)),
                            fx=int(rng.choice([-1, 1])),
                            cx=float(c[0]), cy=float(c[1]),
                            jit=float(s * rng.uniform(0.002, 0.03))))

  # ---------------------------------------- geometry: key = morton<<32|eid
  # The residual is held in a separate entity-indexed array rather than packed into the sort
  # key: the key is already a full u64 and widening the sort to 96 bits would cost far more
  # than the one gather this needs at write time. 4 bytes per point.
  key = np.empty(N, dtype=np.uint64)
  resid = np.empty(N, dtype=np.uint32)
  for r, sp in enumerate(specs):
      lo = r * n_base
      hi = min(lo + n_base, N)
      n_r = hi - lo
      if sp.get("identity"):
          qx, qy = base_qx[:n_r], base_qy[:n_r]
      else:
          th, s, fx = sp["th"], sp["s"], sp["fx"]
          x = (bx[:n_r] * fx) * np.cos(th) - by[:n_r] * np.sin(th)
          y = (bx[:n_r] * fx) * np.sin(th) + by[:n_r] * np.cos(th)
          if sp.get("flat"):
              y = y * 1e-4
          x = x * s + sp["cx"]; y = y * s + sp["cy"]
          if sp["jit"]:
              x += rng.normal(0, sp["jit"], n_r)
              y += rng.normal(0, sp["jit"], n_r)
          qx, qy = quantise32(x), quantise32(y)
          del x, y
      m, res = split_code(qx, qy)
      key[lo:hi] = (m << np.uint64(32)) | np.arange(lo, hi, dtype=np.uint64)
      resid[lo:hi] = res
      del m, res
      if r % 50 == 0:
          log(f"  replica {r}/{R}")
  del bx, by, base_qx, base_qy
  log("keys built; sorting in place")
  key.sort()                                # introsort, in place, no buffer
  log("sorted; writing geometry")

  geo_f = out / "geometry.parquet"
  # `residual` gets no delta encoding: it is the low bits of a coordinate, so it is
  # high-entropy and near-incompressible by construction — delta packing would cost CPU to
  # store the same bytes. `morton` still deltas well, the rows being sorted by it.
  schema = pa.schema([("entity_id", pa.uint32()), ("morton", pa.uint32()),
                      ("residual", pa.uint32()),
                      ("row_id", pa.uint32()), ("priority", pa.uint16())])
  w = pq.ParquetWriter(geo_f, schema, compression="zstd", use_dictionary=False,
                       column_encoding={"morton": "DELTA_BINARY_PACKED",
                                        "row_id": "DELTA_BINARY_PACKED",
                                        "entity_id": "DELTA_BINARY_PACKED"})
  for lo in range(0, N, CHUNK):
      k = key[lo:lo + CHUNK]
      eid = (k & np.uint64(0xFFFFFFFF)).astype(np.uint32)
      w.write_table(pa.table({
          "entity_id": pa.array(eid, pa.uint32()),
          "morton": pa.array((k >> np.uint64(32)).astype(np.uint32), pa.uint32()),
          # Gathered by entity id: the rows are in Morton order here, the residual array is
          # in entity order.
          "residual": pa.array(resid[eid], pa.uint32()),
          "row_id": pa.array(np.arange(lo, min(lo + CHUNK, N), dtype=np.uint32), pa.uint32()),
          "priority": pa.array((splitmix(eid.astype(np.uint64)) >> np.uint64(48)
                                ).astype(np.uint16), pa.uint16()),
      }, schema=schema))
  w.close()
  log(f"wrote {geo_f} ({geo_f.stat().st_size / 1e9:.2f} GB)")

  # ------------------------------------------------------- structure report
  print("\n--- composite structure (full corpus) ---")
  for d in (4, 6, 8, 10, 12):
      shift = np.uint64(64 - 2 * d)         # morton occupies the key's high 32
      counts = np.zeros(4**d, dtype=np.int64)
      for lo in range(0, N, CHUNK):
          counts += np.bincount(key[lo:lo + CHUNK] >> shift, minlength=4**d)
      occ = counts[counts > 0]
      print(f"  depth {d:>2} ({4**d:>12,} tiles): {len(occ):>10,} occupied, "
            f"median {int(np.median(occ)):>8,}, p99 {int(np.percentile(occ, 99)):>9,}, "
            f"max {occ.max():>10,}")
      del counts
  # Both figures come out of one chunked pass. `np.diff(key >> 32)` over the whole array is two
  # further 8 GB temporaries at 10^9, on top of the 12 GB `key` is already holding — which fits
  # the 47 GiB machine this was written against and is an OOM kill on a smaller one, *after* the
  # geometry has been written and *before* scales.json has, which is the worst place to lose.
  #
  # `prev`/`run` carry the cell run across the chunk boundary, so both numbers are exact rather
  # than per-chunk: a run spanning a boundary is counted once and measured at its full length.
  distinct = 0
  maxcell = 0
  prev = None                               # last cell of the previous chunk
  run = 0                                   # length of the run it ended in
  for lo in range(0, N, CHUNK):             # sorted, so cell runs are local
      m = key[lo:lo + CHUNK] >> np.uint64(32)
      lens = np.diff(np.concatenate(([0], np.flatnonzero(np.diff(m)) + 1, [len(m)])))
      distinct += len(lens)
      if prev is not None and int(m[0]) == prev:
          distinct -= 1                     # the run continues rather than starting
          lens[0] += run
      maxcell = max(maxcell, int(lens.max()))
      prev, run = int(m[-1]), int(lens[-1])
  print(f"  distinct Morton cells: {distinct:,} / {N:,} ({100 * distinct / N:.1f}%); "
        f"max points sharing a cell: {maxcell:,}")

  print("\n--- per-scale row counts (prefix corpora) ---")
  scale_rows = {}
  for s in SCALES:
      c = 0
      for lo in range(0, N, CHUNK):
          c += int(((key[lo:lo + CHUNK] & np.uint64(0xFFFFFFFF)) < s).sum())
      scale_rows[s] = c
      print(f"  entity_id < {s:>13,}  ->  {c:>13,} rows")
  del key

# ---------------------------------------------------------------- pairs
pairs_info = {}
if not args.skip_pairs:
    pt = pq.read_table(args.pairs)
    p_ent = pt.column("entity_id").to_numpy().astype(np.int64)
    p_tid = pt.column("term_id").to_numpy()
    del pt
    n_tb = int(p_tid.max()) + 1
    is_glob = (np.arange(n_tb) % 100) < int(round(args.global_frac * 100))
    n_g = int(is_glob.sum()); n_l = n_tb - n_g
    gmap = np.zeros(n_tb, dtype=np.int64); gmap[is_glob] = np.arange(n_g)
    lmap = np.zeros(n_tb, dtype=np.int64); lmap[~is_glob] = np.arange(n_l)
    glob_sel = is_glob[p_tid]
    base_tid = np.where(glob_sel, gmap[p_tid], lmap[p_tid])
    # Sorting by (term_id, entity_id) makes term_id delta to zero and entity_id
    # to small gaps: ~3.8x smaller and ~3x faster to read than unsorted. One
    # permutation serves every replica, because local ids differ only by a
    # constant offset (order-preserving) and globals always sort below them.
    _key = np.where(glob_sel, base_tid, n_g + base_tid).astype(np.int64)
    _o = np.lexsort((p_ent, _key))
    p_ent, base_tid, glob_sel = p_ent[_o], base_tid[_o], glob_sel[_o]
    pdir = out / "pairs"; pdir.mkdir(exist_ok=True)
    pairs_f = pdir / f"{args.pairs_only or 'default'}.pairs.parquet"
    ps = pa.schema([("entity_id", pa.uint32()), ("term_id", pa.uint32())])
    w = pq.ParquetWriter(pairs_f, ps, compression="zstd", use_dictionary=False,
                         column_encoding={"entity_id": "DELTA_BINARY_PACKED",
                                          "term_id": "DELTA_BINARY_PACKED"})
    tot = 0
    for r in range(R):
        ent = p_ent + r * n_base
        m = ent < N                       # last replica is truncated
        if not m.any():
            continue
        tid = np.where(glob_sel, base_tid, n_g + r * n_l + base_tid)
        w.write_table(pa.table({
            "entity_id": pa.array(ent[m].astype(np.uint32), pa.uint32()),
            "term_id": pa.array(tid[m].astype(np.uint32), pa.uint32())}, schema=ps))
        tot += int(m.sum())
        if r % 100 == 0:
            log(f"  pairs replica {r}/{R}")
    w.close()
    pairs_info = dict(pairs=tot, terms=n_g + R * n_l, global_terms=n_g,
                      local_terms_per_replica=n_l)
    log(f"wrote {pairs_f} ({pairs_f.stat().st_size / 1e9:.2f} GB): {tot:,} pairs, "
        f"{n_g + R * n_l:,} terms ({n_g} global, {R}x{n_l} local)")

manifest_f = (out / "pairs" / f"{args.pairs_only}.json") if args.pairs_only \
    else (out / "scales.json")
manifest_f.write_text(json.dumps({
    "points": N, "replicas": R, "base_points": n_base, "seed": args.seed,
    "global_frac": args.global_frac,
    "scales": [{"name": f"{s:,}", "entity_id_limit": s, "rows": scale_rows[s]}
               for s in SCALES],
    "config": args.pairs_only,
    "row_order": "geometry.parquet is sorted by (morton, entity_id); a scale's "
                 "row_id is the running position among rows with entity_id < limit",
    "real_data": {"entity_id_limit": n_base,
                  "note": "replica 0 uses the hashed geometry artifact's coordinates "
                          "verbatim, re-quantised here; its Morton codes are not that "
                          "artifact's `morton` column"},
    **pairs_info,
}, indent=2) + "\n")
log(f"wrote {manifest_f}")
