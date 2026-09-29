"""The identity rule: which item each row of a write names, written as a definition.

A row names items by its `tessera_id` and by every non-null value it carries of a field declared
unique. Each identifier names the item that holds it:

- a `tessera_id` names the live or suppressed item it was issued to, and a row whose `tessera_id`
  names no such item is refused as `unknown_tessera_id`;
- a unique value names the live or suppressed item holding it, and names nothing otherwise.

A row whose identifiers name two different items is refused as `names_two_items`. One naming a
single item addresses it. One naming none creates an item in an ingest batch when it carries a
position, and is otherwise refused as `names_no_item`: in an ingest row without coordinates, and
in every row of a request that only addresses items, a change or a member table.

An ingest batch decides its rows in row order, each against the rows kept before it. A row naming
an item an earlier kept row names is `one_item_twice`; a row carrying a unique value an earlier kept
row carries is `one_value_twice`. A refused row, whatever its reason, claims neither its item nor its
values, so it refuses no later row. Changes and member tables do not apply these two reasons, since
many rows may name one item.

Every row is resolved against what is held before the request. A deleted item names nothing, so its
values are free for a new item. A null is no value: it names nothing, and in an ingest row naming
an item it clears the value that item holds.

`strict` refuses a whole request at its first refused row. An ingest is then `409`. A change or
member request is `404` where that row names no item or an unknown `tessera_id`, and `409` where it
names two.

Where two reasons apply to one row, the rule's stated order decides which is given: a `tessera_id`
naming no item, then values naming two items, then naming no item in a row that cannot create, then
an item an earlier kept row names, then a value an earlier kept row sets.

A column of a member table, and a key of a change's `match`, that is neither `tessera_id` nor a
unique field is ignored and listed in the answer's `ignored_columns`. A `match` left with no key
names nothing. A request with rows none of which carries `tessera_id` or a unique column is
malformed, `422`; a member table with no rows is an empty membership. An ingest batch whose rows can
only edit, because it has no view or no row carries a position, is malformed in the same way when
no row carries `tessera_id` or a unique column.

This module is written from the rule's statement and the HTTP contract, without reference to the
server's resolver, so that the two agreeing on a request is evidence about both.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Mapping, Sequence

NAMES_TWO_ITEMS = "names_two_items"
UNKNOWN_TESSERA_ID = "unknown_tessera_id"
NAMES_NO_ITEM = "names_no_item"
ONE_ITEM_TWICE = "one_item_twice"
ONE_VALUE_TWICE = "one_value_twice"

REASONS = (NAMES_TWO_ITEMS, UNKNOWN_TESSERA_ID, NAMES_NO_ITEM, ONE_ITEM_TWICE, ONE_VALUE_TWICE)

#: The status a strict ingest is refused with, whatever the reason.
INGEST_STRICT_STATUS = 409

#: The status a strict change or member request is refused with, by the reason of its first
#: refused row.
ADDRESSING_STRICT_STATUS = {
    NAMES_NO_ITEM: 404,
    UNKNOWN_TESSERA_ID: 404,
    NAMES_TWO_ITEMS: 409,
}

KEYWORD = "keyword"
INTEGER = "integer"

TESSERA_ID = "tessera_id"

#: The columns an ingest row carries its position in: `x` and `y`, or `lon` and `lat`.
POSITION = ("x", "y", "lon", "lat")


@dataclass(frozen=True)
class Creates:
    """An ingest row that names no item and creates one."""


@dataclass(frozen=True)
class Names:
    """A row that names exactly one held item, by its `tessera_id`."""

    item: int


@dataclass(frozen=True)
class Refused:
    """A row the rule refuses, and why."""

    reason: str


Verdict = Creates | Names | Refused


@dataclass
class Holdings:
    """What a deployment holds, as the rule reads it.

    `unique` maps each field declared unique to `KEYWORD` or `INTEGER`. `items` maps each live or
    suppressed item's `tessera_id` to the values it holds, unique or not, keyed by column; a column
    the item holds no value in is absent. A deleted item is not in `items`.
    """

    unique: dict[str, str]
    items: dict[int, dict[str, object]] = field(default_factory=dict)
    suppressed: set[int] = field(default_factory=set)

    def key(self, column: str, value: object) -> object | None:
        """The value a unique column's cell names by, or `None` for a null. An integer is the same
        value whether sent as a number or as a string of decimal digits."""
        if value is None:
            return None
        if self.unique[column] == INTEGER:
            return int(value)
        return value

    def holders(self) -> dict[tuple[str, object], set[int]]:
        """Every held unique value, with the items holding it."""
        out: dict[tuple[str, object], set[int]] = {}
        for item, values in self.items.items():
            for column in self.unique:
                key = self.key(column, values.get(column))
                if key is not None:
                    out.setdefault((column, key), set()).add(item)
        return out

    def visible(self) -> set[int]:
        """The items a viewer holding every label is served: held and not suppressed."""
        return set(self.items) - self.suppressed


def carried(holdings: Holdings, row: Mapping[str, object]) -> set[tuple[str, object]]:
    """The non-null unique values a row carries, as `(column, key)`."""
    out = set()
    for column in holdings.unique:
        if column in row:
            key = holdings.key(column, row[column])
            if key is not None:
                out.add((column, key))
    return out


def named_by(
    holdings: Holdings,
    held: Mapping[tuple[str, object], set[int]],
    row: Mapping[str, object],
) -> set[int] | None:
    """The items a row's identifiers name, or `None` where its `tessera_id` names no held item."""
    items: set[int] = set()
    tessera_id = row.get(TESSERA_ID)
    if tessera_id is not None:
        if int(tessera_id) not in holdings.items:
            return None
        items.add(int(tessera_id))
    for value in carried(holdings, row):
        items |= held.get(value, set())
    return items


def positioned(row: Mapping[str, object]) -> bool:
    """Whether an ingest row carries a position, and so may create an item."""
    return any(row.get(column) is not None for column in POSITION)


def identifying(holdings: Holdings, rows: Sequence[Mapping[str, object]]) -> bool:
    """Whether any row carries `tessera_id` or a unique column, null or not."""
    return any(c == TESSERA_ID or c in holdings.unique for row in rows for c in row)


def ignored_columns(holdings: Holdings, rows: Sequence[Mapping[str, object]]) -> set[str]:
    """The columns of member rows or `match`es that name nothing and are ignored."""
    return {c for row in rows for c in row if c != TESSERA_ID and c not in holdings.unique}


def malformed_addressing(holdings: Holdings, rows: Sequence[Mapping[str, object]]) -> bool:
    """Whether a change or member request is `422` for carrying rows and no identifying column.
    `rows` are every `match`, or every row of every member table, of the request."""
    return bool(rows) and not identifying(holdings, rows)


def malformed_ingest(
    holdings: Holdings, rows: Sequence[Mapping[str, object]], *, view: bool = True
) -> bool:
    """Whether an ingest batch is `422` for being able only to edit, with no identifying column."""
    edits_only = not view or not any(positioned(row) for row in rows)
    return edits_only and not identifying(holdings, rows)


def resolve_ingest(holdings: Holdings, rows: Sequence[Mapping[str, object]]) -> list[Verdict]:
    """Each row of one ingest batch: created, naming one item, or refused."""
    held = holdings.holders()
    kept_items: set[int] = set()
    kept_values: set[tuple[str, object]] = set()
    verdicts: list[Verdict] = []
    for row in rows:
        items = named_by(holdings, held, row)
        values = carried(holdings, row)
        if items is None:
            verdicts.append(Refused(UNKNOWN_TESSERA_ID))
            continue
        if len(items) > 1:
            verdicts.append(Refused(NAMES_TWO_ITEMS))
            continue
        if not items and not positioned(row):
            verdicts.append(Refused(NAMES_NO_ITEM))
            continue
        if items and next(iter(items)) in kept_items:
            verdicts.append(Refused(ONE_ITEM_TWICE))
            continue
        if values & kept_values:
            verdicts.append(Refused(ONE_VALUE_TWICE))
            continue
        kept_values |= values
        if items:
            (item,) = items
            kept_items.add(item)
            verdicts.append(Names(item))
        else:
            verdicts.append(Creates())
    return verdicts


def resolve_addresses(holdings: Holdings, rows: Sequence[Mapping[str, object]]) -> list[Verdict]:
    """Each row of a request that only addresses items (changes, a member table): naming one item,
    or refused. Many rows may name one item."""
    held = holdings.holders()
    verdicts: list[Verdict] = []
    for row in rows:
        items = named_by(holdings, held, row)
        if items is None:
            verdicts.append(Refused(UNKNOWN_TESSERA_ID))
        elif len(items) > 1:
            verdicts.append(Refused(NAMES_TWO_ITEMS))
        elif not items:
            verdicts.append(Refused(NAMES_NO_ITEM))
        else:
            verdicts.append(Names(next(iter(items))))
    return verdicts


def refused(verdicts: Sequence[Verdict]) -> list[dict]:
    """The refused rows as a receipt lists them: `{"row": i, "reason": r}` in row order."""
    return [
        {"row": i, "reason": v.reason} for i, v in enumerate(verdicts) if isinstance(v, Refused)
    ]


def first_refusal(verdicts: Sequence[Verdict]) -> tuple[int, str] | None:
    """The row a strict request is refused at, with its reason, or `None` where none is refused."""
    for i, verdict in enumerate(verdicts):
        if isinstance(verdict, Refused):
            return i, verdict.reason
    return None


def table_rows(table: Mapping[str, Sequence[object]]) -> list[dict[str, object]]:
    """A member table, an object of equal-length columns, as one row per member."""
    lengths = {len(column) for column in table.values()}
    if len(lengths) > 1:
        raise ValueError(f"a member table's columns differ in length: {sorted(lengths)}")
    n = lengths.pop() if lengths else 0
    return [{name: column[i] for name, column in table.items()} for i in range(n)]


def apply_ingest(
    holdings: Holdings,
    rows: Sequence[Mapping[str, object]],
    verdicts: Sequence[Verdict],
    created: Mapping[int, int],
) -> dict[str, int]:
    """Apply an accepted batch to `holdings`, and count what it did.

    `created` maps each creating row's position to the `tessera_id` the server issued it, which
    this model cannot know. A row naming an item edits it when a column it carries holds a different
    value there, a null clearing a held value, and is unchanged otherwise. Refused rows write
    nothing.
    """
    counts = {"created": 0, "edited": 0, "unchanged": 0}
    for i, (row, verdict) in enumerate(zip(rows, verdicts, strict=True)):
        cells = {
            column: holdings.key(column, value) if column in holdings.unique else value
            for column, value in row.items()
            if column != TESSERA_ID
        }
        if isinstance(verdict, Creates):
            holdings.items[created[i]] = {c: v for c, v in cells.items() if v is not None}
            counts["created"] += 1
        elif isinstance(verdict, Names):
            values = holdings.items[verdict.item]
            changed = False
            for column, value in cells.items():
                if value is None:
                    changed |= values.pop(column, None) is not None
                elif values.get(column) != value:
                    values[column] = value
                    changed = True
            counts["edited" if changed else "unchanged"] += 1
    return counts


def apply_changes(
    holdings: Holdings, changes: Sequence[Mapping[str, object]], verdicts: Sequence[Verdict]
) -> int:
    """Apply the accepted changes, in order, and answer how many were accepted. Each change is
    `{"op": ..., "match": ...}`; `delete` removes the item, `suppress` hides it and `unsuppress`
    lifts that."""
    accepted = 0
    for change, verdict in zip(changes, verdicts, strict=True):
        if not isinstance(verdict, Names):
            continue
        accepted += 1
        item = verdict.item
        if item not in holdings.items:
            continue
        if change["op"] == "delete":
            del holdings.items[item]
            holdings.suppressed.discard(item)
        elif change["op"] == "suppress":
            holdings.suppressed.add(item)
        elif change["op"] == "unsuppress":
            holdings.suppressed.discard(item)
        else:
            raise ValueError(f"unknown op {change['op']!r}")
    return accepted
