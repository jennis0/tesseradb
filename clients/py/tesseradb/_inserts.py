"""An insert: a table handed to a declared thing, with the columns it reads named (§3).

`declare_*` says what exists and takes no data; `insert(target, table, **columns)` hands a table
to a declared thing and names every column that thing needs. The SDK guesses no column name and
rewrites no data: a frame is written to `sources/` as it was given and the declaration names the
columns; a path is read where it lies. Every insert prints two lists, the columns it read and the
columns it ignored.

**What is named and what is not.** The id, the coordinates, the access labels, a layer's key and
a group's view column are named on every call and never matched (§3). An artifacts table, a
members table, a roster and a value set are tables in Tessera's own shape, so a column of theirs
carrying its canonical name — `level`, `parent`, `attached_key` and the rest — is read under that
name where the call does not rename it, and is printed as read rather than as ignored, because
the build and the publication routes read it either way.

**Where the table goes.** Before the first commit an insert binds the table to its target for the
build: a frame is written under `sources/` and a path is recorded, and the declaration names the
file and the columns. After the first commit the same insert is sent at the next `commit()` by the
route its target owns, so its frame goes under `.tessera/inserts/` — out of `[sources]`, which
`tessera check` reads as the whole corpus — and nothing is written under `sources/`.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

from ._refusal import Refusal

#: Where a frame inserted after the first commit is written. Not `sources/`: `tessera check` reads
#: every key of `[sources]` as a file the declaration is built from.
INSERT_FOLDER = ".tessera/inserts"

#: An artifacts table's own columns, in Tessera's own spelling (configuration.md §1). A column of
#: one of these names is read under it where the call renames nothing.
ARTIFACT_COLUMNS = (
    "level", "key", "parent", "contents", "attached_layer", "attached_level", "attached_key",
    "space", "excluding", "members", "bbox", "circle", "ellipse", "wkt",
)

#: A members table's own columns, likewise.
MEMBER_COLUMNS = ("level", "key", "rank", "entity")

#: A value set's own columns.
VOCABULARY_COLUMNS = ("key", "title", "code")


@dataclass
class Contract:
    """Which columns one kind of target reads, and which of them the call must name (§3)."""

    required: tuple[str, ...]
    optional: tuple[str, ...] = ()
    #: One of these must be named, where the contract names a pair.
    either: tuple[str, ...] = ()
    #: The canonical columns of a table in Tessera's own shape, read under their own names.
    canonical: tuple[str, ...] = ()
    #: Whether the call may name metadata columns beyond the contract (a group's roster).
    free: bool = False


CONTRACTS: dict[tuple[str, str], Contract] = {
    ("view", "rows"): Contract(required=("x", "y"), optional=("id", "access")),
    ("view_group", "rows"): Contract(required=("x", "y", "view"), optional=("id", "access")),
    ("view_group", "roster"): Contract(
        required=("key",), optional=("visibility",), free=True
    ),
    ("attribute", "values"): Contract(required=("id", "value"), optional=("view",)),
    ("layer", "key"): Contract(required=("id", "key"), optional=("view",)),
    ("layer", "artifacts"): Contract(
        required=("key",),
        optional=(
            "level", "parent", "contents", "attached_layer", "attached_level", "attached_key",
            "members", "excluding", "space", "bbox", "circle", "ellipse", "wkt", "view",
            "visibility",
        ),
        canonical=ARTIFACT_COLUMNS,
    ),
    ("layer", "members"): Contract(
        required=("id", "key"), optional=("level", "rank", "view"), canonical=MEMBER_COLUMNS
    ),
    ("labels", "text"): Contract(
        required=("key",),
        either=("text", "contents"),
        optional=("level", "attached_key", "attached_level"),
        canonical=ARTIFACT_COLUMNS,
    ),
    ("labels", "members"): Contract(
        required=("id", "key"), optional=("level", "rank"), canonical=MEMBER_COLUMNS
    ),
    ("vocabulary", "values"): Contract(
        required=("key",), optional=("title", "code"), canonical=VOCABULARY_COLUMNS
    ),
}

#: Under a projection a view's coordinates are `lon` and `lat` (configuration.md §1), so the two
#: names the call gives are those rather than `x` and `y`.
PROJECTED = {"x": "lon", "y": "lat"}


@dataclass
class Insert:
    """One table bound to one declared thing, and the columns it reads."""

    target: str
    #: The block kind the target is: `view`, `view_group`, `attribute`, `layer`, `labels` or
    #: `vocabulary`.
    kind: str
    #: Which of that kind's tables this is: `rows`, `roster`, `values`, `key`, `artifacts`,
    #: `members` or `text`.
    role: str
    #: What the call named, by the contract's own name: `{"id": "paper", "x": "x", …}`.
    columns: dict[str, str] = field(default_factory=dict)
    #: An attribute's value column, named explicitly on a view's insert or matched by the
    #: attribute's own name from a frame inserted into the allocation view: `{attribute: column}`.
    named_attributes: dict[str, str] = field(default_factory=dict)
    #: A group's roster carries one column per metadata name the group declared.
    metadata_columns: dict[str, str] = field(default_factory=dict)
    #: The source key this insert writes into `[sources]`, before the first commit.
    source: str | None = None
    path: Path | None = None
    declared_path: str | None = None
    in_place: bool = False
    rows: int = 0
    schema: dict[str, Any] = field(default_factory=dict)
    read: list[str] = field(default_factory=list)
    ignored: list[str] = field(default_factory=list)
    #: Whether this insert was made before the first commit, and so is read by the build.
    at_build: bool = True
    #: A label set inserted as a mapping keeps it, so the table can be written with the
    #: attachment the label set's own declaration knows.
    mapping: dict | None = None

    @property
    def id_column(self) -> str | None:
        return self.columns.get("id")

    @property
    def id_type(self) -> Any:
        column = self.id_column
        return None if column is None else self.schema.get(column)

    def table(self) -> pa.Table:
        """The rows, read back from wherever this insert put them."""
        return pq.read_table(self.path)

    def lines(self) -> list[str]:
        """What this insert prints: the columns it read, and the columns it ignored (§3)."""
        where = "read in place" if self.in_place else "written to " + str(self.declared_path)
        named = ", ".join(f"{role}={column!r}" for role, column in self.columns.items())
        out = [
            f"insert into {self.kind} {self.target!r} ({self.role}): {self.rows} row(s), {where}",
            f"  read:    {', '.join(self.read) or 'nothing'}",
            f"  ignored: {', '.join(self.ignored) or 'nothing'}",
        ]
        if named:
            out.insert(1, f"  columns: {named}")
        return out

    def __str__(self) -> str:
        return "\n".join(self.lines())

    __repr__ = __str__


#: The integer types an id column may be read as, by the name `str(type)` gives them. The build
#: reads an id column at 32 or 64 bits and refuses the narrower widths (configuration.md §8).
INTEGER_TYPES = {f"{sign}int{width}" for sign in ("", "u") for width in (32, 64)}


def is_integer_type(dtype: Any) -> bool:
    """Whether an id column of this type is read as an integer, at the widths the build takes."""
    if dtype is None:
        return False
    return str(dtype) in INTEGER_TYPES


def is_path(data: Any) -> bool:
    return isinstance(data, (str, os.PathLike))


def as_table(data: Any) -> pa.Table:
    """A frame as an Arrow table. pandas, polars and pyarrow all arrive through `pa.table`."""
    if isinstance(data, pa.Table):
        return data
    return pa.table(data)


def build(
    target: str,
    kind: str,
    role: str,
    data: Any,
    named: dict[str, str],
    directory: Path,
    source: str,
    at_build: bool,
    projected: bool = False,
    metadata: tuple[str, ...] = (),
    named_attributes: dict[str, str] | None = None,
) -> Insert:
    """One insert: the table where it belongs, the columns checked, the two lists computed."""
    contract = CONTRACTS[(kind, role)]
    folder = "sources" if at_build else INSERT_FOLDER
    if is_path(data):
        path = Path(data).expanduser().resolve()
        if not path.exists():
            raise Refusal(f"insert into {target!r}: {path} does not exist")
        file = pq.ParquetFile(path)
        schema = dict(zip(file.schema_arrow.names, file.schema_arrow.types))
        rows = file.metadata.num_rows
        insert = Insert(
            target=target,
            kind=kind,
            role=role,
            source=source,
            path=path,
            declared_path=os.path.relpath(path, directory),
            in_place=True,
            rows=rows,
            schema=schema,
            at_build=at_build,
        )
    else:
        table = as_table(data)
        path = directory / folder / f"{source}.parquet"
        path.parent.mkdir(parents=True, exist_ok=True)
        pq.write_table(table, path)
        insert = Insert(
            target=target,
            kind=kind,
            role=role,
            source=source,
            path=path,
            declared_path=f"{folder}/{source}.parquet",
            rows=table.num_rows,
            schema=dict(zip(table.schema.names, table.schema.types)),
            at_build=at_build,
        )
    _check(insert, contract, named, projected, metadata)
    insert.named_attributes = dict(named_attributes or {})
    _lists(insert, contract)
    return insert


def _check(
    insert: Insert,
    contract: Contract,
    named: dict[str, str],
    projected: bool,
    metadata: tuple[str, ...],
) -> None:
    """Every column the target needs is named on the call, and every name is a column (§3)."""
    required = tuple(
        PROJECTED[role] if projected and role in PROJECTED else role
        for role in contract.required
    )
    optional = tuple(
        PROJECTED[role] if projected and role in PROJECTED else role
        for role in contract.optional
    )
    known = set(required) | set(optional) | set(contract.either) | set(metadata)
    unknown = sorted(set(named) - known)
    if unknown and not contract.free:
        raise Refusal(
            f"insert into {insert.kind} {insert.target!r}: "
            + ", ".join(f"{name}=" for name in unknown)
            + " names nothing this target reads. It reads "
            + ", ".join(f"{name}=" for name in sorted(known))
        )
    missing = [role for role in required if role not in named]
    if missing:
        raise Refusal(
            f"insert into {insert.kind} {insert.target!r}: "
            + ", ".join(f"{role}=" for role in missing)
            + " is not named, and this target reads it from every table it is given. Name the "
            "column it is in"
        )
    if contract.either and not any(role in named for role in contract.either):
        raise Refusal(
            f"insert into {insert.kind} {insert.target!r}: name the column the text is in, "
            + " or ".join(f"{role}=" for role in contract.either)
        )
    for role, column in named.items():
        if column not in insert.schema:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: {role}={column!r} names no column "
                f"of this table. Its columns are {', '.join(insert.schema)}"
            )
    # Each role keeps the name the call gave it: a projected view names `lon=` and `lat=`, which
    # is what its declaration writes (configuration.md §1).
    insert.columns = {
        role: column for role, column in named.items() if not (contract.free and role in metadata)
    }
    insert.metadata_columns = {
        role: column for role, column in named.items() if contract.free and role in metadata
    }


def _lists(insert: Insert, contract: Contract) -> None:
    """The columns this insert read and the columns it ignored, in the table's own order (§3)."""
    read = set(insert.columns.values())
    read |= set(insert.metadata_columns.values())
    read |= set(insert.named_attributes.values())
    # A table in Tessera's own shape carries its own column names, which the build and the
    # publication routes read whether or not the call renamed one.
    for column in contract.canonical:
        if column in insert.schema and column not in insert.columns.values():
            read.add(column)
    insert.read = [column for column in insert.schema if column in read]
    insert.ignored = [column for column in insert.schema if column not in read]
