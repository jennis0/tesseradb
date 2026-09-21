"""MeSH: the descriptor DAG, the per-article join, and the ancestor closure the layer is built on.

**What this is.** The NLM publishes Medical Subject Headings as a flat `Descriptor Name;TreeNumber`
file, one line per *position*: 30,954 descriptors at 64,883 tree numbers, 52.9% of them at more than
one position. Keyed by tree number that is a strict tree; keyed by descriptor — which is what an
article's `m` field names, and the key the rung's artifacts take
([decision 0117](../../docs/decisions/0117-a-child-may-name-several-parents.md)) — it is a directed
acyclic graph in which 30.0% of nodes name more than one parent. D is a parent of E iff some tree
number of E has, as its dotted prefix, a tree number D owns; edges are deduplicated across
positions. The graph is acyclic and self-edge-free, verified by Tarjan over the whole of it
(`probes/2026-09-01-mesh-dag/`).

**Why the membership is closed upward.** MeSH indexing assigns the *most specific* heading and not
its ancestors: an article about lung cancer carries `lung neoplasms` and not `neoplasms`. Declared
that way, a parent does not contain its children, containment is reported violated on essentially
every edge, and roll-up serves `neoplasms` in place of `lung neoplasms` with a count that excludes
it — a count of a different thing. So each article's membership is its descriptors **and every
ancestor of each**, which is what a subject heading's population is (`dag-hierarchies.md` §8, ruled
in 0117(D)). The price is rows: ≈1.7×10⁹ `(article, descriptor)` pairs at full scale against
3.1×10⁸ unclosed, and that is the rung's scaling finding rather than a reason to avoid it.

**The closure is a set.** Entries are counted, not deduplicated, so a repeated `(article, ancestor)`
row — which a naive walk over ten descriptors' shared ancestors produces at once — would move a
declared count. Everything below deduplicates per row, by sorting, and the sort is also what puts a
row's ids in ascending order.

**Descriptor mentions that do not resolve against the 2025 vintage are dropped, and the miss is not
random** — it is weighted towards the ancestry and ethnicity terms the NLM revised in 2022–23.
Two figures, and they are not the same measurement: **5.87% of mentions over 89 headings** on
chunk 18 alone (the acquisition README's number, which is where the shape of the miss was first
seen), and **6.3% over 2,875 headings** over the whole corpus, which runs back to 1975 and so meets
far more retired vocabulary. Quote the second beside a whole-corpus figure. They are **dropped and
counted** (owner ruling, `ingest-campaign.md` §4.4); `resolve` returns the count and the worst
offenders by name so a caller can state the slice beside its figures rather than discover it later.

**Everything here is vectorised.** `resolve` and `closure` are called once per staged chunk of about
a million rows, 38 times, so neither may build a Python object per article: the parse is Arrow
compute over the flat entries, the join is one hash lookup over all of them at once, and the closure
is a CSR gather over `int32` NumPy arrays. Measured on chunk 18 — see `README` of the rung.
"""

from __future__ import annotations

import collections
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc

from . import sources

try:  # the writer moves to `common/` when the other track lands; both spellings are one class
    from test_corpora.common.writer import ArtifactSet
except ImportError:  # pragma: no cover - whichever of the two is present
    from test_corpora.arxiv.writer import ArtifactSet

LAYER = "mesh/descriptors"

#: Rows per internal batch in `closure` and `write_layer`. The closure of a million articles is
#: tens of millions of pairs; sorting them in one array is a gigabyte of int64 key that buys
#: nothing, so the work is cut into batches whose peak is bounded and whose result is identical.
BATCH = 200_000

#: The largest value an Arrow `list`'s 32-bit offsets can address. `_list` refuses past it rather
#: than letting an int32 cumulative sum wrap into a well-formed list with the wrong row boundaries.
_OFFSET_LIMIT = 2**31 - 1


def _unique(key: np.ndarray) -> np.ndarray:
    """The sorted distinct values of an integer array, in place where it can be.

    **Not `np.unique`.** NumPy 2.4 answers that through a hash table for integers, which measured
    **11.0 s** on sixteen million `int64` keys against `np.sort`'s **0.28 s** on the same array.
    Every deduplication in this module is over tens of millions of packed keys, so the difference
    is the whole cost of the closure.
    """
    key.sort()
    return key[np.concatenate(([True], key[1:] != key[:-1]))] if key.size else key


class Mesh:
    """The descriptor DAG, and the two operations a staged chunk needs against it."""

    def __init__(self, tree_path: Path | None = None):
        tree_path = tree_path or sources.mesh_tree()
        display: dict[str, str] = {}
        name_of_tn: dict[str, str] = {}
        tns_of: dict[str, list[str]] = collections.defaultdict(list)
        with open(tree_path, encoding="utf-8") as f:
            for line in f:
                line = line.rstrip("\n")
                if not line:
                    continue
                name, tn = line.rsplit(";", 1)
                tn = tn.strip()
                name = name.strip()  # the lines carry a leading space
                key = name.lower()
                display.setdefault(key, name)
                name_of_tn[tn] = key
                tns_of[key].append(tn)

        #: Lowercased descriptor names, sorted; the index into this list is the descriptor id.
        self.descriptors: list[str] = sorted(display)
        #: The NLM's own casing, parallel to `descriptors` — what a viewer is shown.
        self.display: list[str] = [display[k] for k in self.descriptors]
        self._id = {k: i for i, k in enumerate(self.descriptors)}

        #: Deduplicated parent ids per descriptor id. A root's list is empty.
        self.parents: list[list[int]] = [[] for _ in self.descriptors]
        for key, tns in tns_of.items():
            child = self._id[key]
            seen: set[int] = set()
            for tn in tns:
                if "." not in tn:
                    continue  # a root position
                parent = self._id[name_of_tn[tn.rsplit(".", 1)[0]]]
                if parent != child and parent not in seen:
                    seen.add(parent)
                    self.parents[child].append(parent)
            self.parents[child].sort()

        self._value_set = pa.array(self.descriptors, pa.string())
        self._build_closures(tns_of)

    # ----------------------------------------------------------------- the DAG, precomputed once

    def _build_closures(self, tns_of) -> None:
        """The self-and-ancestors set of every descriptor, as CSR, plus its top-level branches.

        Computed once in topological order by union — the memoised walk the probe used — and then
        flattened, because the closure of a chunk is a gather over these arrays and a gather wants
        one contiguous buffer rather than 30,954 Python sets. Mean set size is 8.0 including the
        descriptor itself, so the whole thing is under a megabyte.

        The topological order is Kahn's over the parent lists; it terminates only on an acyclic
        graph, so the assertion below is the cycle check as well. A cycle would be a change in the
        NLM's file rather than in this code, and the build refuses one too.
        """
        n = len(self.descriptors)
        children: list[list[int]] = [[] for _ in range(n)]
        for child, ps in enumerate(self.parents):
            for p in ps:
                children[p].append(child)
        indegree = np.fromiter((len(p) for p in self.parents), np.int32, n)
        queue = collections.deque(int(i) for i in np.flatnonzero(indegree == 0))
        order: list[int] = []
        while queue:
            node = queue.popleft()
            order.append(node)
            for child in children[node]:
                indegree[child] -= 1
                if indegree[child] == 0:
                    queue.append(child)
        assert len(order) == n, "the descriptor DAG has a cycle, which the 2025 vintage has not"
        self.roots = int((np.fromiter((len(p) for p in self.parents), np.int32, n) == 0).sum())

        sets: list[frozenset[int]] = [frozenset()] * n
        for node in order:
            acc: set[int] = set()
            for p in self.parents[node]:
                acc.add(p)
                acc |= sets[p]
            sets[node] = frozenset(acc)

        counts = np.fromiter((len(sets[i]) + 1 for i in range(n)), np.int32, n)
        self._clo_off = np.zeros(n + 1, np.int64)
        np.cumsum(counts, out=self._clo_off[1:])
        flat = np.empty(int(self._clo_off[-1]), np.int32)
        for node in range(n):
            entries = sorted(sets[node] | {node})
            flat[self._clo_off[node] : self._clo_off[node + 1]] = entries
        self._clo_flat = flat
        #: Ancestors alone, for reporting; the closure arrays carry the descriptor as well.
        self.ancestor_count = counts - 1

        # Depth by longest path — decision 0117(B): every edge of a DAG descends, so a served
        # count is not monotone in depth and a budget reads every depth rather than bisecting.
        # Reported here so the layer's spread can be tabled by it; the build computes its own.
        depth = np.zeros(n, np.int32)
        for node in order:
            for p in self.parents[node]:
                if depth[p] + 1 > depth[node]:
                    depth[node] = depth[p] + 1
        self.depth = depth

        # The top-level branch letters a descriptor sits under, as a bit per letter. A descriptor
        # spans several (2,633 of them do), and the union over an article's descriptors is one
        # `bitwise_or`, which is what makes `branches_of` cheap enough to call per row.
        letters = sorted({tn[0] for tns in tns_of.values() for tn in tns})
        self._letters = letters
        bit = {c: 1 << i for i, c in enumerate(letters)}
        mask = np.zeros(n, np.uint32)
        for key, tns in tns_of.items():
            m = 0
            for tn in tns:
                m |= bit[tn[0]]
            mask[self._id[key]] = m
        self._branch_mask = mask
        self._branch_cache: dict[int, list[str]] = {0: []}

    # --------------------------------------------------------------------------------- reporting

    def shape(self) -> dict:
        """The graph's own figures, for a manifest — the numbers `probes/2026-09-01-mesh-dag/`
        measured, recomputed from the file rather than quoted."""
        return {
            "descriptors": len(self.descriptors),
            "edges": sum(len(p) for p in self.parents),
            "roots": self.roots,
            "multi_parent": sum(1 for p in self.parents if len(p) > 1),
            "max_parents": max(len(p) for p in self.parents),
            "longest_path": int(self.depth.max()),
            "branches": "".join(self._letters),
            "ancestors_mean": round(float(self.ancestor_count.mean()), 2),
        }

    # ------------------------------------------------------------------------------ the branches

    def branches_of(self, ids: np.ndarray) -> list[str]:
        """The top-level MeSH branch letters one article's descriptors sit under.

        This is the rung's **access column**: a synthetic compartment scheme over a real column, as
        every rung of the ladder uses. An article whose descriptors span `C` and `N` is labelled
        with both, and an article with no resolved descriptor gets no letter here — the caller
        substitutes `unindexed`, because `point_visibility`'s default fires on an empty list and a
        point that falls through to the default is one nothing states a policy for.
        """
        ids = np.asarray(ids)
        if ids.size == 0:
            return []
        mask = int(np.bitwise_or.reduce(self._branch_mask[ids]))
        hit = self._branch_cache.get(mask)
        if hit is None:
            hit = [c for i, c in enumerate(self._letters) if mask >> i & 1]
            self._branch_cache[mask] = hit
        return hit

    def branches(self, explicit: pa.ListArray, *, empty: str = "unindexed") -> pa.ListArray:
        """`branches_of` over a whole chunk at once, which is how a caller should reach for it.

        Same answer as calling `branches_of` per row and cheaper by two orders of magnitude: the
        per-row union is a segmented `bitwise_or`, done here as `np.maximum.reduceat` over a
        running OR — 16 passes over the flat array, one per letter, rather than a Python call per
        article.
        """
        offsets = np.asarray(explicit.offsets, dtype=np.int64)
        values = np.asarray(explicit.values, dtype=np.int64)
        masks = self._branch_mask[values]
        n = len(explicit)
        # A segmented OR: `add.reduceat` on the bit-plane of each letter, saturated to 0/1.
        per_row = np.zeros(n, np.uint32)
        if values.size:
            starts = offsets[:-1]
            nonempty = starts < offsets[1:]
            idx = starts[nonempty]
            for i in range(len(self._letters)):
                plane = (masks >> i & 1).astype(np.int64)
                hit = np.add.reduceat(plane, idx) > 0
                per_row[nonempty] |= hit.astype(np.uint32) << i
        out: list[list[str]] = []
        for mask in per_row.tolist():
            if mask == 0:
                out.append([empty])
                continue
            hit = self._branch_cache.get(mask)
            if hit is None:
                hit = [c for i, c in enumerate(self._letters) if mask >> i & 1]
                self._branch_cache[mask] = hit
            out.append(hit)
        return pa.array(out, pa.list_(pa.string()))

    # ---------------------------------------------------------------------------------- the join

    def resolve(self, m) -> tuple[pa.ListArray, pa.ListArray, dict]:
        """Raw `m` strings → explicit descriptor ids per row, major-topic ids per row, and stats.

        The field is `name!qualifier*|name*|…`: the descriptor is what precedes the first `!` or
        `*`, a trailing `*` marks a major topic, and the *same descriptor recurs with a qualifier
        each time it was indexed*, so `rna, untranslated` appears five times in one article and
        must count once. Both returned lists are therefore deduplicated and ascending.

        Unresolved mentions are dropped and counted (`ingest-campaign.md` §4.4). `stats` carries
        the totals and the twenty worst names, so a caller can print the slice it lost.
        """
        if isinstance(m, pa.ChunkedArray):
            m = m.combine_chunks()
        elif not isinstance(m, pa.Array):
            m = pa.array(m, pa.string())
        m = pc.fill_null(m, "")
        n = len(m)

        entries = pc.split_pattern(m, "|")
        flat = entries.values
        # The trailing `|` of every field leaves one empty entry per article; it names nothing and
        # is not a miss, which is what `nonempty` keeps it out of below.
        names = pc.extract_regex(flat, r"^(?P<name>[^!*]*)").field("name")
        major = np.asarray(pc.fill_null(pc.ends_with(flat, pattern="*"), False))

        found = pc.index_in(names, value_set=self._value_set)
        ids = np.asarray(pc.fill_null(found, -1))
        row_of = np.repeat(
            np.arange(n, dtype=np.int64), np.diff(np.asarray(entries.offsets, dtype=np.int64))
        )

        hit = ids >= 0
        nonempty = np.asarray(pc.greater(pc.binary_length(flat), 0))
        unresolved = ~hit & nonempty
        # Counted the way the resolved side is counted — **distinct per article**, so a heading
        # seen under three qualifiers on one article is one lost mention and the two halves of the
        # coverage figure are the same kind of number.
        missed = names.filter(pa.array(unresolved))
        encoded = pc.dictionary_encode(missed)
        top = pc.value_counts(missed)
        worst = sorted(
            ((v["values"].as_py(), v["counts"].as_py()) for v in top),
            key=lambda kv: -kv[1],
        )[:20]
        if len(missed):
            width = max(len(encoded.dictionary), 1)
            lost = len(
                _unique(row_of[unresolved] * width + np.asarray(encoded.indices, dtype=np.int64))
            )
        else:
            lost = 0

        explicit = self._rows(row_of[hit], ids[hit], n)
        majors = self._rows(row_of[hit & major], ids[hit & major], n)
        per_row = np.diff(np.asarray(explicit.offsets, dtype=np.int64))
        stats = {
            "rows": n,
            # Articles with at least one *resolved* descriptor. An article indexed only under a
            # retired heading is not one of them, and is counted in the drop.
            "indexed_rows": int(np.count_nonzero(per_row)),
            # Distinct (article, descriptor) mentions, which is what the closure starts from —
            # the same descriptor under five qualifiers is one.
            "resolved_mentions": int(per_row.sum()),
            "major_mentions": int(len(majors.values)),
            "unresolved_mentions": int(lost),
            "unresolved_entries": int(unresolved.sum()),
            "unresolved_distinct": len(top),
            "unresolved_top": worst,
        }
        return explicit, majors, stats

    # ------------------------------------------------------------------------------- the closure

    def closure(self, explicit: pa.ListArray) -> pa.ListArray:
        """Each row's descriptors **and every ancestor of each**, as a set (design §8).

        A gather, not a walk: every descriptor's self-and-ancestors set is already one contiguous
        run of `_clo_flat`, so the whole chunk expands with two `repeat`s and one fancy index, and
        the deduplication is the sort that also orders each row.
        """
        offsets = np.asarray(explicit.offsets, dtype=np.int64)
        values = np.asarray(explicit.values, dtype=np.int64)
        n = len(explicit)
        counts: list[np.ndarray] = []
        expanded: list[np.ndarray] = []
        for lo in range(0, n, BATCH):
            hi = min(lo + BATCH, n)
            ids = values[offsets[lo] : offsets[hi]]
            rows = np.repeat(
                np.arange(hi - lo, dtype=np.int64), np.diff(offsets[lo : hi + 1])
            )
            widths = (self._clo_off[ids + 1] - self._clo_off[ids]).astype(np.int64)
            total = int(widths.sum())
            starts = np.repeat(self._clo_off[ids], widths)
            within = np.arange(total, dtype=np.int64) - np.repeat(
                np.cumsum(widths) - widths, widths
            )
            # **Deduplicated per batch, not once at the end.** The batches are row-disjoint, so
            # the answer is identical either way, and the sort is the whole cost of this function:
            # one sort of 65 million keys is measurably slower than five of thirteen million, and
            # it holds a gigabyte to do it.
            c, v = self._dedup(
                np.repeat(rows, widths),
                self._clo_flat[starts + within].astype(np.int64),
                hi - lo,
            )
            counts.append(c)
            expanded.append(v)
        return self._list(
            np.concatenate(counts) if counts else np.zeros(0, np.int32),
            np.concatenate(expanded) if expanded else np.zeros(0, np.int32),
            n,
        )

    def keys(self, closed: pa.ListArray) -> pa.ListArray:
        """A closure's ids as descriptor keys: the same rows, each a `list<string>` of artifact keys.

        This is the closure in the form a *row* carries it — the `mesh/descriptors` column of
        `points.parquet`, one cell per article naming every artifact it is in. Under `dag` that
        list is plain multi-membership, a set and not a lineage
        ([decision 0125](../../docs/decisions/0125-a-dag-list-column-is-membership-not-lineage.md)), so an ingest
        batch carrying the column lands each entry as one membership and the member table need
        never be inverted per entity. One `take` over the flat ids; the offsets are `closed`'s own.
        """
        return pa.ListArray.from_arrays(
            closed.offsets, pc.take(self._value_set, closed.values), type=pa.list_(pa.string())
        )

    #: The key `_dedup` packs `(row, id)` into is `row << 15 | id`, a power of two so the unpack is
    #: a shift and a mask rather than a division — on tens of millions of entries that is seconds.
    _BITS = 15
    _MASK = (1 << 15) - 1

    def _dedup(self, rows: np.ndarray, ids: np.ndarray, n: int) -> tuple[np.ndarray, np.ndarray]:
        """`(row, id)` pairs → per-row counts and ascending, deduplicated ids.

        Deduplication is by sorting a single `int64` key rather than by a set per article: the ids
        are bounded by the descriptor count, so the packed key is injective and one sort does both
        the dedupe and the per-row ordering.

        **The sort is spelled out rather than left to `np.unique`**, which is not the same
        operation here. NumPy 2.4 answers `np.unique` on integers through a hash table, and on
        sixteen million keys that measured **11.0 s** against `np.sort`'s **0.28 s** on the same
        array — a 39× difference, and the whole cost of the closure. Sorting and taking the
        boundaries is the fast path and it is also the one that leaves the ids ascending, which is
        what the list wants anyway.
        """
        assert len(self.descriptors) <= self._MASK, "the descriptor id no longer fits the key"
        if not rows.size:
            return np.zeros(n, np.int32), np.zeros(0, np.int32)
        key = _unique((rows.astype(np.int64) << self._BITS) | ids.astype(np.int64))
        return (
            np.bincount(key >> self._BITS, minlength=n).astype(np.int32),
            (key & self._MASK).astype(np.int32),
        )

    def _list(self, counts: np.ndarray, values: np.ndarray, n: int) -> pa.ListArray:
        """Per-row counts and a flat value array as an Arrow `list`.

        **The cumulative sum is taken in int64 and checked before it is narrowed.** An Arrow `list`
        addresses its values with 32-bit offsets, and this accumulation used to run in int32 —
        `np.cumsum` into an int32 buffer wraps *silently*, and a wrapped offset does not corrupt the
        values, it moves a row's boundary. What comes out is a well-formed list whose rows hold each
        other's members: a wrong **count** on a served artifact, not a crash.

        The closure is the caller that could reach it. At `prepare.py`'s `MESH_SLICE` of 10^6 rows
        the largest slice measured expanded to 5.8x10^7 entries, 37x under the limit — but the slice
        size is a dial, and ~3.7x10^7 rows would cross it. So this refuses, loudly, naming both
        numbers. `large_list` would carry it, at the cost of a wider offset on every consumer for a
        size nothing here needs.
        """
        offsets = np.zeros(n + 1, np.int64)
        np.cumsum(counts, dtype=np.int64, out=offsets[1:])
        if offsets[-1] > _OFFSET_LIMIT:
            raise ValueError(
                f"{n:,} rows expanded to {int(offsets[-1]):,} entries, past the "
                f"{_OFFSET_LIMIT:,} an Arrow list's 32-bit offsets address. Lower prepare.py's "
                f"MESH_SLICE, or give this a large_list."
            )
        return pa.ListArray.from_arrays(
            pa.array(offsets.astype(np.int32), pa.int32()), pa.array(values, pa.int32())
        )

    def _rows(self, rows: np.ndarray, ids: np.ndarray, n: int) -> pa.ListArray:
        return self._list(*self._dedup(rows, ids, n), n)

    # --------------------------------------------------------------------------------- the layer

    def write_layer(
        self, out: Path, artifacts: ArtifactSet, closed: pa.ListArray, rows: np.ndarray
    ) -> None:
        """Emit one staged chunk's member rows, and (re)declare the artifacts they name.

        **Members stream.** `closed` is already several tens of millions of pairs for a million
        articles, and 1.7×10⁹ over the corpus, so they go straight to the layer's parquet in row
        groups through the writer's streaming path — never through a Python list. `rows` is the
        source entity id of each row of `closed`.

        **The artifacts are rewritten each call**, which is cheap (tens of thousands of rows) and
        is what makes this callable once per chunk without the caller having to say which call is
        the last: a descriptor becomes an artifact the first time a member names it, and the
        declaration after the final chunk is the whole of it.

        **An artifact is a descriptor with at least one member in this corpus.** A heading no
        article here is indexed under draws nothing and would be served as an empty outline, so the
        artifact count is a *corpus* figure and not the tree's 30,954. Restricting each parent list
        to parents that are themselves artifacts is therefore a guard rather than a filter: under
        ancestor closure every ancestor of a member-bearing descriptor bears that same member, so
        nothing is ever dropped by it — but an edge to an artifact the layer does not declare would
        refuse the build, and this is the line that would have to change if the closure ever did.
        """
        if not hasattr(self, "_seen"):
            self._seen = np.zeros(len(self.descriptors), bool)

        offsets = np.asarray(closed.offsets, dtype=np.int64)
        values = np.asarray(closed.values, dtype=np.int64)
        entities = np.asarray(rows, dtype=np.uint64)
        assert len(entities) == len(closed), "one entity id per row of the closure"
        if values.size:
            self._seen[_unique(values.copy())] = True

        n = len(closed)
        for lo in range(0, n, BATCH):
            hi = min(lo + BATCH, n)
            first, last = offsets[lo], offsets[hi]
            if first == last:
                continue
            ids = values[first:last]
            per_row = np.diff(offsets[lo : hi + 1])
            artifacts.stream_members(
                LAYER,
                out,
                pc.take(self._value_set, pa.array(ids, pa.int32())),
                np.repeat(entities[lo:hi], per_row),
            )

        kept = np.flatnonzero(self._seen)
        rows_out = []
        for i in kept.tolist():
            rows_out.append(
                {
                    "level": 0,
                    "key": self.descriptors[i],
                    # One supplied content, rank 0: the heading as the NLM writes it.
                    "contents": [[self.display[i]]],
                    "attached_layer": None,
                    "attached_level": None,
                    "attached_key": None,
                    "parent": [self.descriptors[p] for p in self.parents[i] if self._seen[p]],
                }
            )
        artifacts.per_layer[LAYER] = rows_out

    def artifact_shape(self) -> dict:
        """The declared layer's shape, over the descriptors that have members — the counterpart of
        `shape()`, which is the whole tree. Call after the last `write_layer`."""
        seen = getattr(self, "_seen", np.zeros(len(self.descriptors), bool))
        kept = np.flatnonzero(seen)
        parents = [[p for p in self.parents[i] if seen[p]] for i in kept.tolist()]
        return {
            "artifacts": len(kept),
            "edges": sum(len(p) for p in parents),
            "roots": sum(1 for p in parents if not p),
            "multi_parent": sum(1 for p in parents if len(p) > 1),
            "max_parents": max((len(p) for p in parents), default=0),
        }


# The declaration. `[[layer]]` alone: the caller adds `mesh` and `mesh_members` to its `[sources]`
# and appends this block, as the arXiv rung's optional stage appends its own.
#
# Every disclosure control is stated, because none has a default and each answers a different
# question (`annotations.md` §3, `configuration.md` §1).
#
# `visibility = "public"` — MeSH is published taxonomy. That this corpus has a layer of medical
# subject headings over it is not a fact about any article, so the layer's existence is reachable
# by every principal. The gate is on the *artifacts*, not here.
#
# `require_member_visibility = { count = 50 }` — the absolute form, a fixed floor whatever the
# descriptor's size, matching the arXiv rung's k-means layer so the two are read against one
# number. The absolute form is the one under which roll-up is guaranteed: a child's members are a
# subset of its parent's — which under ancestor closure they are, by construction — so a child's
# masked count is never larger than its parent's and a failing child always leaves a served
# ancestor above it. That is exactly what the closure was paid for, and declaring the proportional
# form here would give it up: a share does not shrink downward, so a narrow child could clear what
# a root failed and the map would show holes above it.
#
# `artifact_visibility = { default = "inherited" }` — no descriptor carries a label of its own;
# each is reachable exactly when the layer is, and then tested on its own masked count.
#
# **The supplied content is `inherited`, not `all`, and that is the one line here worth arguing.**
# A cluster's c-TF-IDF title is a synthesis of the documents it was drawn from, so it is served
# only to a viewer who can read every one of them — `all`, with a generating set. This content is
# the NLM's own spelling of a published heading. It asserts nothing about any article in this
# corpus; it would read identically if the corpus were empty, and any reader can look it up. Under
# `all` it would additionally have to arrive with a generating set, and a set that is tested
# against a claim it did not generate is a control the service carries without meaning (C28). So:
# `inherited`, no generating set, served on the artifact's own existence test — which is the
# masked-count floor above, and *that* is what keeps a sparse descriptor off the map.
#
# `hierarchy = { kind = "dag", prune_children = false }` — decision 0117(A). 30.0% of descriptors
# name more than one parent and no other kind admits that. `prune_children = false` because the
# frontier's own per-artifact test already decides what is served; pruning is a rendering choice
# and this layer has nothing to gain from it.
#
# `membership = "enumerated"` — one row per (descriptor, article), written by `write_layer`. The
# same closure travels a second way, as a `mesh/descriptors` list column on `points.parquet`
# (`keys`), which is what an ingest batch carries: the build reads the member table, the ingest
# cycle reads the column, and the two land the same memberships (decision 0125).
#
# `views = ["knn"]` — the layer is declared over the anchor layout alone. A computed property is
# recomputed per view, and the rung's second view (if it has one) would double the cost of a layer
# whose membership is already the largest thing in the bundle.
LAYER_TOML = """
[[layer]]
name       = "mesh/descriptors"
title      = "MeSH descriptors"
source     = "mesh"
views      = ["knn"]
membership = "enumerated"
hierarchy  = { kind = "dag", prune_children = false }

visibility          = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 50 }

  [layer.members]
  source = "mesh_members"

  [layer.content]
  # **No computed content: this is a filter layer** (owner ruling, 2026-09-02). A descriptor's
  # members are spread across the whole layout — 98% of these artifacts came back `everywhere` in
  # the build's report — so a centroid places a label on nothing, a box is the map and a hull is
  # the map's outline. What a descriptor is good for is naming a set of articles: browsed as a
  # hierarchy and applied as a filter or a highlight, never drawn as a shape. The clustering layer
  # keeps its shapes because its artifacts are compact.
  computed = []

    [[layer.content.supplied]]
    name                      = "name"
    type                      = "text"
    require_member_visibility = "inherited"
"""

SOURCES_TOML = """mesh                  = "mesh-descriptors.parquet"
mesh_members          = "mesh-descriptors-members.parquet"
"""
