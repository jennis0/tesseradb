"""Staging: binding a name in `[sources]` to a frame or a file (python-sdk.md §3).

A frame is written to `sources/<name>.parquet` with the `entity_id` column the build reads; a path
is recorded and read in place, so a large file is not copied. Which of the two happens to a path
is the in-place rule below.

Frames arrive through pyarrow. `pa.table(obj)` takes a pandas frame, a pyarrow table and anything
implementing the Arrow C stream protocol, which polars does, so pyarrow is the only dependency the
`local` extra needs and pandas is optional. The one thing this module asks a frame about directly
is its index, which is pandas-specific and is detected by duck typing rather than by importing
pandas.

**A source names entities when something reads it as naming them.** A frame carrying an
entity-naming column, or staged with `id=`, or staged as the default source, names entities at the
moment it is staged. A frame that carries none of those may still be a view's points or a layer's
members, which the declaration says and staging does not, so its ids are minted when the document
is written and its file is rewritten then ([`_database.Database._resolve_sources`]). A vocabulary's
values and a layer's artifacts name no entity either way and are left as they were staged.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

from ._idmap import IdMap
from ._refusal import Refusal

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
    #: Nothing named an entity when this frame was staged, and it came from a pandas frame whose
    #: index could name one where the declaration turns out to read it as points or members.
    index_available: bool = False
    #: No entity column has been written into this source yet.
    pending_ids: bool = False
    rows: int | None = None
    columns: dict[str, pa.DataType] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)
    #: The user ids this staging assigned or re-used, in staging order.
    user_ids: list[Any] = field(default_factory=list)

    @property
    def rewritable(self) -> bool:
        """A file under `sources/` the SDK wrote and may write again."""
        return not self.in_place


#: The Arrow types an entity column may carry, by the name `str(type)` gives them: a reopened
#: database holds the names rather than the types.
INTEGER_TYPES = {f"{sign}int{width}" for sign in ("", "u") for width in (8, 16, 32, 64)}


def is_integer_type(dtype: Any) -> bool:
    if isinstance(dtype, str):
        return dtype in INTEGER_TYPES
    return pa.types.is_integer(dtype)


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
                return pa.table(data), str(index.name), False, list(index)
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
    folder: str = "sources",
) -> StagedSource:
    table, id_column, index_is_the_id, index_values = to_table(data, id)
    notes: list[str] = []

    entity_column = next((c for c in ENTITY_COLUMNS if c in table.column_names), None)
    if id_column is None and entity_column is None and not default:
        # Nothing here names an entity. A view or a members block may still read it, and the
        # declaration is what says so, so the ids wait for the document.
        pq.write_table(table, _sources_path(directory, name, folder))
        return StagedSource(
            name=name,
            path=_sources_path(directory, name, folder),
            declared_path=f"{folder}/{name}.parquet",
            default=default,
            index_available=index_is_the_id,
            pending_ids=True,
            rows=table.num_rows,
            columns=dict(zip(table.schema.names, table.schema.types)),
            notes=["names no entities as staged"],
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
        if not id_map.identity:
            notes.append(f"'{entity_column}' mapped through the id map")
    else:
        # An unnamed default index: the row position, which is a name only while the frame is the
        # whole corpus. A delta's positions name nothing, so the caller is sent to `id=`.
        if not first_commit:
            raise Refusal(
                f"source {name!r}: a default index names nothing on a delta. A filtered or reset "
                f"frame's positions are not its rows' identities. Name the id column with id="
            )
        if id_map.identity:
            raise Refusal(
                f"source {name!r}: the points source {id_map.identity_source!r} is read where it "
                f"lies, so a row position is not an id in this database. Name the id column with "
                f"id="
            )
        user_ids = list(range(table.num_rows))
        target = "entity_id"
        notes.append("the frame's default index: ids are row positions")

    table = _with_entity_ids(table, target, id_map.source_ids(user_ids, name))
    path = _sources_path(directory, name, folder)
    pq.write_table(table, path)
    return StagedSource(
        name=name,
        path=path,
        declared_path=f"{folder}/{name}.parquet",
        default=default,
        user_id_column=id_column if entity_column is None else None,
        default_index=index_is_the_id and id_column is None,
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
    folder: str = "sources",
) -> StagedSource:
    """A file. Read where it lies where its ids already are what the build reads, rewritten where
    they are not.

    A points file whose `entity_id` column is an integer is read where it lies and the map becomes
    the identity over its ids, so no keyword attribute is written: the user's id is the source id.
    An entity-naming column of any other file with integer values is read in place on the same
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
        return _in_place(name, path, directory, default, columns, "names no entities")
    if entity_column not in columns:
        raise Refusal(f"source {name!r}: id={entity_column!r} names no column of {path.name}")

    if id is None and pa.types.is_integer(columns[entity_column]):
        if default:
            id_map.use_identity(name)
        return _in_place(
            name,
            path,
            directory,
            default,
            columns,
            f"'{entity_column}' is an integer, so the map is the identity over its ids",
        )

    table = pq.read_table(path)
    user_ids = table[entity_column].to_pylist()
    target = entity_column if entity_column in ENTITY_COLUMNS else "entity_id"
    table = _with_entity_ids(table, target, id_map.source_ids(user_ids, name))
    written = _sources_path(directory, name, folder)
    pq.write_table(table, written)
    return StagedSource(
        name=name,
        path=written,
        declared_path=f"{folder}/{name}.parquet",
        default=default,
        user_id_column=entity_column if entity_column not in ENTITY_COLUMNS else None,
        rows=table.num_rows,
        columns=dict(zip(table.schema.names, table.schema.types)),
        notes=[f"read, mapped on '{entity_column}' and written under sources/"],
        user_ids=user_ids,
    )


def mint_entity_ids(staged: StagedSource, id_map: IdMap, read_as: str) -> None:
    """Give a staged frame the `entity_id` column a block that reads it needs.

    Called when the document says this source is a view's points or a layer's members and nothing
    named an entity in it. The ids are the frame's row positions, which name its rows only while
    the frame is the whole corpus, so a frame with no index to fall back on is refused naming
    `id=`.
    """
    if not staged.index_available:
        raise Refusal(
            f"source {staged.name!r} is read as {read_as} and nothing in it names an entity. "
            f"Name the id column with id=, or carry an 'entity' column"
        )
    table = pq.read_table(staged.path)
    ids = id_map.source_ids(range(table.num_rows), staged.name)
    pq.write_table(_with_entity_ids(table, "entity_id", ids), staged.path)
    staged.pending_ids = False
    staged.default_index = True
    staged.columns["entity_id"] = pa.uint64()
    staged.user_ids = list(range(table.num_rows))
    staged.notes.append(f"read as {read_as}: ids are the frame's row positions")


def copy_column(source: StagedSource, target: StagedSource, column: str) -> None:
    """Copy one column from a source into another by entity id, for §4.2's second view.

    The build refuses an entity whose labels disagree between views, so a second view over the
    same entities carries the first view's labels. Where the second is a frame the SDK wrote, the
    column is joined in by id; where it is a file read in place, there is nothing to write into
    and the refusal names the column.
    """
    if not target.rewritable:
        raise Refusal(
            f"source {target.name!r} is read where it lies and carries no '{column}' column. A "
            f"second view over the same entities carries the same labels, and the build refuses an "
            f"entity whose labels disagree between views. Add '{column}' to the file, or stage it "
            f"as a frame"
        )
    held = pq.read_table(source.path, columns=["entity_id", column])
    labels = dict(zip(held["entity_id"].to_pylist(), held[column].to_pylist()))
    table = pq.read_table(target.path)
    values = [labels.get(entity) for entity in table["entity_id"].to_pylist()]
    table = table.append_column(column, pa.array(values, type=held.schema.field(column).type))
    pq.write_table(table, target.path)
    target.columns[column] = held.schema.field(column).type
    target.notes.append(f"'{column}' copied by id from '{source.name}'")


def _in_place(
    name: str,
    path: Path,
    directory: Path,
    default: bool,
    columns: dict[str, pa.DataType],
    why: str,
) -> StagedSource:
    return StagedSource(
        name=name,
        path=path,
        declared_path=_relative(path, directory),
        default=default,
        in_place=True,
        rows=pq.ParquetFile(path).metadata.num_rows,
        columns=columns,
        notes=[f"read in place: {why}"],
    )


def _with_entity_ids(table: pa.Table, column: str, ids: list[int]) -> pa.Table:
    values = pa.array(ids, type=pa.uint64())
    if column in table.column_names:
        return table.set_column(table.column_names.index(column), column, values)
    return table.append_column(column, values)


def _sources_path(directory: Path, name: str, folder: str = "sources") -> Path:
    path = directory / folder / f"{name}.parquet"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def _relative(path: Path, directory: Path) -> str:
    """`[sources]` takes a path relative to the declaration; an absolute one is refused at parse."""
    return os.path.relpath(path, directory)
