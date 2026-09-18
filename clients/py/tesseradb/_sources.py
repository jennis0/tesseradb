"""Staging: binding a name in `[sources]` to a frame or a file (python-sdk.md §3).

A frame is written to `sources/<name>.parquet` as it was staged; a path is recorded and read where
it lies. The SDK rewrites no column and keeps no map between a row and anything else: a row is
named by the id column the declaration points at, or by the `tessera_id` the server hands back.

Frames arrive through pyarrow. `pa.table(obj)` takes a pandas frame, a pyarrow table and anything
implementing the Arrow C stream protocol, which polars does, so pyarrow is the only dependency the
`local` extra needs and pandas is optional. The one thing this module asks a frame about directly
is its index, which is pandas-specific and is detected by duck typing rather than by importing
pandas: a named index is an id column under its name, and an unnamed one names nothing.

**Which column is a source's identity.** `id=` names it. Without `id=`, a column named `id` is it,
and after that the names configuration.md makes canonical: `entity_id` where an entity id is read,
and `entity` on a member row. A file already written for Tessera spells identity that way, and the
declaration reads it under those names by default. A source with none of them has no
identity column: its rows are named by their `tessera_id` (§3), which the build writes no external
id for.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

from ._refusal import Refusal

#: The column names an identity is read from when `id=` names none, in the order they are looked
#: for: the SDK's own spelling, then configuration.md's two canonical ones (§1, §8).
ID_COLUMNS = ("id", "entity_id", "entity")


@dataclass
class StagedSource:
    """One entry of `[sources]`, and what the SDK knows about the data behind it."""

    name: str
    path: Path
    #: The path as `[sources]` spells it: relative to the declaration, never absolute (§8).
    declared_path: str
    default: bool = False
    #: Read where it lies rather than copied under `sources/`.
    in_place: bool = False
    #: The column this source names its rows by, and `None` where it names none (§3).
    id_column: str | None = None
    rows: int | None = None
    columns: dict[str, pa.DataType] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)

    @property
    def id_type(self) -> Any:
        """The Arrow type of the id column, or `None` where this source names no rows."""
        return None if self.id_column is None else self.columns.get(self.id_column)


#: The integer types an id column may be read as, by the name `str(type)` gives them: a reopened
#: database holds the names rather than the types. The build reads an id column at 32 or 64 bits
#: and refuses the narrower widths, so they are not integers here either (configuration.md §8).
INTEGER_TYPES = {f"{sign}int{width}" for sign in ("", "u") for width in (32, 64)}


def is_integer_type(dtype: Any) -> bool:
    """Whether an id column of this type is read as an integer, at the widths the build takes."""
    if dtype is None:
        return False
    return str(dtype) in INTEGER_TYPES


def is_pandas_frame(data: Any) -> bool:
    return hasattr(data, "index") and hasattr(data, "columns") and hasattr(data, "dtypes")


def to_table(data: Any, id: str | None) -> tuple[pa.Table, str | None]:
    """A frame as an Arrow table, with the column that names its rows.

    A pandas index with a name is an id column under that name, and is written into the table so
    the build has a column to read. An unnamed index is the frame's row positions, which name
    nothing outside that frame, so it is not an id (§3).
    """
    if is_pandas_frame(data):
        table = pa.table(data)
        index = data.index
        if id is None and index.name is not None:
            name = str(index.name)
            if name not in table.column_names:
                table = table.append_column(name, pa.array(list(index)))
            return table, name
    else:
        table = data if isinstance(data, pa.Table) else pa.table(data)
    if id is not None:
        if id not in table.column_names:
            raise Refusal(f"id={id!r} names no column of this frame: {table.column_names}")
        return table, id
    return table, _found(table.column_names)


def _found(names) -> str | None:
    for column in ID_COLUMNS:
        if column in names:
            return column
    return None


def stage_frame(
    name: str,
    data: Any,
    directory: Path,
    id: str | None = None,
    default: bool = False,
    folder: str = "sources",
) -> StagedSource:
    """Write a frame to `<folder>/<name>.parquet` as it was staged (§3)."""
    table, id_column = to_table(data, id)
    path = _sources_path(directory, name, folder)
    pq.write_table(table, path)
    notes = []
    if id_column is None:
        notes.append("names no id column: its rows are named by their tessera_id")
    else:
        notes.append(f"'{id_column}' names its rows")
    return StagedSource(
        name=name,
        path=path,
        declared_path=f"{folder}/{name}.parquet",
        default=default,
        id_column=id_column,
        rows=table.num_rows,
        columns=dict(zip(table.schema.names, table.schema.types)),
        notes=notes,
    )


def stage_path(
    name: str,
    data: str | os.PathLike,
    directory: Path,
    id: str | None = None,
    default: bool = False,
    folder: str = "sources",
) -> StagedSource:
    """A file, read where it lies: a large corpus is not copied to be declared (§3)."""
    path = Path(data).expanduser().resolve()
    if not path.exists():
        raise Refusal(f"source {name!r}: {path} does not exist")
    schema = pq.ParquetFile(path).schema_arrow
    columns = dict(zip(schema.names, schema.types))
    if id is not None and id not in columns:
        raise Refusal(f"source {name!r}: id={id!r} names no column of {path.name}")
    id_column = id or _found(schema.names)
    note = (
        "read in place; it names no id column, so its rows are named by their tessera_id"
        if id_column is None
        else f"read in place; '{id_column}' names its rows"
    )
    return StagedSource(
        name=name,
        path=path,
        declared_path=_relative(path, directory),
        default=default,
        in_place=True,
        id_column=id_column,
        rows=pq.ParquetFile(path).metadata.num_rows,
        columns=columns,
        notes=[note],
    )


def _sources_path(directory: Path, name: str, folder: str = "sources") -> Path:
    path = directory / folder / f"{name}.parquet"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def _relative(path: Path, directory: Path) -> str:
    """`[sources]` takes a path relative to the declaration; an absolute one is refused at parse."""
    return os.path.relpath(path, directory)
