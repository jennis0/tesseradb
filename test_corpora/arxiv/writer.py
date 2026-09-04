"""The build inputs, in the shapes `tessera build` reads them (`annotation-write-cycle.md` §6.1).

**One file per layer, one row per artifact.** A row's `contents` is its ranking — best first, one
entry per rank, one value per supplied kind — and is null where the artifact carries none. No row
names its own layer: the layer's `source` does, and the discriminator column is gone, so there is
no way for a layer to ingest another's rows. `attached_*` is the edge a *label* artifact carries to
the cluster it hangs from; the rung's clusterings carry their own titles as content and use none.

**Members are named by source entity id** — the `entity_id` of the points file — and resolved
through the build's own assignment. An id the build did not assign refuses the build rather than
being dropped: a dropped member moves both the count a viewer is shown and the size a proportional
criterion divides by, quietly, in the direction of hiding the cluster.

Both stages of the rung write through this class, which is why it is here rather than in
`prepare.py`: the optional Toponymy stage adds two layers to a directory the first stage wrote,
and the two must produce the same shapes.
"""

from __future__ import annotations

import collections
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq


def slug(layer: str) -> str:
    """The file a layer's rows go in — `clusters/kmeans` in `clusters-kmeans.parquet`."""
    return layer.replace("/", "-")


class ArtifactSet:
    """Artifacts and their members, accumulated per layer and written one file each."""

    def __init__(self):
        self.per_layer = collections.defaultdict(list)
        # layer -> (levels, keys, ranks, entities), the four parallel columns of a member file.
        self.member_rows = collections.defaultdict(lambda: ([], [], [], []))
        # Layers whose member file is written as it is produced rather than accumulated, and the
        # rows each has written so far — see the streaming path below.
        self._streams = {}
        self.streamed = collections.Counter()

    def artifact(self, layer, key, *, level=0, contents=None, attached=None, parent=None):
        """One artifact. `attached` is `(layer, level, key)` — a label on a levelled layer names
        the level its cluster sits on, and a flat one is level 0.

        `parent` is a key, a list of keys, or `None`. It is **written as a list whatever it was**
        (decision 0117): a `dag` layer's child names several parents, and the build reads a scalar
        as a list of one, so one column shape serves every kind and no caller has to know which
        spelling its layer wants.
        """
        if parent is None:
            parents = None
        elif isinstance(parent, str):
            parents = [parent]
        else:
            parents = list(parent)
        self.per_layer[layer].append(
            {
                "level": level,
                "key": key,
                "contents": contents,
                "attached_layer": attached[0] if attached else None,
                "attached_level": attached[1] if attached else None,
                "attached_key": attached[2] if attached else None,
                "parent": parents,
            }
        )

    def members(self, layer, key, rows, *, rank=None, level=0):
        """One row per `(artifact, entity)`. A null `rank` is the artifact's own membership;
        `rank = k` is the generating set of `contents[k]`."""
        assert layer not in self._streams, (
            f"{layer}: its member file is being streamed; rows added here would be dropped"
        )
        levels, keys, ranks, entities = self.member_rows[layer]
        for row in rows:
            levels.append(level)
            keys.append(key)
            ranks.append(rank)
            entities.append(int(row))

    def generating_sets(self, layer, key, pool, *, rng, sample, ranks=2, level=0):
        """One generating set per ranked content, drawn from an artifact's own membership.

        A generating set naming a non-member is a different object and the model refuses it, so
        the draw is from `pool` — which the caller has already written as the artifact's
        membership. **Each rank is generated from a third of the one above it**, so a lower rank is
        satisfiable by a narrower principal than the rank above, which is the point of ranking
        them: a viewer is served the first content whose set they hold entirely, or nothing.

        `ranks` must be the number of contents the artifact carries. A member row at a rank the
        artifact has no content for names a description that was never supplied, and the build
        refuses it.

        Returns the sample it drew, which is `contents[0]`'s generating set.
        """
        pool = np.asarray(pool)
        pick = pool if len(pool) <= sample else rng.choice(pool, sample, replace=False)
        for rank in range(ranks):
            self.members(layer, key, pick[: max(1, len(pick) // 3**rank)], rank=rank, level=level)
        return pick

    # ------------------------------------------------------------------ the streaming member path

    # The accumulating path above holds every member row in Python lists until `write`, which is
    # right for a clustering — one row per point per layer — and impossible for an ancestor-closed
    # hierarchy, whose member file is several rows per point per *ancestor*. Rung 3's MeSH layer
    # is ~1.7e9 rows at full scale (`dag-hierarchies.md` §8), so it is written as it is produced:
    # one `write_table` per staged chunk, straight through a `ParquetWriter` (which splits a
    # call at its own default row-group size, so a chunk is several groups), with `key`
    # dictionary-encoded because a descriptor name recurs once per member.
    #
    # A layer written this way declares its artifacts through `artifact` as any other does — there
    # are tens of thousands of those, not billions — and `write` then skips its member file, which
    # this path has already produced. The two are mutually exclusive per layer, and mixing them is
    # an assertion rather than a silently half-written file.

    MEMBER_SCHEMA = pa.schema(
        [
            pa.field("level", pa.uint32()),
            pa.field("key", pa.string()),
            pa.field("rank", pa.uint32()),
            pa.field("entity", pa.uint64()),
        ]
    )

    def stream_members(self, layer, out: Path, keys, entities, *, level=0, rank=None) -> int:
        """Append one row group of `(artifact, entity)` rows to the layer's member file.

        `keys` is an Arrow string array of artifact keys and `entities` a parallel array of source
        entity ids; both are already the flat, deduplicated shape the file wants. Returns the rows
        written. The file is opened on the first call and closed by `close_streams`.

        `rank` is the accumulating path's, and it is here because a layer whose *membership* is too
        large to accumulate still has generating sets that are not: rung 4's clustering streams
        10⁸ member rows and a few hundred ranked rows per artifact into the same file, and a layer
        may only be written one way. `None` — the default, and every earlier caller's behaviour —
        is the artifact's own membership.
        """
        assert not self.member_rows[layer][1], (
            f"{layer}: member rows were accumulated as well as streamed"
        )
        writer = self._streams.get(layer)
        if writer is None:
            writer = pq.ParquetWriter(
                out / f"{slug(layer)}-members.parquet",
                self.MEMBER_SCHEMA,
                compression="zstd",
                use_dictionary=["key"],
            )
            self._streams[layer] = writer
        n = len(keys)
        writer.write_table(
            pa.table(
                {
                    "level": pa.array(np.full(n, level, dtype=np.uint32), pa.uint32()),
                    "key": keys,
                    # A rank names the generating set of a ranked content; null is the artifact's
                    # own membership, which is what a streamed layer writes for all but a few
                    # hundred rows an artifact.
                    "rank": (
                        pa.nulls(n, pa.uint32())
                        if rank is None
                        else pa.array(np.full(n, rank, dtype=np.uint32), pa.uint32())
                    ),
                    "entity": pa.array(np.asarray(entities, dtype=np.uint64), pa.uint64()),
                },
                schema=self.MEMBER_SCHEMA,
            )
        )
        self.streamed[layer] += n
        return n

    def close_streams(self) -> None:
        """Close every streamed member file. Idempotent, and safe to call when none were opened."""
        for writer in self._streams.values():
            writer.close()
        self._streams.clear()

    # ------------------------------------------------------------------------------ writing out

    def write(self, out: Path, layers=None) -> tuple[int, int]:
        """Write one artifact file and one member file per layer. Returns `(artifacts, members)`.

        `layers` restricts the write to a subset, which is how the second stage adds its two
        layers to a directory the first stage wrote without rewriting the first stage's files.
        """
        chosen = list(self.per_layer) if layers is None else list(layers)
        artifact_rows = member_rows = 0

        for layer in chosen:
            rows = self.per_layer[layer]
            artifact_rows += len(rows)
            pq.write_table(
                pa.table(
                    {
                        "level": pa.array([r["level"] for r in rows], pa.uint32()),
                        "key": pa.array([r["key"] for r in rows], pa.string()),
                        "contents": pa.array(
                            [r["contents"] for r in rows], pa.list_(pa.list_(pa.string()))
                        ),
                        "attached_layer": pa.array(
                            [r["attached_layer"] for r in rows], pa.string()
                        ),
                        "attached_level": pa.array(
                            [r["attached_level"] for r in rows], pa.uint32()
                        ),
                        "attached_key": pa.array([r["attached_key"] for r in rows], pa.string()),
                        "parent": pa.array(
                            [r["parent"] for r in rows], pa.list_(pa.string())
                        ),
                    }
                ),
                out / f"{slug(layer)}.parquet",
            )

        for layer in chosen:
            # A streamed layer's member file is already on disk; writing an empty one here would
            # replace it with nothing.
            if layer in self.streamed:
                member_rows += self.streamed[layer]
                continue
            levels, keys, ranks, entities = self.member_rows[layer]
            member_rows += len(keys)
            pq.write_table(
                pa.table(
                    {
                        "level": pa.array(levels, pa.uint32()),
                        "key": pa.array(keys, pa.string()),
                        "rank": pa.array(ranks, pa.uint32()),
                        "entity": pa.array(np.array(entities, dtype=np.uint64), pa.uint64()),
                    }
                ),
                out / f"{slug(layer)}-members.parquet",
            )

        return artifact_rows, member_rows

    # ------------------------------------------------------------------------------- the checks

    def check(self, n: int, roster_extra=frozenset()) -> None:
        """The properties the rest of the system depends on, asserted rather than assumed.

        Each one fails silently downstream if it is wrong, and each is cheaper to read here, where
        the line that produced it is in view, than in a build report. `roster_extra` carries the
        artifacts an *earlier* stage declared, so the second stage can check an attachment that
        crosses stages without re-reading the first stage's files.
        """
        roster = set(roster_extra) | {
            (layer, r["level"], r["key"]) for layer, rows in self.per_layer.items() for r in rows
        }

        for layer, (levels, keys, ranks, entities) in self.member_rows.items():
            if not keys:
                continue
            assert min(entities) >= 0 and max(entities) < n, (
                f"{layer}: a member is outside the corpus"
            )

        # Every label attaches to a cluster that exists — the build refuses a dangling edge, and a
        # layer declaring `depends_on` refuses an artifact that declares none.
        for layer, rows in self.per_layer.items():
            for r in rows:
                if r["attached_key"] is not None:
                    target = (r["attached_layer"], r["attached_level"], r["attached_key"])
                    assert target in roster, f"{layer}/{r['key']} attaches to a missing {target}"

        # Every member row names an artifact its layer declares, and every generating set is a
        # subset of its artifact's own membership.
        for layer, (levels, keys, ranks, entities) in self.member_rows.items():
            held = {}
            for level, key, rank, entity in zip(levels, keys, ranks, entities):
                assert (layer, level, key) in roster, (
                    f"{layer}: member row names an undeclared {key}"
                )
                held.setdefault((key, rank), set()).add(entity)
            for (key, rank), rows in held.items():
                if rank is None:
                    continue
                assert rows <= held[(key, None)], (
                    f"{layer}/{key} rank {rank} names a document it does not hold"
                )
