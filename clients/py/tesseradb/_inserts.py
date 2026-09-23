"""An insert: a table handed to a declared thing, with the columns it reads named.

`declare_*` says what exists and takes no data; `insert(target, table, **columns)` hands a table
to a declared thing and names every column that thing needs. The SDK guesses no column name and
rewrites no data: a frame is written to `sources/` as it was given and the declaration names the
columns; a path is read where it lies. Every insert prints two lists, the columns it read and the
columns it ignored.

**What is named and what is not.** The id, the coordinates, the access labels, a layer's key and
a group's view column are named on every call and never matched. An artifacts table, a members
table, a roster and a value set are tables in Tessera's own shape, so a column of theirs
carrying its canonical name is named on the call like any other, and one the call passed over
is refused rather than read silently: the build and the publication routes read such a column
under its own name whatever this package prints. Two of them are read under their own names
alone, `level` and `attached_level`, neither being in the build's `fields` map: those keywords
take the canonical name and a column called anything else is renamed in the table. A membership
shape is named by its kind, `shape="polygon"`, its columns being read under theirs.

**Where the table goes.** Before the first commit an insert binds the table to its target for the
build: a frame is written under `sources/` and a path is recorded, and the declaration names the
file and the columns. After the first commit the same insert is sent at the next `commit()` by the
route its target owns, so its frame goes under `.tessera/inserts/`, out of `[sources]`, which
`tessera check` reads as the whole corpus, and nothing is written under `sources/`.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq

from ._refusal import Refusal
from ._reports import Summarised, _count

#: Where a frame inserted after the first commit is written. Not `sources/`: `tessera check` reads
#: every key of `[sources]` as a file the declaration is built from.
INSERT_FOLDER = ".tessera/inserts"

#: An artifacts table's columns, in the build's own field set: the ones a declaration's `fields`
#: map can move, so the call may name a column of any name for each.
ARTIFACT_FIELDS = (
    "key", "view", "space", "contents", "parent", "attached_layer", "attached_key", "members",
    "excluding",
)

#: The artifact columns the build reads under their own names alone: neither is in its `fields`
#: map, so the keyword takes the canonical name and a column called anything else is renamed in
#: the table rather than mapped here.
ARTIFACT_CANONICAL_ONLY = ("level", "attached_level")

#: A membership shape's own columns, by the kind `shape=` names. Read under their own names, the
#: build's `fields` map moving none of them.
SHAPE_COLUMNS = {
    "bbox": ("min_x", "min_y", "max_x", "max_y"),
    "circle": ("cx", "cy", "r"),
    "ellipse": ("cx", "cy", "a", "b", "angle"),
    "polygon": ("geometry",),
}

#: The publication record's own field for each kind, the record carrying `bbox = [min_x, min_y,
#: max_x, max_y]`, `circle = [cx, cy, r]` and `ellipse = [cx, cy, a, b, angle]`. A polygon's
#: `geometry` column is WKB at the build and the record's `wkt` is text, so the two doors read one
#: column two ways.
SHAPE_RECORD_FIELD = {
    "bbox": "bbox", "circle": "circle", "ellipse": "ellipse", "polygon": "wkt",
}
SHAPE_FIELDS = tuple(SHAPE_RECORD_FIELD.values())

#: A members table's columns, likewise: `entity` is what `id=` names, and `level` is canonical.
MEMBER_FIELDS = ("key", "entity", "rank")
MEMBER_CANONICAL_ONLY = ("level",)

#: A value set's columns, and a roster's own two beside the metadata names its group declared.
VOCABULARY_FIELDS = ("key", "title", "code")
ROSTER_FIELDS = ("key", "visibility")


def _artifact_columns() -> tuple[str, ...]:
    """Every column name an artifacts table carries meaning under."""
    shapes = tuple(dict.fromkeys(one for kind in SHAPE_COLUMNS for one in SHAPE_COLUMNS[kind]))
    return ARTIFACT_FIELDS + ARTIFACT_CANONICAL_ONLY + shapes


@dataclass
class Contract:
    """Which columns one kind of target reads, and which of them the call must name."""

    required: tuple[str, ...]
    optional: tuple[str, ...] = ()
    #: One of these must be named, where the contract names a pair.
    either: tuple[str, ...] = ()
    #: The column names this kind of table carries meaning under. A table in Tessera's own shape
    #: is no exception to the rule that every column a target reads is named on the call: one of
    #: these the call did not name is refused, naming the column and the two remedies, so nothing
    #: is read silently at either door.
    canonical: tuple[str, ...] = ()
    #: The keywords whose column the build reads under its own name alone.
    canonical_only: tuple[str, ...] = ()
    #: The keywords whose value is a key rather than a column: one view for a whole table.
    values: tuple[str, ...] = ()
    #: Whether this table carries a membership shape, which `shape=` names by its kind.
    shaped: bool = False


CONTRACTS: dict[tuple[str, str], Contract] = {
    ("view", "rows"): Contract(required=("x", "y"), optional=("id", "access")),
    ("view_group", "rows"): Contract(
        required=("x", "y"), optional=("id", "access", "view"), values=("view_key",)
    ),
    ("view_group", "roster"): Contract(
        required=("key",), optional=("visibility",), canonical=ROSTER_FIELDS
    ),
    ("attribute", "values"): Contract(required=("id", "value"), optional=("view",)),
    ("layer", "key"): Contract(required=("id", "key"), optional=("view",)),
    ("layer", "artifacts"): Contract(
        required=("key",),
        optional=(
            "level", "parent", "contents", "attached_layer", "attached_level", "attached_key",
            "members", "excluding", "space", "view",
        ),
        canonical=_artifact_columns(),
        canonical_only=ARTIFACT_CANONICAL_ONLY,
        shaped=True,
    ),
    ("layer", "members"): Contract(
        required=("id", "key"),
        optional=("level", "rank", "view"),
        canonical=MEMBER_FIELDS + MEMBER_CANONICAL_ONLY,
        canonical_only=MEMBER_CANONICAL_ONLY,
    ),
    ("labels", "text"): Contract(
        required=("key",),
        either=("text", "contents"),
        optional=(
            "level", "parent", "attached_layer", "attached_key", "attached_level", "members",
            "excluding", "space",
        ),
        canonical=_artifact_columns(),
        canonical_only=ARTIFACT_CANONICAL_ONLY,
    ),
    ("labels", "members"): Contract(
        required=("id", "key"),
        optional=("level", "rank"),
        canonical=MEMBER_FIELDS + MEMBER_CANONICAL_ONLY,
        canonical_only=MEMBER_CANONICAL_ONLY,
    ),
    ("vocabulary", "values"): Contract(
        required=("key",), optional=("title", "code"), canonical=VOCABULARY_FIELDS
    ),
}

#: Under a projection a view's coordinates are `lon` and `lat`, so those are the two names the
#: call gives rather than `x` and `y`.
PROJECTED = {"x": "lon", "y": "lat"}


@dataclass(repr=False)
class Insert(Summarised):
    """One table bound to one declared thing, and the columns it reads.

    It shows as one line: the target, the rows, the columns read and how many were ignored.
    `columns` is what the call named, `read` and `ignored` the table's columns in its own order,
    and `path` where the rows are, `in_place` saying whether that is the file the call gave.
    """

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
    #: The membership shape kind this table's rows carry, and the columns it is written in.
    shape: str | None = None
    shape_columns: list[str] = field(default_factory=list)
    #: The one view of its group every row of this table belongs to, where `view_key=` named it
    #: rather than `view=` naming a column.
    view_key: str | None = None
    #: The source key this insert writes into `[sources]`, before the first commit.
    source: str | None = None
    path: Path | None = None
    declared_path: str | None = None
    in_place: bool = False
    rows: int = 0
    schema: dict[str, Any] = field(default_factory=dict)
    read: list[str] = field(default_factory=list)
    ignored: list[str] = field(default_factory=list)

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

    def summary(self) -> list[str]:
        into = self.target if self.role in ("rows", "key", "values") else (
            f"{self.target} ({self.role})"
        )
        if self.view_key is not None:
            into += f", view {self.view_key}"
        ignored = f"; ignored {', '.join(self.ignored)}" if self.ignored else ""
        return [
            f"{into}: {_count(self.rows, 'row')} "
            f"(read {', '.join(self.read) or 'nothing'}{ignored})"
        ]


#: The integer types an id column may be read as, by the name `str(type)` gives them. The build
#: reads an id column at 32 or 64 bits and refuses the narrower widths.
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


def decoded(table: pa.Table) -> pa.Table:
    """The table with each dictionary column, or list of dictionary values, as its plain values.

    A pandas `Categorical` arrives as an Arrow dictionary column. Decoding it here means every
    target that reads a string column reads a categorical one the same way, at the build and at
    a running server.
    """
    for at, column in enumerate(table.schema):
        plain = _plain_type(column.type)
        if plain != column.type:
            table = table.set_column(
                at, pa.field(column.name, plain, column.nullable), table.column(at).cast(plain)
            )
    return table


def _plain_type(dtype: pa.DataType) -> pa.DataType:
    if pa.types.is_dictionary(dtype):
        return dtype.value_type
    if pa.types.is_list(dtype) and pa.types.is_dictionary(dtype.value_type):
        return pa.list_(dtype.value_type.value_type)
    if pa.types.is_large_list(dtype) and pa.types.is_dictionary(dtype.value_type):
        return pa.large_list(dtype.value_type.value_type)
    return dtype


def schema_of(data: Any) -> dict:
    """One frame's or file's columns and their types, without reading a row where it is a file."""
    if is_path(data):
        return _named_types(pq.ParquetFile(Path(data).expanduser()).schema_arrow)
    return _named_types(as_table(data).schema)


def _named_types(schema) -> dict:
    return dict(zip(schema.names, schema.types))


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
        schema = _named_types(file.schema_arrow)
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
        )
    else:
        table = decoded(as_table(data))
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
            schema=_named_types(table.schema),
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
    """Every column the target needs is named on the call, and every name is a column."""
    required = tuple(
        PROJECTED[role] if projected and role in PROJECTED else role
        for role in contract.required
    )
    optional = tuple(
        PROJECTED[role] if projected and role in PROJECTED else role
        for role in contract.optional
    )
    known = set(required) | set(optional) | set(contract.either) | set(metadata)
    known |= set(contract.values)
    if contract.shaped:
        known.add("shape")
    unknown = sorted(set(named) - known)
    if unknown:
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
    if "view" in contract.optional and "view_key" in contract.values:
        named_view = [one for one in ("view", "view_key") if one in named]
        if not named_view:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: a group's rows say which of its "
                f"views each belongs to. Name the column that says which with view=, or the one "
                f"view this whole table is for with view_key="
            )
        if len(named_view) == 2:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: view= names the column that says "
                f"which view each row belongs to and view_key= names one view for the whole "
                f"table. A table is one or the other, so name one of them"
            )
    insert.view_key = named.pop("view_key", None)
    shape = named.pop("shape", None) if contract.shaped else None
    shaped = _shape_columns(insert, shape)
    for role, column in named.items():
        if column not in insert.schema:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: {role}={column!r} names no column "
                f"of this table. Its columns are {', '.join(insert.schema)}"
            )
        if role in contract.canonical_only and column != role:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: {role}={column!r} is read by the "
                f"build under its own name, {role!r}, and no declaration moves it. Rename the "
                f"column in the table"
            )
    # Each role keeps the name the call gave it: a projected view names `lon=` and `lat=`, which
    # is what its declaration writes.
    insert.columns = {
        role: column for role, column in named.items() if role not in metadata
    }
    insert.metadata_columns = {
        role: column for role, column in named.items() if role in metadata
    }
    insert.shape = shape
    insert.shape_columns = list(shaped)
    _refuse_a_column_read_silently(insert, contract, shaped)


def _shape_columns(insert: Insert, shape: str | None) -> tuple[str, ...]:
    """The columns `shape=` names, each read under its own name."""
    if shape is None:
        return ()
    if shape not in SHAPE_COLUMNS:
        raise Refusal(
            f"insert into {insert.kind} {insert.target!r}: shape={shape!r} is not a shape kind. "
            f"It is one of {', '.join(sorted(SHAPE_COLUMNS))}, and its columns are read under "
            f"their own names"
        )
    missing = [column for column in SHAPE_COLUMNS[shape] if column not in insert.schema]
    if missing:
        raise Refusal(
            f"insert into {insert.kind} {insert.target!r}: shape={shape!r} is written in "
            f"{', '.join(SHAPE_COLUMNS[shape])}, and this table carries no "
            f"{', '.join(missing)}. The build reads them under those names, so the columns are "
            f"renamed in the table"
        )
    return SHAPE_COLUMNS[shape]


def _refuse_a_column_read_silently(
    insert: Insert, contract: Contract, shaped: tuple[str, ...]
) -> None:
    """A canonical column the call did not name.

    A table in Tessera's own shape is no exception to the rule that every column a target reads
    is named on the call: the build reads such a column under its own name whatever the SDK
    prints, so a column the call passed over is refused rather than read silently at one door and
    ignored at the other.
    """
    if not contract.canonical:
        return
    named = set(insert.columns.values()) | set(insert.metadata_columns.values()) | set(shaped)
    unnamed = [
        column
        for column in insert.schema
        if column in set(contract.canonical) and column not in named
    ]
    if not unnamed:
        return
    raise Refusal(
        f"insert into {insert.kind} {insert.target!r}: this table carries "
        + ", ".join(repr(column) for column in unnamed)
        + ", which the build and the publication routes read under that name whatever this call "
        "says. Name it on the call ("
        + ", ".join(_remedy(column) for column in unnamed)
        + "), or drop the column from the table"
    )


def _remedy(column: str) -> str:
    """How a column of this name is named on the call."""
    for kind, columns in SHAPE_COLUMNS.items():
        if column in columns:
            return f"shape={kind!r}"
    return f"{column}="


def _lists(insert: Insert, contract: Contract) -> None:
    """The columns this insert read and the columns it ignored, in the table's own order.

    What is read is what the call named, and nothing else: a canonical column it did not name is
    refused above rather than read here, so the two lists say the same thing at both doors.
    """
    read = set(insert.columns.values())
    read |= set(insert.metadata_columns.values())
    read |= set(insert.named_attributes.values())
    read |= set(insert.shape_columns)
    insert.read = [column for column in insert.schema if column in read]
    insert.ignored = [column for column in insert.schema if column not in read]
