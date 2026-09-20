from __future__ import annotations

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
from pyarrow import ipc

from .split import declared_layers, in_sorted, member_table_columns

# ---------------------------------------------------------------------------------------------
# The hold-out, as ingest batches
# ---------------------------------------------------------------------------------------------


def wire_columns(rung: Path, points: Path | None = None) -> tuple[str | None, list[str]]:
    """`(access column, attribute columns)` for the hold-out's batches, **read off the rung's own
    declaration** rather than listed here.

    The batch a rung's hold-out is sent as is not a property of this driver: hard-coding MedCPT's
    four made the driver refuse rung 4 after building its 92M-row base, on a `KeyError` for a
    column that rung does not have.

    The access column is the first `point_visibility.field` any view declares — a rung compartments
    on one column, and the wire takes one list of labels a row; the rung's column may be a list per
    row (rung 3 and MedCPT) or one string (rung 4's licence, rung 5's publisher), and
    [`encode_batch`] sends both as the list (decision 0129). The attribute
    columns are every `[[attribute]]` the declaration names that `points` actually holds, which is
    what makes the ingested rows carry the same columns the built ones do; a rung whose points file
    does not hold one of them is a rung whose build would have refused too.

    **A view's own file decides what its pass carries.** An attribute is entity-space and rides the
    anchor's file alone (arXiv's second view carries identity, position and the access column and
    nothing else), so a second view's pass sends a position and a label for an entity that already
    exists and restates nothing declared once for the whole item.
    """
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    access = None
    for view in declared.get("view", []):
        field = (view.get("point_visibility") or {}).get("field")
        if field:
            access = field
            break
    held = set(pq.ParquetFile(points or rung / "points.parquet").schema_arrow.names)
    attributes = [a["name"] for a in declared.get("attribute", []) if a["name"] in held]
    return access, attributes


def encode_batch(
    table: pa.Table, access: str | None, attributes: list[str], columns: Sequence[str] = ()
) -> bytes:
    """One Arrow IPC stream for a slice of the hold-out.

    `access` is the wire's **list of labels**, one element per label, each taken verbatim
    (contracts §3.4, decision 0129): a rung's list column travels as itself, a scalar compartment
    column as one-element lists, and a null — a null scalar or a null list — as the empty list,
    which the server would otherwise refuse for the whole batch. **The empty list is a row with no
    label, and the view's declaration decides it at both entry points** (decision 0133): where the
    view declares a `point_visibility.default` the server gives the row that label, as the build
    gives it to a null or empty value; where it declares none the server refuses the batch naming
    the count, as the build refuses the corpus. This driver applies no default of its own, so an
    ingest cycle takes the same declaration the build took. Nothing here joins or splits a label, so a compartment
    key containing a comma is one term on both sides of the split. `external_id` is the **source entity id, eight bytes little-endian**, the same form the
    build mints under `--mint-external-ids` (see [`external_ids`]). That is what makes an ingested
    row addressable on `/control/changes` afterwards, and what an artifact's `members` names it by
    on the same footing as a base row. Every declared attribute travels beside it, by the name the
    declaration gives it — see [`wire_columns`].

    `columns` names the column-route layers (the module doc): each is a column of `table` already
    named for the layer, carrying the row's member list as the member table spells it — one entry
    per declared level, null where the row is in no artifact at that level — and it travels as
    itself. A publication-route layer has no column here, because the column would name artifacts
    that do not exist yet and a layer declaring supplied content refuses to mint them.
    """
    entities = table.column("entity_id").to_pylist()
    arrays = [
        table.column("x").cast(pa.float64()).combine_chunks(),
        table.column("y").cast(pa.float64()).combine_chunks(),
    ]
    names = ["x", "y"]
    if access is not None:
        column = table.column(access).combine_chunks()
        if isinstance(column, pa.ChunkedArray):
            column = pa.concat_arrays(column.chunks) if column.num_chunks else pa.array([], column.type)
        if pa.types.is_list(column.type) or pa.types.is_large_list(column.type):
            column = column.cast(pa.list_(pa.string()))
            lengths = pc.fill_null(pc.list_value_length(column), 0).to_numpy(zero_copy_only=False)
            offsets = np.concatenate([[0], np.cumsum(lengths)]).astype(np.int32)
            column = pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), pc.list_flatten(column))
        else:
            values = column.cast(pa.string())
            lengths = pc.cast(pc.is_valid(values), pa.int32()).to_numpy(zero_copy_only=False)
            offsets = np.concatenate([[0], np.cumsum(lengths)]).astype(np.int32)
            column = pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), values.drop_null())
        arrays.append(column)
        names.append("access")
    arrays.append(pa.array([int(e).to_bytes(8, "little") for e in entities], pa.binary()))
    names.append("external_id")
    # **Every declared attribute, the access column included.** The scalar tail is read back by
    # position, so an omission misaligns it exactly as a spurious column does — and a rung whose
    # compartment is also a rendered attribute (rung 5's `publisher`) sends it twice on purpose:
    # once as `access`, the plugin's own descriptor list, and once as the column itself.
    for name in attributes:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    for name in columns:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    batch = pa.RecordBatch.from_arrays(
        [pa.array(a) if not isinstance(a, pa.Array) else a for a in arrays], names=names
    )
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class MemberStream:
    """A column-route layer's member table, read in lockstep with the points file.

    Both files ascend by entity — the rung's preparation writes them so, and this checks it a row
    group at a time rather than trusting it — so the member rows a points batch needs are the ones
    up to its last entity. One row group is decoded at a time, filtered to the hold-out, and what is
    live is the rows past the last batch's entities; at rung 5's 2.3×10⁸ member rows nothing is held
    whole. A hold-out entity the table does not name is in no artifact and gets a null cell, which
    is what the build reads for a point the member table leaves out; the count is recorded.
    """

    def __init__(self, name: str, path: Path, held: np.ndarray):
        self.name = name
        self.path = path
        self.reader = pq.ParquetFile(path)
        self.entity_column, self.key_column = member_table_columns(self.reader.schema_arrow)
        self.key_type = self.reader.schema_arrow.field(self.key_column).type
        self.held = held
        self.group = 0
        self.pending: list[pa.Table] = []
        self.last_entity = -1
        self.stats = {"file": path.name, "rows_with_keys": 0, "rows_without": 0}

    def _pull(self) -> bool:
        """Decode the next row group into `pending`, filtered to the hold-out; False when done."""
        if self.group >= self.reader.metadata.num_row_groups:
            return False
        table = self.reader.read_row_group(self.group, columns=[self.entity_column, self.key_column])
        self.group += 1
        entities = table.column(self.entity_column).to_numpy()
        if len(entities):
            if int(entities[0]) <= self.last_entity or np.any(np.diff(entities.astype(np.int64)) <= 0):
                raise ValueError(
                    f"{self.path.name}: `{self.entity_column}` is not strictly ascending across row "
                    f"group {self.group - 1}; the driver reads a member table in lockstep with the "
                    f"points file and needs both in entity order"
                )
            self.last_entity = int(entities[-1])
        table = table.filter(pa.array(in_sorted(entities, self.held)))
        if table.num_rows:
            self.pending.append(table)
        return True

    def keys_for(self, entities: np.ndarray) -> pa.Array:
        """The member list of each of `entities` (ascending), null where the table names none."""
        if len(entities) == 0:
            return pa.array([], self.key_type)
        top = int(entities[-1])
        while self.last_entity < top and self._pull():
            pass
        have = pa.concat_tables(self.pending) if self.pending else None
        if have is None or have.num_rows == 0:
            self.stats["rows_without"] += len(entities)
            return pa.nulls(len(entities), self.key_type)
        wanted = pa.array(entities).cast(have.column(self.entity_column).type)
        index = pc.index_in(wanted, value_set=have.column(self.entity_column).combine_chunks())
        keys = pc.take(have.column(self.key_column).combine_chunks(), index)
        missing = index.null_count
        self.stats["rows_without"] += missing
        self.stats["rows_with_keys"] += len(entities) - missing
        rest = have.filter(pc.greater(have.column(self.entity_column), top))
        self.pending = [rest] if rest.num_rows else []
        return keys


class HoldOut:
    """The held-back rows, streamed out of the rung's own parquet as ingest batches.

    **Streamed, never materialised.** At *f* = 100% of rung 3 the hold-out is the whole corpus —
    4 GB of parquet, tens of gigabytes of Arrow — and holding it beside a running server on a
    47 GB box is the run failing for a reason that has nothing to do with what it measures. Only
    `head_rows` rows are kept, for the write cycle, which needs the same bytes twice.

    A column-route layer's member list rides each batch as the column named for the layer, joined
    from a [`MemberStream`] read in lockstep with the points; `member_stats` records, per layer, how
    many hold-out rows the table named and how many it did not.

    **A body is bounded by both of the route's caps, and both are read from the served
    deployment's `limits` block** ([`Cycle.served_limits`]). The row cap (`batch_rows`, the
    deployment's `ingest_max_batch_rows`) sizes a slice; the driver sends exactly it, so a run also
    exercises the cap's own boundary. The byte cap (`max_body_bytes`, the deployment's
    `ingest_max_batch_bytes`) is enforced on the route before decoding, and
    a 10,000-row slice of a rung with a text attribute can exceed it: rung 4's abstracts put a
    10,000-row body near the 16 MiB cap, and 19 of the 92M cell's slices went over it. [`bodies`] encodes the slice and, where the body is over the cap, halves the slice and
    encodes each half again until every piece fits, in row order; each piece keeps its own
    first-row index, so the batch id the caller derives from it stays unique. `body_stats` counts
    the bodies sent, the bodies that were over the cap and split (a half that is still over counts
    again), the bodies sent over the cap because they were one row, and the largest body sent.
    """

    def __init__(
        self,
        rung: Path,
        held: np.ndarray,
        max_body_bytes: int,
        batch_rows: int,
        head_rows: int = 0,
        log=print,
        points: Path | None = None,
        members: bool = True,
    ):
        #: The view's own points file: its positions, its access column, and whichever declared
        #: attributes it holds.
        self.points = points or rung / "points.parquet"
        self.batch_rows = batch_rows
        self.access, self.attributes = wire_columns(rung, self.points)
        self.held = np.sort(held)
        self.head_rows = head_rows
        self.max_body_bytes = int(max_body_bytes)
        self.log = log
        self.body_stats = self.new_body_stats()
        self.head: pa.Table | None = None
        # Membership is entity-space and travels once, with the pass that allocates the entities.
        self.members = [
            MemberStream(layer["name"], layer["members"], self.held)
            for layer in declared_layers(rung)
            if layer["route"] == "column" and members
        ]
        self.columns = [stream.name for stream in self.members]
        self.member_stats = {stream.name: stream.stats for stream in self.members}
        self.last_entity = -1

    @staticmethod
    def new_body_stats() -> dict:
        return {"bodies": 0, "slices": 0, "bodies_split": 0, "largest_body_bytes": 0, "over_cap": 0}

    def bodies(self, table: pa.Table, start: int, stats: dict | None = None):
        """Yield `(first row index, body bytes, row count)` for one slice, every body under the cap.

        The slice is encoded whole. A body over `max_body_bytes` is not sent: the slice is halved
        and each half is encoded again, so the pieces come out in row order and each carries the
        index of its first row. Exact rather than estimated: the body that is sent is the body
        that was measured, so a piece under the cap here is under it on the route. A single row
        whose body is over the cap cannot be split; it is sent as it is, so the route's 422 is
        recorded in `statuses` and `first_refusal` and `over_cap` counts it. The first split is
        logged.
        """
        stats = self.body_stats if stats is None else stats
        stats["slices"] += 1
        pending = [(start, table)]
        while pending:
            first, piece = pending.pop()
            body = self.encode(piece)
            if len(body) > self.max_body_bytes and piece.num_rows > 1:
                half = piece.num_rows // 2
                # Pushed in reverse so the earlier half is encoded and yielded first.
                pending.append((first + half, piece.slice(half)))
                pending.append((first, piece.slice(0, half)))
                stats["bodies_split"] += 1
                if stats["bodies_split"] == 1:
                    self.log(
                        f"  a {piece.num_rows:,}-row slice encodes to {len(body):,} B, over the "
                        f"{self.max_body_bytes:,}-byte cap; splitting until every body fits"
                    )
                continue
            if len(body) > self.max_body_bytes:
                stats["over_cap"] += 1
            stats["bodies"] += 1
            stats["largest_body_bytes"] = max(stats["largest_body_bytes"], len(body))
            yield first, body, piece.num_rows

    def batches(self, rows: int | None = None):
        """Yield `(first row index, body bytes, row count)` for the whole hold-out, in file order,
        in slices of `rows` (the served row cap unless given).

        **One row group at a time through `read_row_group`, not `iter_batches`.** Measured on
        rung 4's 52 GB points file (`probes/2026-09-05-holdout-memory/`): pyarrow 25's
        `iter_batches` reader keeps about 150 MB of every row group it has yielded alive in
        Arrow's pool for the life of the iterator, whatever the caller drops and whichever
        allocator backs the pool (`mimalloc` and `system` measured), so the driver reached 38 GB by
        900 batches and the cell stalled. `read_row_group` holds one decoded row group at a time
        and the same file streams whole with the driver under 3 GB.
        """
        rows = self.batch_rows if rows is None else rows
        self.body_stats = self.new_body_stats()
        reader = pq.ParquetFile(self.points)
        pending: list[pa.Table] = []
        pending_rows = 0
        emitted = 0
        head: list[pa.Table] = []
        head_rows = 0
        for index in range(reader.metadata.num_row_groups):
            group = reader.read_row_group(index)
            table = group.filter(pa.array(in_sorted(group.column("entity_id").to_numpy(), self.held)))
            del group
            if table.num_rows == 0:
                continue
            if self.members:
                entities = table.column("entity_id").to_numpy()
                if int(entities[0]) <= self.last_entity or np.any(np.diff(entities.astype(np.int64)) <= 0):
                    raise ValueError(
                        f"{self.points.name}: `entity_id` is not strictly ascending across row group "
                        f"{index}; a member table is read in lockstep with it and needs both in "
                        f"entity order"
                    )
                self.last_entity = int(entities[-1])
                for stream in self.members:
                    table = table.append_column(stream.name, stream.keys_for(entities))
            if head_rows < self.head_rows:
                take = min(self.head_rows - head_rows, table.num_rows)
                head.append(table.slice(0, take))
                head_rows += take
            pending.append(table)
            pending_rows += table.num_rows
            while pending_rows >= rows:
                whole = pa.concat_tables(pending)
                yield from self.bodies(whole.slice(0, rows), emitted)
                emitted += rows
                rest = whole.slice(rows)
                pending = [rest] if rest.num_rows else []
                pending_rows = rest.num_rows
        if pending_rows:
            whole = pa.concat_tables(pending)
            yield from self.bodies(whole, emitted)
        if head:
            self.head = pa.concat_tables(head)

    def encode(self, table: pa.Table) -> bytes:
        """[`encode_batch`] over a slice of this hold-out, member columns included."""
        return encode_batch(table, self.access, self.attributes, self.columns)
