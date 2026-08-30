import os, pyarrow as pa, pyarrow.parquet as pq
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "synth")
os.makedirs(D, exist_ok=True)
N = 200_000
COVERED = 160_000

def w(name, arrays, schema):
    pq.write_table(pa.table(arrays, schema=schema), os.path.join(D, name))

# ---- points ----------------------------------------------------------------
ids = list(range(N))
w("points.parquet",
  [pa.array(ids, pa.uint64()),
   pa.array([((e * 7919) % 1_000_000) / 1000.0 for e in ids], pa.float64()),
   pa.array([((e * 5023) % 1_000_000) / 1000.0 for e in ids], pa.float64())],
  pa.schema([("entity_id", pa.uint64()), ("x", pa.float64()), ("y", pa.float64())]))

# ---- the nested tree -------------------------------------------------------
leaves = [f"t-{b}{i}" for b in "ab" for i in range(4)]
tree_rows = [("t-root", None)] + [("t-a", "t-root"), ("t-b", "t-root")] \
          + [(k, "t-" + k[2]) for k in leaves]
w("tree.parquet",
  [pa.array([0] * len(tree_rows), pa.uint32()),
   pa.array([k for k, _ in tree_rows], pa.string()),
   pa.array([p for _, p in tree_rows], pa.string())],
  pa.schema([("level", pa.uint32()), ("key", pa.string()), ("parent", pa.string())]))

block = COVERED // 8
tree_members = []
for i, key in enumerate(leaves):
    for e in range(i * block, (i + 1) * block):
        tree_members.append((key, None, e))
        tree_members.append(("t-" + key[2], None, e))
        tree_members.append(("t-root", None, e))
# a duplicate member entry, which the containment report counts as two
tree_members.append(("t-root", None, 0))
tree_members.append(("t-a", None, 0))
tree_members.append(("t-a0", None, 0))
# Three reports the bundle carries, each needing a row of its own to be non-zero: a key no
# artifacts source declares is minted, a null key is an unclustered row, and a leaf member the
# parent does not hold is a containment violation on a layer that does not prune.
tree_members.append(("t-minted", None, 1))
tree_members.append((None, None, 2))
tree_members.append(("t-a0", None, 199_999))

def members(name, rows):
    w(name,
      [pa.array([k for k, _, _ in rows], pa.string()),
       pa.array([r for _, r, _ in rows], pa.uint32()),
       pa.array([e for _, _, e in rows], pa.uint64())],
      pa.schema([("key", pa.string()), ("rank", pa.uint32()), ("entity", pa.uint64())]))

members("tree_members.parquet", tree_members)

# ---- the tiered ladder -----------------------------------------------------
ladder_rows = [(0, f"l0-{i}") for i in range(2)] \
            + [(1, f"l1-{i}") for i in range(8)] \
            + [(2, f"l2-{i}") for i in range(32)]
w("ladder.parquet",
  [pa.array([l for l, _ in ladder_rows], pa.uint32()),
   pa.array([k for _, k in ladder_rows], pa.string())],
  pa.schema([("level", pa.uint32()), ("key", pa.string())]))

leaf = COVERED // 32
# **The ladder's member source addresses by position**, the way a tiered layer's does: the `key`
# list's index is the level, so one row carries a point's whole lineage.
ladder_entities = list(range(COVERED))
ladder_keys = [[f"l0-{e // leaf // 16}", f"l1-{e // leaf // 4}", f"l2-{e // leaf}"]
               for e in ladder_entities]
w("ladder_members.parquet",
  [pa.array(ladder_entities, pa.uint64()),
   pa.array(ladder_keys, pa.list_(pa.string()))],
  pa.schema([("entity", pa.uint64()), ("key", pa.list_(pa.string()))]))
ladder_members = ladder_keys

# ---- the label layer, attached into the tree -------------------------------
labels = [("x-0", "t-a"), ("x-1", "t-b0")]
w("labels.parquet",
  [pa.array([k for k, _ in labels], pa.string()),
   pa.array([[["the whole cluster"], ["the visible part"]] for _ in labels],
            pa.list_(pa.list_(pa.string()))),
   pa.array(["tree/t"] * len(labels), pa.string()),
   pa.array([a for _, a in labels], pa.string())],
  pa.schema([("key", pa.string()),
             ("contents", pa.list_(pa.list_(pa.string()))),
             ("attached_layer", pa.string()),
             ("attached_key", pa.string())]))

label_members = []
for key, span in (("x-0", range(0, 4 * block)), ("x-1", range(4 * block, 5 * block))):
    for e in span:
        label_members.append((key, None, e))
        label_members.append((key, 0, e))
        if e % 3 == 0:
            label_members.append((key, 1, e))
members("labels_members.parquet", label_members)

# ---- a membership spelled by exclusion -------------------------------------
w("curated.parquet",
  [pa.array(["c-0", "c-1"], pa.string()),
   pa.array([[5, 7, 9], list(range(1000, 200_000))], pa.list_(pa.uint64()))],
  pa.schema([("key", pa.string()), ("excluding", pa.list_(pa.uint64()))]))
print("wrote", sorted(os.listdir(D)))
print("tree pairs", len(tree_members), "ladder pairs", 3 * len(ladder_members),
      "label pairs", len(label_members))
