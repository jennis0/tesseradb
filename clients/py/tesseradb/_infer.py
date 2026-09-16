"""What an undeclared column of the default source becomes (python-sdk.md §4.5).

`declare_view` claims id, x, y and access on its source, and a layer's `from_column` claims its
column; every other column of the **default** source becomes an attribute by the table below, and
the SDK prints what it did once. An explicit `declare_attribute` on the same name overrides the
inferred block, and on a source that is not the default nothing is inferred.

Deciding a string column costs a read of that column: the distinct count and the median length
are facts about the values, and a Parquet footer carries neither. Only columns no block declares
are read, and only from the default source, so a declaration that names its columns reads none of
them; a declaration that names none reads each once per call.

The two thresholds are assumed rather than measured: a string column of at most 4,096 distinct
values is a category, and one whose median length is under 64 characters is a keyword. They decide
a default, and the user overrides one column with one call, so the cost of either being wrong is a
line in the printed table and a second call.
"""

from __future__ import annotations

import statistics
from dataclasses import dataclass

import pyarrow as pa

#: At most this many distinct values makes a string column a category (assumed).
FEW_DISTINCT = 4096
#: A median length under this many characters makes a string column a keyword rather than text
#: (assumed).
SHORT_MEDIAN = 64


@dataclass
class InferredColumn:
    name: str
    dtype: str
    declared_as: str
    render: bool
    index: bool
    vocabulary: str | None = None
    why: str = ""


def infer(table: pa.Table, claimed: set[str]) -> tuple[list[dict], list[dict], list[InferredColumn]]:
    """The attribute blocks, the vocabulary blocks and the row per column for the report."""
    attributes: list[dict] = []
    vocabularies: list[dict] = []
    rows: list[InferredColumn] = []
    for name, dtype in zip(table.schema.names, table.schema.types):
        if name in claimed:
            continue
        row = _column(table, name, dtype)
        rows.append(row)
        if row.declared_as == "not inferred":
            continue
        block = {"name": name, "type": row.declared_as}
        if row.vocabulary is not None:
            block["vocabulary"] = row.vocabulary
            vocabularies.append(_vocabulary(table, name))
        if row.render:
            block["render"] = True
        if row.index:
            block["index"] = True
        attributes.append(block)
    return attributes, vocabularies, rows


def _column(table: pa.Table, name: str, dtype: pa.DataType) -> InferredColumn:
    spelled = str(dtype)
    declared = _scalar_type(dtype)
    if declared is not None:
        return InferredColumn(name, spelled, declared, render=True, index=True, why="the matching width")
    if pa.types.is_timestamp(dtype):
        return InferredColumn(
            name, spelled, "timestamp_us", render=True, index=True, why="the unit is served"
        )
    if pa.types.is_string(dtype) or pa.types.is_large_string(dtype):
        values = table[name].to_pylist()
        present = [v for v in values if v is not None]
        distinct = len(set(present))
        if distinct <= FEW_DISTINCT:
            return InferredColumn(
                name,
                spelled,
                "category",
                render=True,
                index=True,
                vocabulary=name,
                why=f"{distinct} distinct value(s), at most {FEW_DISTINCT}",
            )
        median = statistics.median([len(v) for v in present]) if present else 0
        if median < SHORT_MEDIAN:
            return InferredColumn(
                name,
                spelled,
                "keyword",
                render=False,
                index=True,
                why=f"median length {median:.0f}, under {SHORT_MEDIAN}",
            )
        return InferredColumn(
            name,
            spelled,
            "text",
            render=False,
            index=True,
            why=f"median length {median:.0f}, at least {SHORT_MEDIAN}; render is refused on text",
        )
    why = (
        "a list of strings is an access column or a declared attribute"
        if pa.types.is_list(dtype)
        else "no inference for this type"
    )
    return InferredColumn(name, spelled, "not inferred", render=False, index=False, why=why)


_WIDTHS = {
    "bool": "bool",
    "int8": "i8",
    "int16": "i16",
    "int32": "i32",
    "int64": "i64",
    "uint8": "u8",
    "uint16": "u16",
    "uint32": "u32",
    "uint64": "u64",
    "float": "f32",
    "double": "f64",
}


def _scalar_type(dtype: pa.DataType) -> str | None:
    return _WIDTHS.get(str(dtype))


def _vocabulary(table: pa.Table, name: str) -> dict:
    """An inferred vocabulary is open and public, at one width above what its values need (§4.4).

    Open, because the values are minted from the data and the column is the roster; public,
    because a local user is the authority the build's warning asks for. Issue #83 is open on
    serving `derived`, under which `derived` becomes this default.
    """
    present = [v for v in table[name].to_pylist() if v is not None]
    return {
        "name": name,
        "width": width_above(len(set(present))),
        "value_set": "open",
        "visibility": "public",
    }


def width_above(distinct: int) -> str:
    """One width above what `distinct` values need, `u16` at least, and `u32` at the ceiling.

    An open vocabulary at the width its first values fill has no code for the next one, and the
    width is fixed at the first commit.
    """
    return "u16" if distinct < 2**8 - 1 else "u32"
