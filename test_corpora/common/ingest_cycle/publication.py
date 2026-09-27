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

#: The most decimal digits a `u64` takes, and each power of ten below it.
_DIGITS = 20
_POWERS = np.array([10**k for k in range(1, _DIGITS)], np.uint64)

#: A member table's row, as the partitioning pass writes it: the artifact's ordinal in publication
#: order, the row's rank (-1 is the membership, `k` is `contents[k]`'s generating set) and the
#: entity.
MEMBER_RECORD = np.dtype([("idx", "<i4"), ("rank", "<i2"), ("entity", "<u8")])

EMPTY_ENTITIES = np.zeros(0, np.uint64)


def digit_counts(entities: np.ndarray) -> np.ndarray:
    """How many decimal digits each entity id takes."""
    values = np.asarray(entities, np.uint64)
    return 1 + np.searchsorted(_POWERS, values, side="right")


def json_list_bytes(entities: np.ndarray) -> int:
    """The length [`json_list`] produces for `entities`: `[` and `]`, and each id's digits, two
    quotes and a comma, bar the last comma."""
    if not len(entities):
        return 2
    return int(digit_counts(entities).sum()) + 3 * len(entities) + 1


def cells_within(entities: np.ndarray, budget: int) -> int:
    """How many leading `entities` [`json_list`] fits in `budget` bytes, brackets included."""
    if budget < 2:
        return 0
    cumulative = np.cumsum(digit_counts(entities) + 3)
    return int(np.searchsorted(cumulative, budget - 1, side="right"))


def json_list(entities: np.ndarray) -> bytes:
    """A JSON array of entity ids as decimal strings, the join field's values, as bytes: one
    `(n, 23)` byte array holding each id's quote, twenty digits, quote and comma, masked to the
    digits each id takes, then one copy, rather than a Python string built per member.
    """
    if not len(entities):
        return b"[]"
    values = np.asarray(entities, np.uint64).copy()
    cell = np.empty((len(values), _DIGITS + 3), np.uint8)
    cell[:, 0] = ord('"')
    for at in range(_DIGITS, 0, -1):
        cell[:, at] = (values % 10).astype(np.uint8) + ord("0")
        values //= 10
    cell[:, _DIGITS + 1] = ord('"')
    cell[:, _DIGITS + 2] = ord(",")
    keep = np.ones(cell.shape, bool)
    keep[:, 1 : _DIGITS + 1] = np.arange(_DIGITS) >= (_DIGITS - digit_counts(entities))[:, None]
    return b"[" + cell[keep].tobytes()[:-1] + b"]"


def roster_table(path: Path) -> pa.Table:
    """A roster, with `parent` as a list of keys whichever way the file spells it: a single
    parent may be written as a plain string, which would otherwise be counted as one edge per
    character. Normalised here so every reader below sees one shape.
    """
    table = pq.read_table(path)
    if "parent" not in table.schema.names:
        return table
    at = table.schema.get_field_index("parent")
    kind = table.schema.field(at).type
    if pa.types.is_list(kind) or pa.types.is_large_list(kind):
        return table
    listed = pa.array(
        [[] if parent is None else [parent] for parent in table.column("parent").to_pylist()],
        pa.list_(pa.string()),
    )
    return table.set_column(at, pa.field("parent", listed.type), listed)


def in_parent_order(table: pa.Table) -> pa.Table:
    """A roster's rows reordered by longest path to a root, so no artifact precedes a parent of
    its own. A parent the roster does not hold is ignored.
    """
    if "parent" not in table.schema.names:
        return table
    keys = table.column("key").to_pylist()
    parents = table.column("parent").to_pylist()
    at = {key: i for i, key in enumerate(keys)}
    depth = [-1] * len(keys)

    for start in range(len(keys)):
        # Iterative rather than recursive: a DAG is the caller's data, and a recursion limit is
        # not the refusal anyone wants to read. `on_stack` is the cycle guard.
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
    Where it holds, a member file sorted by key can be streamed and published in file order;
    where it does not, the file is partitioned into publication order first.
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
    """Whether consecutive row groups' key ranges never overlap, so every key's rows are
    contiguous, allowing one shared key at a boundary.
    """
    return all(a_max <= b_min for (_, a_max), (b_min, _) in zip(ranges, ranges[1:]))


def member_columns(table: pa.Table, value_set: pa.Array, path: Path) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """`(ordinal int32, rank int16, entity uint64)` for one read of a member table. The ordinal
    is the key's position in `value_set`, the roster in publication order; a row whose key the
    roster does not hold is a refusal naming it. `rank` null becomes -1.
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
    """Yield `(ordinal, {rank: entities})` for every ordinal present, in ordinal order: one
    `lexsort` over the rows, then the group boundaries are one `diff`.
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
    """One declared layer's roster, published in batches under a byte cap. Every artifact
    carries its whole member set, named by the join field `field`; one whose body would exceed
    `--publish-max-bytes` is published with as many members as fit, then grown through
    `PATCH /control/layers/{name}/artifacts`. Only an artifact whose key, content and parents
    alone do not fit is declined.

    The member table is read in artifact order once, never held whole: streamed where
    consecutive row groups' key ranges do not overlap and key order is parent-before-child, or
    partitioned via buckets under `--publish-bucket-rows` read back one at a time otherwise.

    The roster's `parent` list travels as the artifact's `parent`, published before their
    children ([`in_parent_order`]); a declined artifact's edge is dropped from its children so a
    census difference lands on it rather than on every descendant.
    """

    def __init__(
        self,
        roster: Path,
        members: Path | None,
        work: Path,
        max_bytes: int,
        bucket_rows: int,
        limits: dict,
        field: str,
        view_column: str | None = None,
        access_column: str | None = None,
    ):
        self.table = in_parent_order(roster_table(roster))
        #: The join field every member, as a decimal value, is named by.
        self.field = field
        #: The roster column naming the view each artifact belongs to, on a group-scoped layer.
        self.view_column = view_column
        #: The roster column each artifact's own access label is read from, sent as `access`.
        self.access_column = access_column
        self.rows = self.table.to_pylist()
        self.keys = [row["key"] for row in self.rows]
        self.held = set(self.keys)
        self.members_path = members
        self.work = work
        # Every cap is the served deployment's, read from `/control/status`'s `limits` block.
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
            # A roster may carry each artifact's members itself, as a list of entity ids.
            inline = "members" in self.table.schema.names
            self.stats["read_path"] = "the roster's members column" if inline else "no member table"
            for i, row in enumerate(self.rows):
                members = np.array(row["members"] or [], np.uint64) if inline else None
                yield i, {} if members is None else {-1: members}
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
        # budget has a bucket to itself.
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

    def _view(self, i: int) -> bytes:
        if self.view_column is None:
            return b""
        return b',"view":' + json.dumps(self.rows[i][self.view_column]).encode()

    def _head(self, i: int) -> bytes:
        access = b""
        if self.access_column:
            # Stated on every row, `null` for no label of its own: a layer reading labels refuses a
            # record that states none.
            labels = self.rows[i].get(self.access_column)
            if labels:
                labels = [labels] if isinstance(labels, str) else list(labels)
            access = b',"access":' + json.dumps(labels or None).encode()
        return (
            b'{"key":' + json.dumps(self.rows[i]["key"]).encode() + self._view(i) + access
            + b',"members":'
        )

    def bodies(self):
        """Yield the requests in order: `("put", level, body, artifacts, members, edges)` per
        batch, and `("grow", level, key, body, members)` per slice that grows an artifact the
        batch before it published. A batch closes at `--publish-max-bytes`, the artifact count,
        or a level change."""
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
                yield from self._grow_slices(row_level, i, remainder)
        if batch:
            yield "put", level, self._body(level, batch), *counts

    def _grow_body(self, level: int, i: int, members: np.ndarray) -> bytes:
        """One growth of roster row `i`, naming its view on a group-scoped layer."""
        return (
            b'{"level":' + str(level).encode() + b',"field":' + json.dumps(self.field).encode()
            + b',"artifacts":[{"key":' + json.dumps(self.rows[i]["key"]).encode() + self._view(i)
            + b',"members":' + json_list(members) + b"}]}"
        )

    def _grow_slices(self, level: int, i: int, members: np.ndarray):
        """`("grow", level, key, body, members)` for `members`, in slices under the growth
        route's byte cap and its member count, each slice sized at the widest member's width."""
        key = self.rows[i]["key"]
        fixed = len(self._grow_body(level, i, EMPTY_ENTITIES)) - 2
        widest = int(digit_counts(members).max(initial=1)) + 3
        per_slice = max(1, min((self.grow_max_bytes - fixed - 1) // widest, self.max_members))
        self.stats["grown_artifacts"] += 1
        self.stats["grown_members_sent"] += len(members)
        for start in range(0, len(members), per_slice):
            piece = members[start : start + per_slice]
            self.stats["grow_slices"] += 1
            yield "grow", level, key, self._grow_body(level, i, piece), len(piece)

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
        # Sized before anything large is built, so a declined artifact's list is never assembled.
        fixed = len(head) + len(tail) + len(self._body(0, [b""]))
        if content_heads:
            fixed += len(b',"content":[') + 1 + sum(
                len(h) + json_list_bytes(g) + 2 for h, g in content_heads
            )
        budget = self.max_bytes - fixed
        if budget < json_list_bytes(EMPTY_ENTITIES):
            size = fixed + json_list_bytes(members)
            self.declined_keys.add(row["key"])
            self.stats["declined_artifacts"].append(
                {"key": row["key"], "members": len(members), "body_bytes": size, "content_counted": True}
            )
            return None, 0, 0, None
        fit = cells_within(members, budget)
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
            b'{"level":' + str(level).encode() + b',"field":' + json.dumps(self.field).encode()
            + b',"artifacts":[' + b",".join(blocks)
            + b"]}"
        )

    def cleanup(self) -> None:
        """Remove the layer's transient buckets. Called whether or not the publication finished."""
        if self.work.exists():
            shutil.rmtree(self.work, ignore_errors=True)
