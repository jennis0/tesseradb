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

from .split import (
    declared_layers,
    declared_views,
    in_sorted,
    member_table_columns,
    read_view_rows,
    source_path,
)

# ---------------------------------------------------------------------------------------------
# The hold-out, as ingest batches
# ---------------------------------------------------------------------------------------------


def wire_columns(rung: Path, view: dict | None = None) -> tuple[str | None, list[str], list[dict]]:
    """`(access column, attribute columns, joined files)` for one view's batches, the anchor's by
    default, read off the rung's own declaration: the view's `point_visibility.field`, and every
    `[[attribute]]` that travels with a point there. One read from a file of its own is joined
    on entity id, as the build reads it beside the points: each joined file is `(file, columns,
    select, fields)`. A group-scoped attribute travels only on a view of its group or of one
    sharing its keys, its rows picked by its file's discriminator, `view` unless `fields.view`
    renames it."""
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})
    entity = declared.get("defaults", {}).get("entity_id_field", "entity_id")
    view = view or declared_views(rung)[0]
    access = view["point_visibility"].get("field")
    held = set(pq.ParquetFile(view["points"]).schema_arrow.names)
    attributes: list[str] = []
    joined: dict = {}
    for attribute in declared.get("attribute", []):
        group = scope_group(attribute)
        if group not in (None, view["owner"]):
            continue
        own = source_path(rung, named, attribute.get("source"))
        if own in (None, view["points"]):
            if attribute["name"] in held:
                attributes.append(attribute["name"])
            continue
        column = (attribute.get("fields") or {}).get("view", "view")
        select = (column, view["key"]) if group is not None else None
        entry = joined.setdefault(
            (own, select),
            {
                "file": own,
                "columns": [],
                "select": select,
                "fields": {"entity_id": (attribute.get("fields") or {}).get("entity_id", entity)},
            },
        )
        entry["columns"].append(attribute["name"])
        attributes.append(attribute["name"])
    return access, attributes, list(joined.values())


def scope_group(attribute: dict) -> str | None:
    """The group an attribute's values vary by, or None for one value per entity."""
    scope = attribute.get("scope")
    return scope.get("group") if isinstance(scope, dict) else None


def encode_batch(
    table: pa.Table,
    coordinates: Sequence[str],
    access: str | None,
    attributes: list[str],
    columns: Sequence[str] = (),
) -> bytes:
    """One Arrow IPC stream for a slice of the hold-out. `coordinates` is the view's pair,
    `lon`/`lat` for a projected view and `x`/`y` for one with none, which the table and the wire
    both spell so. `access` is the wire's list of labels, a null becoming the empty list for the
    view's declaration to interpret. `external_id` is the source entity id, eight bytes
    little-endian, the build's own form. `columns` names the column-route layers, already named
    for the layer they belong to."""
    entities = table.column("entity_id").to_pylist()
    names = list(coordinates)
    arrays = [table.column(name).cast(pa.float64()).combine_chunks() for name in names]
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
    # Every declared attribute, the access column included: the scalar tail is read back by
    # position, so an omission misaligns it exactly as a spurious column does. A column that is
    # also the compartment attribute is sent twice on purpose — once as `access`, once as itself.
    for name in attributes:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    for name in columns:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    batch = pa.RecordBatch.from_arrays(arrays, names=names)
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class MemberStream:
    """A column-route layer's member table, read in lockstep with the points file, one row
    group at a time and filtered to the hold-out. An entity the table does not name gets a null
    cell; the count is recorded."""

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
    """The held-back rows, streamed out of the rung's own parquet as ingest batches, never
    materialised whole. Only `head_rows` rows are kept, for the write cycle, which needs the
    same bytes twice.

    A body is bounded by both of the route's caps, read from the served deployment's `limits`
    block: the row cap sizes a slice exactly, and a slice over the byte cap is halved by
    [`bodies`] until every piece fits."""

    def __init__(
        self,
        rung: Path,
        held: np.ndarray,
        max_body_bytes: int,
        batch_rows: int,
        head_rows: int = 0,
        log=print,
        view: dict | None = None,
        members: Sequence[str] = (),
        record_order: bool = False,
    ):
        #: The view's own points file: its positions, its access column, and whichever declared
        #: attributes it holds; and the `(column, key)` picking the view's rows out of a file
        #: several views share.
        view = view or declared_views(rung)[0]
        self.points = view["points"]
        self.select = view["select"]
        #: The view's coordinate pair, and the file's spelling of each canonical column a batch
        #: reads by name.
        self.coordinates = [name for name in view["fields"] if name not in ("entity_id", "view")]
        self.renamed = {
            spelt: name for name, spelt in view["fields"].items() if name != "view" and spelt != name
        }
        self.batch_rows = batch_rows
        self.held = np.sort(held)
        self.access, self.attributes, joined = wire_columns(rung, view)
        #: Each joined file's rows for the hold-out, read once: small beside the points.
        self.joined = [
            read_view_rows(
                {"points": entry["file"], "select": entry["select"], "fields": entry["fields"]},
                ["entity_id", *entry["columns"]],
                self.held,
            )
            for entry in joined
        ]
        self.head_rows = head_rows
        self.max_body_bytes = int(max_body_bytes)
        self.log = log
        self.body_stats = self.new_body_stats()
        self.head: pa.Table | None = None
        #: The column-route layers in `members`, whose member lists travel as batch columns.
        self.members = [
            MemberStream(layer["name"], layer["members"], self.held)
            for layer in declared_layers(rung)
            if layer["route"] == "column" and layer["name"] in members
        ]
        self.columns = [stream.name for stream in self.members]
        self.member_stats = {stream.name: stream.stats for stream in self.members}
        self.last_entity = -1
        #: Each row's entity id in the order the batches send them, where asked for: what the row
        #: index a body starts at is an index into.
        self.order: list[int] | None = [] if record_order else None

    def emit(self, table: pa.Table, start: int):
        """[`bodies`] over one slice, its entity ids recorded first where `order` is kept."""
        if self.order is not None:
            self.order += table.column("entity_id").to_pylist()
        yield from self.bodies(table, start)

    @staticmethod
    def new_body_stats() -> dict:
        return {"bodies": 0, "slices": 0, "bodies_split": 0, "largest_body_bytes": 0, "over_cap": 0}

    def bodies(self, table: pa.Table, start: int, stats: dict | None = None):
        """Yield `(first row index, body bytes, row count)` for one slice, every body under the
        cap: a body over it is halved and each half encoded again. A single row over the cap
        cannot be split, so `over_cap` counts it."""
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
        """Yield `(first row index, body bytes, row count)` for the whole hold-out, in slices of
        `rows`, a row group at a time through `read_row_group` rather than `iter_batches`, which
        keeps every yielded group alive for the iterator's life."""
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
            if self.select is not None:
                group = group.filter(pc.equal(group.column(self.select[0]), self.select[1]))
            if self.renamed:
                group = group.rename_columns([self.renamed.get(n, n) for n in group.column_names])
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
            for values in self.joined:
                at = pc.index_in(table.column("entity_id"), value_set=values.column("entity_id"))
                for name in values.column_names[1:]:
                    table = table.append_column(name, pc.take(values.column(name), at))
            if head_rows < self.head_rows:
                take = min(self.head_rows - head_rows, table.num_rows)
                head.append(table.slice(0, take))
                head_rows += take
            pending.append(table)
            pending_rows += table.num_rows
            while pending_rows >= rows:
                whole = pa.concat_tables(pending)
                yield from self.emit(whole.slice(0, rows), emitted)
                emitted += rows
                rest = whole.slice(rows)
                pending = [rest] if rest.num_rows else []
                pending_rows = rest.num_rows
        if pending_rows:
            whole = pa.concat_tables(pending)
            yield from self.emit(whole, emitted)
        if head:
            self.head = pa.concat_tables(head)

    def encode(self, table: pa.Table) -> bytes:
        """[`encode_batch`] over a slice of this hold-out, member columns included."""
        return encode_batch(table, self.coordinates, self.access, self.attributes, self.columns)

