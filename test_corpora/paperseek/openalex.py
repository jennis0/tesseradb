"""OpenAlex's topic tree, the per-slice join against it, and the four-level layer it declares.

**What this is.** OpenAlex publishes a four-level classification of the scholarly record — 4
domains → 26 fields → 252 subfields → 4,516 topics — as its own entity tables, CC0, and assigns
every work a `primary_topic` in it. It is a strict containment hierarchy: a topic sits in exactly
one subfield, a subfield in one field, a field in one domain, and the counts above are the whole of
it. That is what makes this a `tiered` layer and not a `dag` one — rung 3's MeSH descriptors needed
the DAG because 30% of them name several parents; nothing here does.

**A work has one topic, so membership is not closed upward — it is *stated* at four levels.** MeSH
had to be closed upward because indexing assigns the most specific heading and a parent would
otherwise not contain its children. Here the parent chain of a work's topic is single and known, so
`write_layer` writes the four rows directly: a work under `T11045` is a member of that topic, of
subfield 3312, of field 33 and of domain 2. Four rows per work exactly — 4.1x10^8 over the corpus,
against MeSH's 1.7x10^9 — and containment holds by construction, which is what
`require_member_visibility = { count = 50 }` needs to guarantee roll-up (a child's masked count is
never larger than its parent's).

**The join is a sorted-array probe, not a hash.** `extract.py` writes the extract sorted by id, and
the id is the number after the `W` (both sides spell it `https://openalex.org/W…`), so a slice of a
million ids resolves in one `searchsorted` over a `uint64` array. That is the vectorised shape the
interface asks for and it is cheaper than an Arrow hash join, which would rebuild a hash table over
10^8 rows on every slice. What it is *not*, under any spelling, is a Python dict of 10^8 strings.

**Everything a work does not have is null, and nulls are counted rather than argued with.** A
PaperSeek id OpenAlex no longer carries resolves to nulls throughout; a work with no
`best_oa_location` carries no licence; a work whose `primary_topic` is null is a member of nothing.
`self.stats` accumulates all of it across calls so a caller can state the slice beside its figures.

**The licence is the corpus's compartment**, which is why the vocabulary is closed and a constant:
a work's label is its licence key, a work with no licence carries no label and is public, and a
closed vocabulary is what makes an unexpected value a refusal rather than a silent widening.
`LICENCES` is the measured set (see `README-openalex.md`); an extract carrying anything outside it
raises here rather than at the build.
"""

from __future__ import annotations

import glob
import json
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

try:  # the writer moves to `common/` when the other track lands; both spellings are one class
    from test_corpora.common.writer import ArtifactSet
except ImportError:  # pragma: no cover - whichever of the two is present
    from test_corpora.arxiv.writer import ArtifactSet

from ..common.paths import staged
from . import sources
from .extract import OPENALEX, VINTAGE, keys

def short(openalex_id: str) -> str:
    """`https://openalex.org/subfields/3312` → `3312`; `…/T11045` → `T11045`.

    The artifact key at every level, fixed by the interface: the part of the id after the last
    slash. Topic keys carry their `T`, the other three are bare numbers — which is how OpenAlex
    itself spells them — and keys are scoped per level, so field `33` and subfield `3312` never
    collide.
    """
    return openalex_id.rsplit("/", 1)[-1]


LAYER = "topics/openalex"

#: Level names, coarse to fine. The index is the declared `level`.
LEVELS = ("domain", "field", "subfield", "topic")

#: The closed licence vocabulary, in a fixed order: the ten values `best_oa_location.license`
#: takes across the 98,925,699 matched works, **most common first** and measured, not guessed
#: (`README-openalex.md` §3). Order is fixed because it is the term order of a `u8` vocabulary in
#: the declaration, and reordering it would renumber every point's label without changing a row.
#: `mit` is real and carries 62 works — a closed vocabulary that omitted it would refuse the
#: build over those 62, which is exactly what closed is for.
LICENCES = [
    "cc-by",
    "cc-by-nc-nd",
    "cc-by-nc",
    "other-oa",
    "cc-by-sa",
    "cc-by-nc-sa",
    "public-domain",
    "cc-by-nd",
    "publisher-specific-oa",
    "mit",
]

#: Members written per `stream_members` call, per level. Four levels x this is the peak row count
#: held at once, and a million-row slice of works is 4x10^6 member rows however it is cut.
BATCH = 1_000_000


class OpenAlex:
    """The topic tree, and the two operations a staged slice needs against it."""

    def __init__(self, extract_path: Path | None = None, dimensions: Path | None = None):
        # The dimension tables are a few MB in total, read whole.
        self.topics = self._tree(dimensions or staged(OPENALEX, VINTAGE) / "parquet")
        self._load(Path(extract_path or sources.staging() / "openalex-extract.parquet"))
        self.stats = {
            "rows": 0,
            "matched": 0,
            "unmatched": 0,
            "with_topic": 0,
            "with_licence": 0,
        }

    # -------------------------------------------------------------------------------- the tree

    @staticmethod
    def _read(base: Path, table: str, columns: list[str]) -> pa.Table:
        parts = sorted(glob.glob(f"{base / table}/*/part_*.parquet"))
        assert parts, f"no {table} dimension table under {base}"
        return pa.concat_tables([pq.ParquetFile(p).read(columns=columns) for p in parts])

    def _tree(self, base: Path) -> pa.Table:
        """The four dimension tables, flattened to one row per topic in ascending `T` order.

        The `topics` table already carries each topic's subfield, field and domain as structs, so
        the three coarser tables are read as the **check** rather than as the source: how many of
        each they publish must agree with what `topics` says, and a disagreement would mean the
        snapshot's own tables are inconsistent. Sorting by the topic number is what fixes the
        ordinal the rest of this module indexes by — the file order is the accident of which
        `updated_date` partition a topic last moved in.

        4,516 rows: small enough that the flattening is a list comprehension and clearer for it.
        """
        topics = self._read(base, "topics", ["id", "display_name", "subfield", "field", "domain"])
        rows = topics.to_pylist()
        rows.sort(key=lambda r: int(short(r["id"])[1:]))

        table = pa.table(
            {
                "topic_id": pa.array([short(r["id"]) for r in rows], pa.string()),
                "name": pa.array([r["display_name"] for r in rows], pa.string()),
                **{
                    column: pa.array(
                        [r[column]["display_name"] for r in rows], pa.string()
                    )
                    for column in ("subfield", "field", "domain")
                },
                **{
                    f"{column}_id": pa.array(
                        [short(r[column]["id"]) for r in rows], pa.string()
                    )
                    for column in ("subfield", "field", "domain")
                },
            }
        ).select(
            [
                "topic_id",
                "name",
                "subfield_id",
                "subfield",
                "field_id",
                "field",
                "domain_id",
                "domain",
            ]
        )

        for published, name in (("subfields", "subfield"), ("fields", "field"), ("domains", "domain")):
            declared = len(pc.unique(self._read(base, published, ["id"])["id"].combine_chunks()))
            assert declared == len(pc.unique(table[f"{name}_id"])), (
                f"the {published} table publishes {declared} of them and topics.{name} names "
                f"{len(pc.unique(table[f'{name}_id']))}"
            )
        return table

    # ----------------------------------------------------------------------------- the extract

    def _load(self, path: Path) -> None:
        """The extract, held once: a sorted `uint64` id array and the five columns beside it."""
        table = pq.read_table(path, read_dictionary=["type", "licence", "topic_id"])
        self._ids = np.asarray(table["id"].combine_chunks())
        assert np.all(np.diff(self._ids) > 0), (
            "the extract is not sorted ascending by id, which `resolve` probes it as; "
            "rerun `extract.py --combine`"
        )
        self._year = table["publication_year"].combine_chunks()
        self._type = table["type"].combine_chunks()
        self._is_oa = table["is_oa"].combine_chunks()
        self._licence = table["licence"].combine_chunks()

        held = {v for v in pc.unique(pc.cast(self._licence, pa.string())).to_pylist() if v}
        unknown = sorted(held - set(LICENCES))
        if unknown:
            raise ValueError(
                f"the extract carries licences outside the closed vocabulary: {unknown}. "
                "Add them to LICENCES (and to the declaration's [[vocabulary]]) — a closed "
                "vocabulary that does not name a term the corpus holds refuses the build."
            )

        # Topic ordinal per extract row: `index_in` against the tree, once, so `resolve` gathers
        # an int32 rather than joining strings.
        ordinal = pc.index_in(
            pc.cast(table["topic_id"].combine_chunks(), pa.string()),
            value_set=self.topics["topic_id"],
        )
        self.unknown_topics = int(
            pc.sum(
                pc.and_(pc.is_null(ordinal), pc.is_valid(table["topic_id"].combine_chunks()))
            ).as_py()
            or 0
        )
        self._topic = pc.cast(ordinal, pa.int32())
        # Per level: its distinct keys in ascending numeric order, each key's display name, the
        # ordinal of each *topic*'s node at that level, and each node's parent one level up. All
        # four are tiny (4,798 nodes in total) and precomputing them makes `write_layer` a gather.
        self._level_keys: list[pa.Array] = []
        self._level_names: list[pa.Array] = []
        self._level_parent: list[np.ndarray | None] = []
        self._of_topic: list[np.ndarray] = []
        for depth, level in enumerate(LEVELS):
            column = "topic_id" if level == "topic" else f"{level}_id"
            label = "name" if level == "topic" else level
            ids = self.topics[column].to_pylist()
            names = self.topics[label].to_pylist()
            order = sorted(set(ids), key=lambda k: int(k[1:] if k[0] == "T" else k))
            index = {k: i for i, k in enumerate(order)}
            of_topic = np.fromiter((index[k] for k in ids), np.int32, len(ids))
            display = [""] * len(order)
            for key, name in zip(ids, names):
                display[index[key]] = name
            self._level_keys.append(pa.array(order, pa.string()))
            self._level_names.append(pa.array(display, pa.string()))
            self._of_topic.append(of_topic)
            if depth == 0:
                self._level_parent.append(None)
            else:
                parent = np.zeros(len(order), np.int32)
                parent[of_topic] = self._of_topic[depth - 1]
                self._level_parent.append(parent)

    # ------------------------------------------------------------------------------- the join

    def resolve(self, ids: pa.Array) -> pa.Table:
        """One slice of PaperSeek ids → one row each, in the same order.

        `publication_year int32 | null`, `type string | null`, `is_oa bool | null`,
        `licence string | null` (lowercase, e.g. `cc-by`), `topic int32 | null` (an ordinal into
        `self.topics`). An id OpenAlex no longer carries is null throughout and counted.
        """
        if isinstance(ids, pa.ChunkedArray):
            ids = ids.combine_chunks()
        elif not isinstance(ids, pa.Array):
            ids = pa.array(ids, pa.string())
        key = keys(ids)
        pos = np.searchsorted(self._ids, key)
        np.clip(pos, 0, max(len(self._ids) - 1, 0), out=pos)
        hit = self._ids[pos] == key if len(self._ids) else np.zeros(len(key), bool)
        take = pa.array(pos.astype(np.int64), mask=~hit)

        out = pa.table(
            {
                "publication_year": pc.take(self._year, take),
                "type": pc.cast(pc.take(self._type, take), pa.string()),
                "is_oa": pc.take(self._is_oa, take),
                "licence": pc.cast(pc.take(self._licence, take), pa.string()),
                "topic": pc.take(self._topic, take),
            }
        )
        self.stats["rows"] += len(ids)
        self.stats["matched"] += int(hit.sum())
        self.stats["unmatched"] += int(len(ids) - hit.sum())
        self.stats["with_topic"] += int(len(out) - out["topic"].null_count)
        self.stats["with_licence"] += int(len(out) - out["licence"].null_count)
        return out

    def licences(self) -> list[str]:
        """The closed vocabulary's keys, in the order the declaration numbers them."""
        return list(LICENCES)

    # ------------------------------------------------------------------------------ the layer

    def write_layer(
        self, out: Path, artifacts: ArtifactSet, topic: np.ndarray, rows: np.ndarray
    ) -> None:
        """Emit one slice's member rows at four levels, and (re)declare the artifacts they name.

        `topic` is the ordinal `resolve` returned, one per row, **negative where the work has
        none** (a null Arrow value converts to a negative through `to_numpy(zero_copy_only=False)`
        only by accident, so the caller passes -1 and this checks it rather than trusting a
        sentinel it did not set). `rows` is the source entity id of each.

        **Members stream.** Four rows per work is 4.1x10^8 over the corpus, so they go straight to
        the layer's parquet in row groups through the writer's streaming path, never through a
        Python list.

        **The artifacts are rewritten each call**, which is cheap — 4,798 rows at most — and is
        what makes this callable once per slice without the caller having to say which call is the
        last. **An artifact is a node with at least one member in this corpus**: a topic no work
        here sits under would be served as an empty outline, so the count is a corpus figure and
        not the tree's. Restricting a parent link to a parent that is itself an artifact is a guard
        and never a filter — every ancestor of a member-bearing node bears that same member by
        construction — but an edge to an undeclared artifact would refuse the build.
        """
        topic = np.asarray(topic, dtype=np.int64)
        entities = np.asarray(rows, dtype=np.uint64)
        assert len(topic) == len(entities), "one entity id per row of the slice"
        if not hasattr(self, "_seen"):
            self._seen = [np.zeros(len(k), bool) for k in self._level_keys]

        held = topic >= 0
        topic = topic[held]
        entities = entities[held]

        for depth in range(len(LEVELS)):
            index = self._of_topic[depth][topic]
            if index.size:
                self._seen[depth][np.unique(index)] = True
            for lo in range(0, len(index), BATCH):
                chunk = index[lo : lo + BATCH]
                if not chunk.size:
                    continue
                artifacts.stream_members(
                    LAYER,
                    out,
                    pc.take(self._level_keys[depth], pa.array(chunk.astype(np.int32))),
                    entities[lo : lo + BATCH],
                    level=depth,
                )

        artifacts.per_layer[LAYER] = self._artifact_rows()

    def _artifact_rows(self) -> list[dict]:
        rows = []
        for depth in range(len(LEVELS)):
            keys_ = self._level_keys[depth].to_pylist()
            names = self._level_names[depth].to_pylist()
            parent_of = self._level_parent[depth]
            above = self._level_keys[depth - 1].to_pylist() if depth else []
            for i in np.flatnonzero(self._seen[depth]).tolist():
                parent = None
                if parent_of is not None:
                    p = int(parent_of[i])
                    parent = [above[p]] if self._seen[depth - 1][p] else []
                rows.append(
                    {
                        "level": depth,
                        "key": keys_[i],
                        # One supplied content, rank 0: OpenAlex's own display name.
                        "contents": [[names[i]]],
                        "attached_layer": None,
                        "attached_level": None,
                        "attached_key": None,
                        "parent": parent,
                    }
                )
        return rows

    def artifact_shape(self) -> dict:
        """The declared layer's shape, over the nodes that have members — call after the last
        `write_layer`. The counterpart of the tree's own 4 / 26 / 252 / 4,516."""
        seen = getattr(self, "_seen", [np.zeros(len(k), bool) for k in self._level_keys])
        return {
            "artifacts": int(sum(s.sum() for s in seen)),
            "per_level": {name: int(seen[d].sum()) for d, name in enumerate(LEVELS)},
            "tree_per_level": {name: len(self._level_keys[d]) for d, name in enumerate(LEVELS)},
        }

    def shape(self) -> dict:
        """The published tree's own figures, recomputed from the tables rather than quoted."""
        return {
            "topics": self.topics.num_rows,
            "subfields": len(pc.unique(self.topics["subfield_id"])),
            "fields": len(pc.unique(self.topics["field_id"])),
            "domains": len(pc.unique(self.topics["domain_id"])),
            "extract_rows": int(len(self._ids)),
            "unknown_topic_ids": self.unknown_topics,
            "licences": len(LICENCES),
        }


# The declaration. `[[layer]]` alone: the caller adds `topics` and `topics_members` to its
# `[sources]` and appends this block, as rung 3's MeSH layer does.
#
# Every disclosure control is stated, because none has a default and each answers a different
# question (`annotations.md` §3, `configuration.md` §1).
#
# `visibility = "public"` — the OpenAlex topic tree is published, CC0 taxonomy. That this corpus
# has a layer of scholarly topics over it is not a fact about any work, so the layer's existence is
# reachable by every principal. The gate is on the *artifacts*, not here.
#
# `artifact_visibility = { default = "inherited" }` — no topic carries a label of its own; each is
# reachable exactly when the layer is, and is then tested on its own masked count.
#
# `require_member_visibility = { count = 50 }` — the absolute form, a fixed floor whatever the
# node's size, matching rung 3's MeSH layer and the arXiv rung's k-means layer so the three are
# read against one number. The absolute form is the one under which roll-up is guaranteed: a
# child's members are a subset of its parent's — which here holds by construction, a work being
# written as a member at all four of its levels — so a child's masked count is never larger than
# its parent's and a failing child always leaves a served ancestor above it. The proportional form
# would give that up: a share does not shrink downward, so a narrow topic could clear what its
# domain failed and the map would show holes above it.
#
# `hierarchy = { kind = "tiered", prune_children = true }` — containment running coarser to finer
# across four declared levels, which is exactly what OpenAlex publishes. Not `dag`: every node here
# has one parent. `prune_children = true` as geonames' two tiered layers, so a client zoomed to a
# domain is not also served the 4,516 topics beneath it.
#
# **The supplied content is `inherited`, not `all`.** A cluster's c-TF-IDF title is a synthesis of
# the documents it was drawn from and is served only to a viewer who can read every one of them.
# This content is OpenAlex's own display name for a published, CC0 taxonomy node. It asserts
# nothing about any work in this corpus, it would read identically if the corpus were empty, and
# any reader can look it up. Under `all` it would additionally have to arrive with a generating
# set, and a set tested against a claim it did not generate is a control the service carries
# without meaning (C28). So `inherited`, served on the artifact's own existence test — which is the
# masked-count floor above, and *that* is what keeps a sparse topic off the map.
#
# `computed = ["centroid", "box", "hull"]` — the opposite call from rung 3's MeSH layer, and the
# reason is the geometry. A MeSH descriptor's members are spread across the whole layout (98% came
# back `everywhere`), so a centroid places a label on nothing. A topic in an embedding space should
# be compact — that is what the embedding is *for* — so the three shapes should mean something
# here. The vectors track measures the median box share and **the layer is withdrawn if it draws
# nothing**, on the same rule that withdrew two of the arXiv rung's taxonomies.
#
# `membership = "enumerated"` — four rows per work, written by `write_layer`.
#
# The zoom ranges overlap by one step at each seam, following geonames' `admin/hierarchy`: four
# levels over the 0..16 the tile addressing caps at, so a viewport asking at any depth is answered
# at one or two levels rather than none.
LAYER_TOML = """
[[layer]]
name       = "topics/openalex"
title      = "OpenAlex topics"
source     = "topics"
views      = ["knn"]
membership = "enumerated"
hierarchy  = { kind = "tiered", prune_children = true }

visibility          = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 50 }

  [[layer.levels]]
  level = 0
  title = "Domain"
  zoom  = [0, 4]

  [[layer.levels]]
  level = 1
  title = "Field"
  zoom  = [3, 8]

  [[layer.levels]]
  level = 2
  title = "Subfield"
  zoom  = [7, 12]

  [[layer.levels]]
  level = 3
  title = "Topic"
  zoom  = [11, 16]

  [layer.members]
  source = "topics_members"

  [layer.content]
  computed = ["centroid", "box", "hull"]

    [[layer.content.supplied]]
    name                      = "name"
    type                      = "text"
    require_member_visibility = "inherited"
"""

SOURCES_TOML = """topics                = "topics-openalex.parquet"
topics_members        = "topics-openalex-members.parquet"
"""


def main() -> int:
    """`python -m test_corpora.paperseek.openalex` — the tree's and the extract's figures."""
    oa = OpenAlex()
    print(json.dumps(oa.shape(), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
