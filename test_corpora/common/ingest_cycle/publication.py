from __future__ import annotations

import json
import shutil
import time
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

# ---------------------------------------------------------------------------------------------
# Publication — the artifacts, after their points
# ---------------------------------------------------------------------------------------------

#: The base64 alphabet, indexed by sextet.
_B64 = np.frombuffer(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/", np.uint8)

#: A member table's row, as the partitioning pass writes it: the artifact's ordinal in publication
#: order, the row's rank (−1 is the membership, `k` is `contents[k]`'s generating set) and the
#: entity. 14 bytes; 1.66×10⁹ rows of rung 3's DAG membership are 23 GB of transient files.
MEMBER_RECORD = np.dtype([("idx", "<i4"), ("rank", "<i2"), ("entity", "<u8")])

EMPTY_ENTITIES = np.zeros(0, np.uint64)


def external_ids_b64(entities: np.ndarray) -> np.ndarray:
    """`(n, 12)` uint8: each entity id as eight little-endian bytes, base64.

    **The external id is the source entity id, little-endian, because that is the one address both
    halves of the split share.** A publication names its members by external id, and its members
    are base rows and ingested rows alike: the ingested half carries whatever `external_id` the
    batch supplied, and the built half carries whatever the build minted, which is
    `source_id.to_le_bytes()` under `--mint-external-ids` (`tessera-build`'s `ExternalIdRow`) and
    nothing at all without it. So the driver builds the base with that flag and sends the same
    eight bytes on ingest, and one member list then addresses both.

    Computed from the ids in NumPy rather than looked up in a table indexed by entity id: rung 5's
    table was 16 bytes for each of 2.3×10⁸ entities, 3.7 GB, filled by a Python loop over every
    id. Eight bytes are two full base64 groups and one group of two bytes, so the twelfth
    character is always `=`.

    ⊘ **This is why the PMID is no longer the external id.** An earlier driver sent the PMID, which
    is the natural caller identifier for that corpus and is still the `pmid` attribute, but the
    build cannot mint an external id from a column, so a published membership over base rows was
    unaddressable and the whole batch was refused, naming member 0 of artifact 0.
    """
    raw = np.ascontiguousarray(entities, dtype="<u8").view(np.uint8).reshape(-1, 8)
    out = np.empty((len(raw), 12), np.uint8)
    for group in range(2):
        b0, b1, b2 = raw[:, 3 * group], raw[:, 3 * group + 1], raw[:, 3 * group + 2]
        out[:, 4 * group] = _B64[b0 >> 2]
        out[:, 4 * group + 1] = _B64[((b0 & 3) << 4) | (b1 >> 4)]
        out[:, 4 * group + 2] = _B64[((b1 & 15) << 2) | (b2 >> 6)]
        out[:, 4 * group + 3] = _B64[b2 & 63]
    b6, b7 = raw[:, 6], raw[:, 7]
    out[:, 8] = _B64[b6 >> 2]
    out[:, 9] = _B64[((b6 & 3) << 4) | (b7 >> 4)]
    out[:, 10] = _B64[(b7 & 15) << 2]
    out[:, 11] = ord("=")
    return out


def json_list_bytes(count: int) -> int:
    """The length [`json_list`] produces for `count` ids: `["` + 12 chars + `"` and a comma each."""
    return 2 if count == 0 else 15 * count + 1


def json_list(entities: np.ndarray) -> bytes:
    """A JSON array of base64 external ids, as bytes, assembled in NumPy.

    One `(n, 15)` byte array — quote, twelve characters, quote, comma — then one copy. No Python
    string is made per member: `json.dumps` over 7.6×10⁵ of them was the largest single cost in
    a publication, and a list of 4×10⁶ Python `bytes` is a gigabyte of interpreter objects.
    """
    if not len(entities):
        return b"[]"
    cell = np.empty((len(entities), 15), np.uint8)
    cell[:, 0] = ord('"')
    cell[:, 1:13] = external_ids_b64(entities)
    cell[:, 13] = ord('"')
    cell[:, 14] = ord(",")
    return b"[" + cell.tobytes()[:-1] + b"]"


def in_parent_order(table: pa.Table) -> pa.Table:
    """A roster's rows reordered so that no artifact precedes a parent of its own.

    A publication resolves a parent key against the level as it stands **plus the artifacts earlier
    in the same batch** (`LayerRegistry::prepare_publish`), so a child published before its parent
    names nothing and refuses the batch. A roster is written in key order, which for a DAG of MeSH
    descriptors is alphabetical and unrelated to depth.

    By longest path to a root, which is the layer's own depth and is what a level-by-level
    publication would have used. A parent the roster does not hold is ignored here, as it is where
    the row is written: it is not an artifact of this layer.
    """
    if "parent" not in table.schema.names:
        return table
    keys = table.column("key").to_pylist()
    parents = table.column("parent").to_pylist()
    at = {key: i for i, key in enumerate(keys)}
    depth = [-1] * len(keys)

    for start in range(len(keys)):
        # Iterative rather than recursive: 3.0×10⁴ descriptors is shallow, but a rung's DAG is the
        # caller's data and a recursion limit is not the refusal anyone wants to read. `on_stack`
        # is the cycle guard — the service refuses a cycle at publication, and a driver that spun
        # for ever instead of saying so would look like a hung run.
        stack = [start]
        on_stack = set()
        while stack:
            i = stack[-1]
            if depth[i] >= 0:
                on_stack.discard(i)
                stack.pop()
                continue
            pending = [at[k] for k in (parents[i] or []) if k in at and depth[at[k]] < 0]
            if any(j in on_stack for j in pending):
                raise ValueError(f"the roster's parent edges cycle at {keys[i]!r}")
            if pending:
                on_stack.add(i)
                stack.extend(pending)
                continue
            depth[i] = 1 + max(
                (depth[at[k]] for k in (parents[i] or []) if k in at), default=-1
            )
            on_stack.discard(i)
            stack.pop()
    order = sorted(range(len(keys)), key=lambda i: depth[i])
    return table.take(pa.array(order, pa.int64()))


def key_order_is_parent_order(keys: Sequence[str], parents: Sequence[list | None]) -> bool:
    """Whether publishing the roster in key order would put every parent before its children.

    True for a layer with no edges. Where it holds, a member file sorted by key can be streamed
    and published in file order; where it does not, the file is partitioned into publication
    order first.
    """
    held = set(keys)
    return all(
        parent < key
        for key, own in zip(keys, parents)
        for parent in (own or [])
        if parent in held
    )


def key_ranges(path: Path) -> list[tuple[str, str]] | None:
    """`(min key, max key)` per row group from the file's own statistics; None if any lacks them."""
    reader = pq.ParquetFile(path)
    column = reader.schema_arrow.names.index("key")
    out = []
    for i in range(reader.metadata.num_row_groups):
        statistics = reader.metadata.row_group(i).column(column).statistics
        if statistics is None or not statistics.has_min_max:
            return None
        out.append((statistics.min, statistics.max))
    return out


def grouped_by_key(ranges: Sequence[tuple[str, str]]) -> bool:
    """Whether consecutive row groups' key ranges never overlap, so every key's rows are contiguous.

    Two adjacent row groups may share one key at their boundary; a row group whose maximum exceeds
    the next one's minimum means a key's rows can be anywhere in the file. Parquet orders string
    statistics bytewise, as Python compares `str`.
    """
    return all(a_max <= b_min for (_, a_max), (b_min, _) in zip(ranges, ranges[1:]))


def member_columns(table: pa.Table, value_set: pa.Array, path: Path) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """`(ordinal int32, rank int16, entity uint64)` for one read of a member table.

    The ordinal is the key's position in `value_set`, the roster in publication order. A `key`
    read as a dictionary is mapped through its dictionary, so a 10⁶-row row group costs one
    `index_in` over its distinct keys and one NumPy take; a row whose key the roster does not hold
    is a refusal naming it. `rank` null becomes −1.
    """
    keys = table.column("key")
    if keys.null_count:
        raise ValueError(f"{path.name}: {keys.null_count} member rows have a null key")
    parts = []
    for chunk in keys.chunks:
        if pa.types.is_dictionary(chunk.type):
            at = pc.fill_null(pc.index_in(chunk.dictionary, value_set=value_set), -1)
            part = at.to_numpy().astype(np.int32)[chunk.indices.to_numpy()]
            if (part < 0).any():
                unknown = chunk.dictionary.to_pylist()
                missing = [k for k, a in zip(unknown, at.to_pylist()) if a < 0][:3]
                raise ValueError(f"{path.name}: member rows name keys the roster does not: {missing}")
        else:
            at = pc.fill_null(pc.index_in(chunk, value_set=value_set), -1)
            part = at.to_numpy().astype(np.int32)
            if (part < 0).any():
                missing = pc.filter(chunk, pc.equal(at, -1)).to_pylist()[:3]
                raise ValueError(f"{path.name}: member rows name keys the roster does not: {missing}")
        parts.append(part)
    idx = parts[0] if len(parts) == 1 else np.concatenate(parts)
    ranks = table.column("rank")
    rank = pc.fill_null(ranks, 0).to_numpy().astype(np.int64)
    if rank.max(initial=0) >= np.iinfo(np.int16).max:
        raise ValueError(f"{path.name}: a content rank of {int(rank.max())} does not fit this pass")
    rank = rank.astype(np.int16)
    rank[pc.is_null(ranks).to_numpy(zero_copy_only=False)] = -1
    entity = table.column("entity").to_numpy().astype(np.uint64)
    return idx, rank, entity


def rank_groups(idx: np.ndarray, rank: np.ndarray, entity: np.ndarray):
    """Yield `(ordinal, {rank: entities})` for every ordinal present, in ordinal order.

    One `lexsort` over the rows, then the group boundaries are one `diff`.
    """
    if not len(idx):
        return
    order = np.lexsort((rank, idx))
    idx, rank, entity = idx[order], rank[order], entity[order]
    del order
    edges = np.flatnonzero((np.diff(idx) != 0) | (np.diff(rank) != 0)) + 1
    starts, ends = np.r_[0, edges], np.r_[edges, len(idx)]
    current, groups = int(idx[0]), {}
    for lo, hi in zip(starts, ends):
        ordinal = int(idx[lo])
        if ordinal != current:
            yield current, groups
            current, groups = ordinal, {}
        groups[int(rank[lo])] = entity[lo:hi]
    yield current, groups


class Publication:
    """One declared layer's roster, published in batches under a byte cap, as they are assembled.

    **Every artifact carries its whole member set**, base rows and ingested rows alike, addressed
    by external id, which is the address both halves share. The batch is the commit unit at the
    route, so the cap splits *between* artifacts. An artifact whose whole body would exceed
    `--publish-max-bytes` is published with its key, content, parents and as many members as fit
    under the cap, then **grown** through `PATCH /control/layers/{name}/artifacts` in slices of at
    most the cap, in the same parent-before-child order and after the batch that published it
    (decision 0127; contracts §3.4). `grown_artifacts`, `grow_requests` and `grown_members` record
    that path. Only an artifact whose key, content and parents alone do not fit is declined, and
    recorded with its key, member count and body bytes before its member list is ever built.

    **The member table is read in artifact order once, and never held whole.** A layer's member
    table can be 1.66×10⁹ rows (rung 3's DAG membership), and a driver that inverted it in memory
    to address it per artifact held ~25 GB of bodies for it and declined the layer instead. Two
    readers, chosen from the file's own row-group statistics:

    * **Streamed**, where consecutive row groups' key ranges do not overlap (a k-means member file,
      written one cluster after another) and key order is a parent-before-child order for this
      layer. Row groups are read one at a time; a key closes when the next row group's minimum is
      past it, so what is live is the row groups spanning one key boundary. A key that reappears
      after closing is a refusal, so the statistics are checked rather than trusted.
    * **Partitioned**, otherwise. One pass over `key` and `rank` counts each artifact's rows, and
      buckets are planned as ranges of the publication order under `--publish-bucket-rows` (an
      artifact over the budget has a bucket to itself). One pass writes every row as a 14-byte
      record into its bucket under `--work`; then one bucket at a time is read back, sorted, and
      published. Peak memory is one bucket plus one artifact's body, whatever the table's size.
      Buckets are ranges of the publication order, so a parent is never in a later bucket than
      its child. An artifact whose membership alone already exceeds the route's cap is declined
      at planning and its rows are not written.

    **The roster's `parent` list travels as the artifact's `parent`**, which is where a `dag`
    layer's edges are spelled and the only place they are (decision 0125); `edges_published`
    counts what landed, against `edges_declared`. Parents are published before their children
    ([`in_parent_order`]): a parent must already exist or sit earlier in the same batch, an
    ordering an edge has always carried (`annotation-representation.md` §5.0.4).

    **A declined artifact is not held, and a child's edge to it is dropped before sending.** The
    route refuses an artifact whose parent the layer does not hold, so an edge to a declined parent
    would refuse every descendant's batch and the census would list the whole tree as missing
    rather than the artifacts that were declined. The child is kept, its edge to the declined
    parent is dropped, and `edges_dropped_to_declined` counts them beside `edges_published`. A
    parent whose batch the route **refused** is a different case: its children's batches are
    refused too, each refusal is counted and logged, and the census then lists the subtree.
    """

    def __init__(
        self,
        roster: Path,
        members: Path | None,
        work: Path,
        max_bytes: int,
        bucket_rows: int,
        limits: dict,
    ):
        self.table = in_parent_order(pq.read_table(roster))
        self.rows = self.table.to_pylist()
        self.keys = [row["key"] for row in self.rows]
        self.held = set(self.keys)
        self.members_path = members
        self.work = work
        # **Every cap is the served deployment's**, read from `/control/status`'s `limits` block
        # ([`Cycle.served_limits`]): `--publish-max-bytes` is clamped to the publication route's
        # byte cap, a batch closes at its artifact count, and a growth slice stays under both the
        # growth route's byte cap and its member count. A value carried by the driver would be a
        # second number that can disagree with the one the route refuses over.
        self.max_bytes = min(max_bytes, int(limits["publish"]["max_body_bytes"]))
        self.max_artifacts = int(limits["publish"]["max_artifacts_per_request"])
        self.grow_max_bytes = min(max_bytes, int(limits["grow"]["max_body_bytes"]))
        self.max_members = int(limits["grow"]["max_members_per_request"])
        self.bucket_rows = bucket_rows
        self.stats = {
            "artifacts": len(self.rows),
            "members": 0,
            "generating_set_entries": 0,
            "edges_declared": 0,
            "edges_in_roster_and_layer": 0,
            "artifacts_with_several_parents": 0,
            "read_path": None,
            "row_groups": None,
            "buckets": None,
            "count_s": None,
            "partition_s": None,
            "declined_artifacts": [],
            "edges_dropped_to_declined": 0,
            "grown_artifacts": 0,
            "grow_slices": 0,
            "grown_members_sent": 0,
        }
        self.declined_keys: set[str] = set()
        for row in self.rows:
            own = row.get("parent") or []
            in_layer = [key for key in own if key in self.held]
            self.stats["edges_declared"] += len(own)
            self.stats["edges_in_roster_and_layer"] += len(in_layer)
            self.stats["artifacts_with_several_parents"] += 1 if len(in_layer) > 1 else 0

    # -- the member table, one artifact at a time -----------------------------------------

    def members(self):
        """Yield `(roster index, {rank: entities})` for every artifact, parents before children."""
        if self.members_path is None:
            self.stats["read_path"] = "no member table"
            for i in range(len(self.rows)):
                yield i, {}
            return
        ranges = key_ranges(self.members_path)
        self.stats["row_groups"] = pq.ParquetFile(self.members_path).metadata.num_row_groups
        parents = [row.get("parent") for row in self.rows]
        if ranges is not None and grouped_by_key(ranges) and key_order_is_parent_order(self.keys, parents):
            self.stats["read_path"] = "streamed"
            yield from self._streamed(ranges)
        else:
            self.stats["read_path"] = "partitioned"
            yield from self._partitioned()

    def _streamed(self, ranges: Sequence[tuple[str, str]]):
        reader = pq.ParquetFile(self.members_path, read_dictionary=["key"])
        value_set = pa.array(self.keys, pa.string())
        by_key = sorted(range(len(self.keys)), key=self.keys.__getitem__)
        closed = np.zeros(len(self.keys), bool)
        window: list[tuple[np.ndarray, np.ndarray, np.ndarray]] = []
        next_to_close = 0
        for i in range(len(ranges)):
            group = reader.read_row_group(i, columns=["key", "rank", "entity"])
            idx, rank, entity = member_columns(group, value_set, self.members_path)
            del group
            if closed[idx].any():
                reopened = self.keys[int(idx[closed[idx]][0])]
                raise ValueError(
                    f"{self.members_path.name}: {reopened!r} has rows after the row group its "
                    f"statistics said it ended in; the file is not grouped by key"
                )
            window.append((idx, rank, entity))
            bound = ranges[i + 1][0] if i + 1 < len(ranges) else None
            closing = []
            while next_to_close < len(by_key) and (bound is None or self.keys[by_key[next_to_close]] < bound):
                closing.append(by_key[next_to_close])
                next_to_close += 1
            if not closing:
                continue
            closed[closing] = True
            idx, rank, entity = (np.concatenate(parts) for parts in zip(*window))
            done = closed[idx]
            groups = dict(rank_groups(idx[done], rank[done], entity[done]))
            keep = ~done
            window = [(idx[keep], rank[keep], entity[keep])] if keep.any() else []
            del idx, rank, entity, done, keep
            for ordinal in closing:
                yield ordinal, groups.pop(ordinal, {})

    def _partitioned(self):
        reader = pq.ParquetFile(self.members_path, read_dictionary=["key"])
        value_set = pa.array(self.keys, pa.string())
        count = len(self.keys)

        # One pass over key: each artifact's rows, for the bucket budget.
        t0 = time.perf_counter()
        rows_of = np.zeros(count, np.int64)
        for i in range(reader.metadata.num_row_groups):
            group = reader.read_row_group(i, columns=["key", "rank"])
            idx, _, _ = member_columns(
                group.append_column("entity", pa.nulls(group.num_rows, pa.uint64()).fill_null(0)),
                value_set, self.members_path,
            )
            del group
            rows_of += np.bincount(idx, minlength=count)
        self.stats["count_s"] = round(time.perf_counter() - t0, 2)

        # Buckets: ranges of the publication order under the row budget. An artifact over the
        # budget has a bucket to itself; its membership is published in slices, never declined.
        bucket_of = np.full(count, -1, np.int32)
        bucket_range: list[tuple[int, int]] = []
        start, filled = 0, 0
        for ordinal in range(count):
            if filled and filled + rows_of[ordinal] > self.bucket_rows:
                bucket_range.append((start, ordinal))
                start, filled = ordinal, 0
            bucket_of[ordinal] = len(bucket_range)
            filled += int(rows_of[ordinal])
        bucket_range.append((start, count))
        self.stats["buckets"] = len(bucket_range)
        del rows_of

        # One pass writing every row to its bucket, as 14-byte records.
        t0 = time.perf_counter()
        self.work.mkdir(parents=True, exist_ok=True)
        handles = [open(self.work / f"bucket-{b:05d}.bin", "wb") for b in range(len(bucket_range))]
        try:
            for i in range(reader.metadata.num_row_groups):
                group = reader.read_row_group(i, columns=["key", "rank", "entity"])
                idx, rank, entity = member_columns(group, value_set, self.members_path)
                del group
                bucket = bucket_of[idx]
                order = np.argsort(bucket, kind="stable")
                records = np.empty(len(idx), MEMBER_RECORD)
                records["idx"], records["rank"], records["entity"] = idx[order], rank[order], entity[order]
                bounds = np.searchsorted(bucket[order], np.arange(len(bucket_range) + 1))
                del idx, rank, entity, bucket, order
                for b, handle in enumerate(handles):
                    if bounds[b + 1] > bounds[b]:
                        handle.write(records[bounds[b]:bounds[b + 1]].tobytes())
                del records
        finally:
            for handle in handles:
                handle.close()
        self.stats["partition_s"] = round(time.perf_counter() - t0, 2)

        # One bucket at a time, sorted, every ordinal of its range yielded whether or not it has rows.
        for b, (lo, hi) in enumerate(bucket_range):
            path = self.work / f"bucket-{b:05d}.bin"
            records = np.fromfile(path, MEMBER_RECORD)
            path.unlink()
            groups = dict(rank_groups(records["idx"], records["rank"], records["entity"]))
            del records
            for ordinal in range(lo, hi):
                yield ordinal, groups.pop(ordinal, {})

    # -- bodies ----------------------------------------------------------------------------

    def _head(self, i: int) -> bytes:
        return b'{"key":' + json.dumps(self.rows[i]["key"]).encode() + b',"members":'

    def bodies(self):
        """Yield the requests in order: `("put", level, body, artifacts, members, edges)` as each
        batch fills, and `("grow", level, key, body, members)` for each slice that grows an artifact
        the batch before it published.

        A batch closes when the next artifact would take it over `--publish-max-bytes` or the
        route's artifact count, or sits on another level; the wrapper is one level per request. An
        artifact whose whole membership does not fit closes the batch it is in, so its slices
        follow the request that created it.
        """
        if self.members_path is not None:
            self.work.mkdir(parents=True, exist_ok=True)
        batch: list[bytes] = []
        size = 0
        level = None
        counts = [0, 0, 0]
        for i, groups in self.members():
            block, members_n, edges_n, remainder = self._block(i, groups)
            if block is None:
                continue
            row_level = int(self.rows[i].get("level") or 0)
            if batch and (
                row_level != level
                or size + len(block) + 1 > self.max_bytes
                or counts[0] >= self.max_artifacts
            ):
                yield "put", level, self._body(level, batch), *counts
                batch, size, counts = [], 0, [0, 0, 0]
            batch.append(block)
            size += len(block) + 1
            level = row_level
            counts[0] += 1
            counts[1] += members_n
            counts[2] += edges_n
            if remainder is not None:
                yield "put", level, self._body(level, batch), *counts
                batch, size, counts = [], 0, [0, 0, 0]
                yield from self._grow_slices(row_level, self.rows[i]["key"], remainder)
        if batch:
            yield "put", level, self._body(level, batch), *counts

    def _grow_body(self, level: int, key: str, members: np.ndarray) -> bytes:
        return (
            b'{"level":' + str(level).encode() + b',"addressing":"external","artifacts":[{"key":'
            + json.dumps(key).encode() + b',"members":' + json_list(members) + b"}]}"
        )

    def _grow_slices(self, level: int, key: str, members: np.ndarray):
        """`("grow", level, key, body, members)` for `members`, in slices under the growth
        route's byte cap and its member count."""
        fixed = len(self._grow_body(level, key, EMPTY_ENTITIES)) - 2
        per_slice = max(1, min((self.grow_max_bytes - fixed - 1) // 15, self.max_members))
        self.stats["grown_artifacts"] += 1
        self.stats["grown_members_sent"] += len(members)
        for start in range(0, len(members), per_slice):
            piece = members[start : start + per_slice]
            self.stats["grow_slices"] += 1
            yield "grow", level, key, self._grow_body(level, key, piece), len(piece)

    def _block(self, i: int, groups: dict) -> tuple[bytes | None, int, int, np.ndarray | None]:
        """One artifact's JSON with as many members as fit under the cap, its member and edge
        counts, and the members left to grow it with (None when the whole membership fit); or
        `(None, 0, 0, None)` for an artifact declined because its key, content and parents alone do
        not fit."""
        row = self.rows[i]
        parents = [key for key in (row.get("parent") or []) if key in self.held]
        # Parents are published before their children, so a parent's decline is known here.
        kept_parents = [key for key in parents if key not in self.declined_keys]
        self.stats["edges_dropped_to_declined"] += len(parents) - len(kept_parents)
        parents = kept_parents
        members = groups.get(-1, EMPTY_ENTITIES)
        self.stats["members"] += len(members)
        head = self._head(i)
        contents = row.get("contents") or []
        content_heads: list[tuple[bytes, np.ndarray]] = []
        for rank, values in enumerate(contents):
            generated = groups.get(rank, EMPTY_ENTITIES)
            self.stats["generating_set_entries"] += len(generated)
            content_heads.append((b'{"values":' + json.dumps(list(values)).encode() + b',"generated_from":', generated))
        tail = b""
        if parents:
            tail += b',"parent":' + json.dumps(parents).encode()
        if row.get("attached_layer"):
            tail += b',"attached_to":' + json.dumps(
                {
                    "layer": row["attached_layer"],
                    "level": int(row.get("attached_level") or 0),
                    "key": row["attached_key"],
                }
            ).encode()
        tail += b"}"
        # Sized before anything large is built: a declined artifact's list is never assembled, and
        # a grown one's is built only as far as the cap allows.
        fixed = len(head) + len(tail) + len(self._body(0, [b""]))
        if content_heads:
            fixed += len(b',"content":[') + 1 + sum(
                len(h) + json_list_bytes(len(g)) + 2 for h, g in content_heads
            )
        budget = self.max_bytes - fixed
        if budget < json_list_bytes(0):
            size = fixed + json_list_bytes(len(members))
            self.declined_keys.add(row["key"])
            self.stats["declined_artifacts"].append(
                {"key": row["key"], "members": len(members), "body_bytes": size, "content_counted": True}
            )
            return None, 0, 0, None
        fit = max(0, (budget - 1) // 15)
        remainder = None
        if fit < len(members):
            members, remainder = members[:fit], members[fit:]
        parts = [head, json_list(members)]
        if content_heads:
            parts.append(b',"content":[')
            parts.append(b",".join(h + json_list(g) + b"}" for h, g in content_heads))
            parts.append(b"]")
        parts.append(tail)
        return b"".join(parts), len(members), len(parents), remainder

    def _body(self, level: int, blocks: list[bytes]) -> bytes:
        return (
            b'{"level":' + str(level).encode() + b',"addressing":"external","artifacts":['
            + b",".join(blocks)
            + b"]}"
        )

    def cleanup(self) -> None:
        """Remove the layer's transient buckets. Called whether or not the publication finished."""
        if self.work.exists():
            shutil.rmtree(self.work, ignore_errors=True)
