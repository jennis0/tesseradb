# A corpus shaped to exercise the one term the ordinary shapes do not: a single artifact holding
# half of every member pair in the build, beside a few thousand scattered small ones.
import os, numpy as np, pyarrow as pa, pyarrow.parquet as pq
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dom")
os.makedirs(D, exist_ok=True)
N = 20_000_000
FAN = 2048

e = np.arange(N, dtype=np.uint64)
# Scrambled positions, so entity order (signature-sorted) is a permutation of source order and a
# membership picked by `e mod FAN` is scattered in entity space rather than contiguous.
x = ((e * np.uint64(2654435761)) % np.uint64(1_000_000)).astype(np.float64) / 1000.0
y = ((e * np.uint64(40503)) % np.uint64(1_000_000)).astype(np.float64) / 1000.0
pq.write_table(pa.table({"entity_id": pa.array(e, pa.uint64()),
                         "x": pa.array(x, pa.float64()),
                         "y": pa.array(y, pa.float64())}), os.path.join(D, "points.parquet"))

# The ladder: level 0 is one artifact, level 1 is FAN of them.
rows = [(0, "root")] + [(1, f"c-{i}") for i in range(FAN)]
pq.write_table(pa.table({"level": pa.array([l for l, _ in rows], pa.uint32()),
                         "key": pa.array([k for _, k in rows], pa.string())}),
               os.path.join(D, "ladder.parquet"))

# One row per point, key list = [level 0, level 1] — so every point is a member pair at each level
# and the root holds exactly half of the build's pairs.
leaf = np.char.add("c-", (e % FAN).astype(np.int64).astype(str))
keys = np.empty(2 * N, dtype=object)
keys[0::2] = "root"
keys[1::2] = leaf
offsets = pa.array(np.arange(N + 1, dtype=np.int32) * 2, pa.int32())
values = pa.array(keys, pa.string())
pq.write_table(pa.table({"entity": pa.array(e, pa.uint64()),
                         "key": pa.ListArray.from_arrays(offsets, values)}),
               os.path.join(D, "ladder_members.parquet"))
print("points", N, "artifacts", 1 + FAN, "pairs", 2 * N, "dominant share 0.5")
