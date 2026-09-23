"""`declare_columns`: every column of a frame declared from its data type.

Each column not in `skip` and not already declared is declared as a detail: stored and shown
when an item is opened, neither rendered nor filterable unless named. `keyword` and `category`
choose those types for string columns, which are `text` otherwise, and a categorical column is a
category. Only the frame's schema is read, never its values.
"""

from __future__ import annotations

from dataclasses import dataclass

import pyarrow as pa

#: A category declared from a frame reads an open, public vocabulary at this width.
VOCABULARY_WIDTH = "u16"


@dataclass
class DeclaredColumn:
    """One row of the table the helper prints."""

    name: str
    dtype: str
    declared_as: str
    render: bool
    index: bool
    vocabulary: str | None = None
    why: str = ""


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


def columns_of(
    schema: dict[str, pa.DataType],
    skip: set[str],
    render: set[str],
    index: set[str],
    keyword: set[str],
    category: set[str],
    declared: set[str],
) -> tuple[list[dict], list[dict], list[DeclaredColumn]]:
    """The attribute blocks, the vocabulary blocks, and one row per column for the report."""
    attributes: list[dict] = []
    vocabularies: list[dict] = []
    rows: list[DeclaredColumn] = []
    for name, dtype in schema.items():
        if name in skip or name in declared:
            continue
        row = _column(name, dtype, name in keyword, name in category)
        row.render = row.declared_as != "not declared" and name in render
        row.index = row.declared_as != "not declared" and name in index
        rows.append(row)
        if row.declared_as == "not declared":
            continue
        block: dict = {"name": name, "type": row.declared_as}
        if row.vocabulary is not None:
            block["vocabulary"] = row.vocabulary
            vocabularies.append(
                {
                    "name": row.vocabulary,
                    "width": VOCABULARY_WIDTH,
                    "value_set": "open",
                    "visibility": "public",
                }
            )
        if row.render:
            block["render"] = True
        if row.index:
            block["index"] = True
        attributes.append(block)
    return attributes, vocabularies, rows


def _column(name: str, dtype: pa.DataType, keyword: bool, category: bool) -> DeclaredColumn:
    spelled = str(dtype)
    if pa.types.is_dictionary(dtype):
        value = dtype.value_type
        if (pa.types.is_string(value) or pa.types.is_large_string(value)) and not keyword:
            return DeclaredColumn(
                name, spelled, "category", False, False, vocabulary=name,
                why="a categorical column, over an open public vocabulary at width u16",
            )
        dtype, spelled = value, str(value)
    width = _WIDTHS.get(spelled)
    if width is not None:
        return DeclaredColumn(name, spelled, width, False, False, why="the matching width")
    if pa.types.is_timestamp(dtype):
        return DeclaredColumn(name, spelled, "timestamp_us", False, False, why="a datetime")
    if pa.types.is_string(dtype) or pa.types.is_large_string(dtype):
        if category:
            return DeclaredColumn(
                name, spelled, "category", False, False, vocabulary=name,
                why="category= named it, over an open public vocabulary",
            )
        if keyword:
            return DeclaredColumn(name, spelled, "keyword", False, False, why="keyword= named it")
        return DeclaredColumn(name, spelled, "text", False, False, why="a string column")
    if pa.types.is_list(dtype) or pa.types.is_large_list(dtype):
        return DeclaredColumn(
            name, spelled, "not declared", False, False,
            why="a list of strings is a view's access= column, or a declared attribute",
        )
    return DeclaredColumn(
        name, spelled, "not declared", False, False, why="no type is declared for this one"
    )
