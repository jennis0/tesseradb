"""multiview — a cheap fixture exercising every shape `docs/design/views.md` r6 adds.

Not a rung on the dataset ladder: it measures nothing and is not part of the ingest campaign.
Its only job is to be a `corpus.toml` a two-view build can be pointed at once one exists — one
plain view, two view groups (one sharing the other's views, in each of the two roster forms), a
constant and a group-scoped attribute, and an unscoped and a scoped layer — small enough to
prepare and validate in seconds, at a default of about 100,000 rows total across every file.

**Positions are a mix of real-derived and synthetic, and this is the whole of what "real" means
here.** No network call and no staged dataset is touched. Instead this script samples entity rows
from `data/ladder/geonames/points.parquet` — the GeoNames rung's own **built** output, already on
local disk from an earlier run of `test_corpora/geonames/prepare.py` and `tessera build` — and
carries three of its columns straight through:

- `world` and `quarter_alt`'s positions are that sample's `x`/`y`, **un-projected back to WGS84
  degrees** through `test_corpora.common.projection.unproject` (the frame is `web_mercator`/`unit`,
  the same one `frame.json` on that rung records), so what this script emits is real longitude and
  latitude that happens to have made a round trip through the projection rather than synthetic
  coordinates.
- `importance` is that sample's `population`, and `kind` is its `feature_class` — both carried
  through unchanged.

Everything else is synthetic and seeded, and is labelled so throughout this file and the README:
`quarter`'s four per-quarter layouts are an abstract embedding with no geographic meaning (a base
2D point per entity, hashed deterministically from its id, put through a different seeded
rotation/scale/translate per quarter — genuinely different layouts, reproducible without carrying
RNG state across files); `sentiment` is drawn from a seeded normal; the four entity-overlap
buckets and which points carry no access term are seeded coin flips.

If `data/ladder/geonames/points.parquet` is not present — a fresh checkout that has never run the
GeoNames rung — this script refuses rather than downloading or synthesising a substitute; run
`python -m test_corpora.geonames.prepare` (or point `--geonames-points` at any points parquet
carrying the same five columns) first.
"""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

from ..common import projection
from ..common.paths import ladder

RUNG = "multiview"

#: Default total row count across every file is ~4.7x this, by construction (see `main`'s
#: docstring maths) — n_total * SCALE_DEFAULT ~= 21,300 entities gives ~100,000 rows.
N_TOTAL_DEFAULT = 21_300

SEED = 20260830

QUARTERS = ["2026-Q1", "2026-Q2", "2026-Q3", "2026-Q4"]

#: GeoNames' own nine feature classes (`featureCodes_en.txt`'s readme, restated in
#: `test_corpora/geonames/prepare.py`) — reused here as `kind`'s vocabulary because the sampled
#: rows' `feature_class` column is drawn from exactly this set.
FEATURE_CLASSES = {
    "A": "Country, state, region",
    "H": "Stream, lake, other water body",
    "L": "Park, area, other region",
    "P": "City, village, other populated place",
    "R": "Road, railroad",
    "S": "Spot, building, farm",
    "T": "Mountain, hill, rock",
    "U": "Undersea feature",
    "V": "Forest, heath, other vegetation",
}


def _splitmix64(state: np.ndarray) -> np.ndarray:
    """A deterministic hash over `uint64` arrays — no RNG state to carry between files.

    Positions for one entity have to agree wherever they are derived independently (the quarter
    layout, the quarter_alt jitter), so they are pure functions of `(entity_id, salt)` rather than
    draws from a stream whose order would have to be reproduced exactly.
    """
    x = (state + np.uint64(0x9E3779B97F4A7C15)).astype(np.uint64)
    z = x
    z = (z ^ (z >> np.uint64(30))) * np.uint64(0xBF58476D1CE4E5B9)
    z = (z ^ (z >> np.uint64(27))) * np.uint64(0x94D049BB133111EB)
    z = z ^ (z >> np.uint64(31))
    return z


def hashed_unit(entity_id: np.ndarray, salt: int) -> np.ndarray:
    """One deterministic float in [0, 1) per entity id, keyed by `salt`."""
    mixed = (entity_id.astype(np.uint64) ^ np.uint64(salt)) & np.uint64(0xFFFFFFFFFFFFFFFF)
    z = _splitmix64(mixed)
    return (z >> np.uint64(11)).astype(np.float64) / float(1 << 53)


def hashed_normal(entity_id: np.ndarray, salt: int) -> np.ndarray:
    """A standard normal per entity id, via Box-Muller over two independent hashed uniforms."""
    u1 = np.clip(hashed_unit(entity_id, salt), 1e-12, 1.0)
    u2 = hashed_unit(entity_id, salt + 1)
    return np.sqrt(-2.0 * np.log(u1)) * np.cos(2.0 * np.pi * u2)


def base_layout(entity_id: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """The one abstract 2D point per entity that every quarter's layout is a transform of."""
    x = (hashed_unit(entity_id, 0x51A1) - 0.5) * 60.0
    y = (hashed_unit(entity_id, 0x51A2) - 0.5) * 60.0
    return x, y


def quarter_layout(entity_id: np.ndarray, quarter_idx: int) -> tuple[np.ndarray, np.ndarray]:
    """A per-quarter rotate/scale/translate of the base layout, plus small per-entity jitter.

    Genuinely different per quarter — the point of `quarter` existing as a group at all — and
    still a pure function of `(entity_id, quarter_idx)`, so the same entity's Q1 and Q3 positions
    can each be recomputed from nothing but its id.
    """
    x, y = base_layout(entity_id)
    theta = quarter_idx * (np.pi / 6.0)
    scale = 1.0 + 0.12 * quarter_idx
    tx, ty = quarter_idx * 3.0, -quarter_idx * 2.0
    jx = (hashed_unit(entity_id, 0x9100 + quarter_idx) - 0.5) * 1.5
    jy = (hashed_unit(entity_id, 0x9200 + quarter_idx) - 0.5) * 1.5
    xr = scale * (np.cos(theta) * x - np.sin(theta) * y) + tx + jx
    yr = scale * (np.sin(theta) * x + np.cos(theta) * y) + ty + jy
    return np.clip(xr, -39.5, 39.5), np.clip(yr, -39.5, 39.5)


def load_geonames_sample(points_path: Path, n_total: int, rng: np.random.Generator) -> pa.Table:
    """`n_total` rows sampled without replacement from a built GeoNames rung's own points file."""
    if not points_path.exists():
        raise FileNotFoundError(
            f"{points_path} not found — this fixture derives its real-ish structure from a "
            "built GeoNames rung's own output rather than downloading anything. Run "
            "`python -m test_corpora.geonames.prepare` first, or pass --geonames-points."
        )
    columns = ["entity_id", "x", "y", "country", "feature_class", "population"]
    table = pq.read_table(points_path, columns=columns)
    n_available = table.num_rows
    if n_total > n_available:
        raise ValueError(f"asked for {n_total} entities but {points_path} carries only {n_available}")
    idx = rng.choice(n_available, size=n_total, replace=False)
    idx.sort()  # a sorted take is the cheap one, and order carries no meaning here
    return table.take(pa.array(idx))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--scale", type=float, default=1.0, help="multiplies the default ~21,300 entities")
    ap.add_argument("--seed", type=int, default=SEED)
    ap.add_argument(
        "--geonames-points",
        type=Path,
        default=None,
        help="override the GeoNames points parquet sampled for real-ish structure "
        "(default: data/ladder/geonames/points.parquet)",
    )
    args = ap.parse_args()

    n_total = max(200, int(round(N_TOTAL_DEFAULT * args.scale)))
    rng = np.random.default_rng(args.seed)

    repo_root = Path(__file__).resolve().parents[2]
    points_path = args.geonames_points or (repo_root / "data" / "ladder" / "geonames" / "points.parquet")

    out = ladder(RUNG)

    print(f"multiview: sampling {n_total:,} entities from {points_path}")
    sample = load_geonames_sample(points_path, n_total, rng)
    entity_id = sample["entity_id"].to_numpy()
    src_x = sample["x"].to_numpy()
    src_y = sample["y"].to_numpy()
    country = sample["country"].to_pylist()
    feature_class = sample["feature_class"].to_pylist()
    population = sample["population"].to_numpy()

    # --- real-derived geometry: un-project the GeoNames sample's own [0,1] Web Mercator position
    # back to WGS84 degrees. Scalar per row (projection.unproject is the reference implementation
    # and is deliberately not duplicated here) — n_total rows, so this is well under a second.
    lon = np.empty(n_total, dtype=np.float64)
    lat = np.empty(n_total, dtype=np.float64)
    for i in range(n_total):
        lon[i], lat[i] = projection.unproject(float(src_x[i]), float(src_y[i]), frame="unit")

    # --- the synthetic per-point compartment scheme, following the Overture rung's pattern: the
    # point's own country stands in for an access term. ~4% carry none, to exercise the default.
    has_access = hashed_unit(entity_id, 0xACCE) >= 0.04
    access = [c if (has_access[i] and c) else None for i, c in enumerate(country)]

    # --- the four entity-overlap buckets (spec §7's join rule, spec §4's overlap cases):
    #   A (15%) — world, and every quarter
    #   B (15%) — world, and exactly one quarter
    #   C (30%) — world only, never a quarter
    #   D (40%) — never world; exactly two quarters
    bucket_roll = hashed_unit(entity_id, 0xB0C4)
    is_a = bucket_roll < 0.15
    is_b = (bucket_roll >= 0.15) & (bucket_roll < 0.30)
    is_c = (bucket_roll >= 0.30) & (bucket_roll < 0.60)
    is_d = bucket_roll >= 0.60

    in_world = is_a | is_b | is_c

    # B's single quarter, and D's pair, both a deterministic hashed choice.
    b_quarter = (hashed_unit(entity_id, 0xB1) * 4.0).astype(np.int64).clip(0, 3)
    d_first = (hashed_unit(entity_id, 0xD1) * 4.0).astype(np.int64).clip(0, 3)
    d_gap = 1 + (hashed_unit(entity_id, 0xD2) * 3.0).astype(np.int64).clip(0, 2)  # 1..3
    d_second = (d_first + d_gap) % 4

    in_quarter = np.zeros((4, n_total), dtype=bool)
    for q in range(4):
        in_quarter[q] |= is_a
        in_quarter[q] |= is_b & (b_quarter == q)
        in_quarter[q] |= is_d & ((d_first == q) | (d_second == q))

    print(
        f"  buckets: A(full)={int(is_a.sum()):,} B(world+1)={int(is_b.sum()):,} "
        f"C(world-only)={int(is_c.sum()):,} D(quarters-only)={int(is_d.sum()):,}"
    )
    print(f"  world rows: {int(in_world.sum()):,}")
    for q in range(4):
        print(f"  quarter {QUARTERS[q]} rows: {int(in_quarter[q].sum()):,}")

    # === world.parquet ===============================================================
    world_mask = in_world
    world_table = pa.table(
        {
            "entity_id": pa.array(entity_id[world_mask]),
            "lon": pa.array(lon[world_mask]),
            "lat": pa.array(lat[world_mask]),
            "access": pa.array([access[i] for i in np.flatnonzero(world_mask)], type=pa.string()),
        }
    )
    pq.write_table(world_table, out / "world.parquet")

    # === quarter-2026-Qn.parquet (form A) =============================================
    for q in range(4):
        mask = in_quarter[q]
        ids = entity_id[mask]
        x, y = quarter_layout(ids, q)
        # sentiment: present for ~85% of a quarter's rows, null otherwise — presence bitmap.
        has_sentiment = hashed_unit(ids, 0x5E17 + q) < 0.85
        sentiment_vals = np.clip(hashed_normal(ids, 0x5E20 + q) * 0.3, -1.0, 1.0).astype(np.float32)
        sentiment = pa.array(
            [float(sentiment_vals[i]) if has_sentiment[i] else None for i in range(len(ids))],
            type=pa.float32(),
        )
        table = pa.table(
            {
                "entity_id": pa.array(ids),
                "x": pa.array(x),
                "y": pa.array(y),
                "access": pa.array([access[i] for i in np.flatnonzero(mask)], type=pa.string()),
                "sentiment": sentiment,
            }
        )
        pq.write_table(table, out / f"quarter-{QUARTERS[q]}.parquet")

    # === quarter-alt.parquet (form B: one file, a `quarter` discriminator column) =====
    # Real-derived positions again, distinct from `world`'s: a small deterministic per-quarter
    # geographic nudge (~0.05-0.2 degrees) on the same underlying real lon/lat, so the two groups'
    # geometry differs without inventing a second unrelated dataset.
    alt_rows = {"entity_id": [], "quarter": [], "lon": [], "lat": [], "access": []}
    for q in range(4):
        mask = in_quarter[q]
        idxs = np.flatnonzero(mask)
        ids = entity_id[mask]
        dlon = (hashed_unit(ids, 0xA10 + q) - 0.5) * 0.4
        dlat = (hashed_unit(ids, 0xA20 + q) - 0.5) * 0.4
        alt_lon = np.clip(lon[mask] + dlon, -180.0, 180.0)
        alt_lat = np.clip(lat[mask] + dlat, -85.0511287798066, 85.0511287798066)
        alt_rows["entity_id"].extend(ids.tolist())
        alt_rows["quarter"].extend([QUARTERS[q]] * len(ids))
        alt_rows["lon"].extend(alt_lon.tolist())
        alt_rows["lat"].extend(alt_lat.tolist())
        alt_rows["access"].extend(access[i] for i in idxs)
    alt_table = pa.table(
        {
            "entity_id": pa.array(alt_rows["entity_id"]),
            "quarter": pa.array(alt_rows["quarter"], type=pa.string()),
            "lon": pa.array(alt_rows["lon"]),
            "lat": pa.array(alt_rows["lat"]),
            "access": pa.array(alt_rows["access"], type=pa.string()),
        }
    )
    pq.write_table(alt_table, out / "quarter-alt.parquet")

    # === vocab-kind.parquet, attrs-constant.parquet ===================================
    classes_present = sorted(set(c for c in feature_class if c))
    vocab_table = pa.table(
        {
            "key": pa.array(classes_present, type=pa.string()),
            "code": pa.array(range(1, len(classes_present) + 1), type=pa.int64()),
            "title": pa.array([FEATURE_CLASSES.get(c, c) for c in classes_present], type=pa.string()),
        }
    )
    pq.write_table(vocab_table, out / "vocab-kind.parquet")

    attrs_table = pa.table(
        {
            "entity_id": pa.array(entity_id),
            "importance": pa.array(population),
            "kind": pa.array(feature_class, type=pa.string()),
        }
    )
    pq.write_table(attrs_table, out / "attrs-constant.parquet")

    # === collections.parquet — the unscoped layer, drawn on ["world", "quarter"] ======
    # Six curated collections, membership drawn irrespective of bucket — some members will be
    # absent from a given view (a C-bucket entity under "quarter", a D-bucket one under "world"),
    # which is the ordinary case a layer over a group has to tolerate.
    collection_names = ["frontier", "core", "outliers", "review", "watchlist", "archive"]
    coll_rows = {"key": [], "contents": [], "members": [], "access": []}
    for i, name in enumerate(collection_names):
        pick = hashed_unit(entity_id, 0xC0 + i) < 0.08  # ~8% of the whole population each
        members = entity_id[pick].tolist()
        coll_rows["key"].append(name)
        coll_rows["contents"].append([f"{name}-tag"])
        coll_rows["members"].append(members)
        coll_rows["access"].append(None if i % 3 else "public")
    coll_table = pa.table(
        {
            "key": pa.array(coll_rows["key"], type=pa.string()),
            "contents": pa.array(coll_rows["contents"], type=pa.list_(pa.string())),
            "members": pa.array(coll_rows["members"], type=pa.list_(pa.int64())),
            "access": pa.array(coll_rows["access"], type=pa.string()),
        }
    )
    pq.write_table(coll_table, out / "collections.parquet")
    print(f"  collections: {[len(m) for m in coll_rows['members']]} members each")

    # === clusters-quarter.parquet — the scoped layer, a different set per quarter =====
    # Clustered by angle from the quarter's own layout centroid — trivially different per quarter
    # because the coordinates themselves are (`quarter_layout`), which is the property this layer
    # exists to exercise.
    N_CLUSTERS = 6
    clus_rows = {"key": [], "quarter": [], "contents": [], "members": []}
    for q in range(4):
        mask = in_quarter[q]
        ids = entity_id[mask]
        x, y = quarter_layout(ids, q)
        angle = np.arctan2(y, x)
        cluster_idx = ((angle + np.pi) / (2 * np.pi) * N_CLUSTERS).astype(np.int64).clip(0, N_CLUSTERS - 1)
        for c in range(N_CLUSTERS):
            members = ids[cluster_idx == c].tolist()
            clus_rows["key"].append(f"{QUARTERS[q]}-cluster-{c}")
            clus_rows["quarter"].append(QUARTERS[q])
            clus_rows["contents"].append([f"cluster {c}"])
            clus_rows["members"].append(members)
    clus_table = pa.table(
        {
            "key": pa.array(clus_rows["key"], type=pa.string()),
            "quarter": pa.array(clus_rows["quarter"], type=pa.string()),
            "contents": pa.array(clus_rows["contents"], type=pa.list_(pa.string())),
            "members": pa.array(clus_rows["members"], type=pa.list_(pa.int64())),
        }
    )
    pq.write_table(clus_table, out / "clusters-quarter.parquet")

    # === the declaration, copied beside the data it declares ==========================
    here = Path(__file__).parent
    shutil.copy(here / "corpus.toml", out / "corpus.toml")

    print("\nwrote:")
    for path in sorted(out.glob("*.parquet")):
        print(f"  {path.name:<32} {path.stat().st_size / 1e3:>9.1f} kB  ({pq.read_metadata(path).num_rows:,} rows)")
    print(f"  corpus.toml")
    print(f"\nnext:\n  python -m test_corpora.multiview.validate")


if __name__ == "__main__":
    main()
