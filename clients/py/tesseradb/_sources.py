"""Staging: binding a name in `[sources]` to a frame or a file (python-sdk.md §3).

A frame is written to `sources/<name>.parquet` with the `entity_id` column the build reads; a path
is recorded and read in place, so a large file is not copied. Which of the two happens to a path
is the in-place rule below.

Frames arrive through pyarrow. `pa.table(obj)` takes a pandas frame, a pyarrow table and anything
implementing the Arrow C stream protocol, which polars does, so pyarrow is the only dependency the
`local` extra needs and pandas is optional. The one thing this module asks a frame about directly
is its index, which is pandas-specific and is detected by duck typing rather than by importing
pandas.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

from ._idmap import IdMap

#: The columns that name an entity in a source the user did not write for Tessera.
ENTITY_COLUMNS = ("entity", "entity_id")


@dataclass
class StagedSource:
    """One entry of `[sources]`, and what the SDK knows about the data behind it."""

    name: str
    path: Path
    #: The path as `[sources]` spells it: relative to the declaration, never absolute (§8).
    declared_path: str
    default: bool = False
    #: Read where it lies rather than rewritten, the map being the identity over its ids.
    in_place: bool = False
    #: The column identifying a row in the user's own terms, kept as an indexed `keyword`
    #: attribute under its own name where the source is one a view reads.
    user_id_column: str | None = None
    #: The ids were the frame's own row positions: accepted at the first commit, refused on a
    #: delta.
    default_index: bool = False
    rows: int | None = None
    columns: dict[str, pa.DataType] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)
    #: The user ids this staging assigned or re-used, in staging order.
    user_ids: list[Any] = field(default_factory=list)

    def table(self) -> pa.Table:
        """The data, read back. Schemas are cheap; this reads rows and is used for inference."""
        return pq.read_table(self.path)


class Refusal(ValueError):
    """What the SDK refuses, in the design's own terms."""


def is_pandas_frame(data: Any) -> bool:
    return hasattr(data, "index") and hasattr(data, "columns") and hasattr(data, "dtypes")


def to_table(data: Any, id: str | None) -> tuple[pa.Table, str | None, bool, list[Any] | None]:
    """A frame as an Arrow table, with the column that identifies a row.

    Returns the table, the name of the user's id column, whether the ids are row positions, and
    the id values where they come from a pandas index rather than from a column.
    """
    if is_pandas_frame(data):
        index = data.index
        if id is None:
            if index.name is not None:
                table = pa.table(data)
                return table, str(index.name), False, list(index)
            return pa.table(data), None, True, None
        table = pa.table(data)
        if id not in table.column_names:
            raise Refusal(f"id={id!r} names no column of this frame: {table.column_names}")
        return table, id, False, None
    table = data if isinstance(data, pa.Table) else pa.table(data)
    if id is not None:
        if id not in table.column_names:
            raise Refusal(f"id={id!r} names no column of this frame: {table.column_names}")
        return table, id, False, None
    return table, None, False, None


def stage_frame(
    name: str,
    data: Any,
    directory: Path,
    id_map: IdMap,
    id: str | None = None,
    default: bool = False,
    first_commit: bool = True,
) -> StagedSource:
    table, id_column, default_index, index_values = to_table(data, id)
    notes: list[str] = []

    entity_column = next((c for c in ENTITY_COLUMNS if c in table.column_names), None)
    # A source names entities when the caller says which column does, when it carries an
    # entity-naming column, or when it is the default source, which is the points file. A
    # vocabulary's values and a layer's artifacts name no entity and take no id: minting source
    # ids for their rows would spend the id space on objects that are not entities.
    if id_column is None and entity_column is None and not default:
        pq.write_table(table, _sources_path(directory, name))
        return StagedSource(
            name=name,
            path=_sources_path(directory, name),
            declared_path=f"sources/{name}.parquet",
            default=default,
            rows=table.num_rows,
            columns=dict(zip(table.schema.names, table.schema.types)),
            notes=["names no entities: written as staged"],
        )

    if id_column is not None:
        if index_values is not None and id_column not in table.column_names:
            # A pandas index that `pa.table` kept as metadata rather than as a column: the id has
            # to be in the file, since the build reads columns and the attribute names one.
            table = table.append_column(id_column, pa.array(list(index_values)))
        user_ids = table[id_column].to_pylist()
        target = "entity_id"
    elif entity_column is not None:
        user_ids = table[entity_column].to_pylist()
        target = entity_column
        notes.append(f"'{entity_column}' mapped through the id map")
    else:
        # An unnamed default index: the row position, which is a name only while the frame is the
        # whole corpus. A delta's positions name nothing, so the caller is sent to `id=`.
        if not first_commit:
            raise Refusal(
                f"source {name!r}: a default index names nothing on a delta — a filtered or reset "
                f"frame's positions are not its rows' identities. Name the id column with id="
            )
        user_ids = list(range(table.num_rows))
        target = "entity_id"
        notes.append("the frame's default index: ids are row positions")

    source_ids = id_map.source_ids(user_ids)
    column = pa.array(source_ids, type=pa.uint64())
    if target in table.column_names:
        table = table.set_column(table.column_names.index(target), target, column)
    else:
        table = table.append_column(target, column)

    path = _sources_path(directory, name)
    pq.write_table(table, path)
    return StagedSource(
        name=name,
        path=path,
        declared_path=f"sources/{name}.parquet",
        default=default,
        user_id_column=id_column if entity_column is None else None,
        default_index=default_index,
        rows=table.num_rows,
        columns=dict(zip(table.schema.names, table.schema.types)),
        notes=notes,
        user_ids=user_ids,
    )


def stage_path(
    name: str,
    data: str | os.PathLike,
    directory: Path,
    id_map: IdMap,
    id: str | None = None,
    default: bool = False,
) -> StagedSource:
    """A file. Read in place where its ids already are what the build reads, rewritten where not.

    A points file whose `entity_id` column is an integer is read where it lies and the map is the
    identity over its ids, so no keyword attribute is written: the user's id is the source id. An
    entity-naming column of any other file with integer values is read in place on the same
    ground, so a members table beside an in-place points file names the same entities.
    """
    path = Path(data).expanduser().resolve()
    if not path.exists():
        raise Refusal(f"source {name!r}: {path} does not exist")
    schema = pq.ParquetFile(path).schema_arrow
    columns = dict(zip(schema.names, schema.types))

    entity_column = id or next((c for c in ENTITY_COLUMNS if c in columns), None)
    if entity_column is None:
        # A vocabulary's values or a layer's artifacts: no entity is named, so there is nothing to
        # map and the file is read where it lies.
        return StagedSource(
            name=name,
            path=path,
            declared_path=_relative(path, directory),
            default=default,
            in_place=True,
            rows=pq.ParquetFile(path).metadata.num_rows,
            columns=columns,
            notes=["names no entities: read in place"],
        )
    if entity_column not in columns:
        raise Refusal(f"source {name!r}: id={entity_column!r} names no column of {path.name}")

    if id is None and pa.types.is_integer(columns[entity_column]):
        return StagedSource(
            name=name,
            path=path,
            declared_path=_relative(path, directory),
            default=default,
            in_place=True,
            rows=pq.ParquetFile(path).metadata.num_rows,
            columns=columns,
            notes=[f"read in place: '{entity_column}' is an integer, so the map is the identity"],
        )

    table = pq.read_table(path)
    user_ids = table[entity_column].to_pylist()
    source_ids = pa.array(id_map.source_ids(user_ids), type=pa.uint64())
    target = entity_column if entity_column in ENTITY_COLUMNS else "entity_id"
    if target in table.column_names:
        table = table.set_column(table.column_names.index(target), target, source_ids)
    else:
        table = table.append_column(target, source_ids)
    written = _sources_path(directory, name)
    pq.write_table(table, written)
    return StagedSource(
        name=name,
        path=written,
        declared_path=f"sources/{name}.parquet",
        default=default,
        user_id_column=entity_column if entity_column not in ENTITY_COLUMNS else None,
        rows=table.num_rows,
        columns=dict(zip(table.schema.names, table.schema.types)),
        notes=[f"read, mapped on '{entity_column}' and written under sources/"],
        user_ids=user_ids,
    )


def _sources_path(directory: Path, name: str) -> Path:
    path = directory / "sources" / f"{name}.parquet"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def _relative(path: Path, directory: Path) -> str:
    """`[sources]` takes a path relative to the declaration; an absolute one is refused at parse."""
    return os.path.relpath(path, directory)
