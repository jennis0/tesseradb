"""**`POST /v1/aggregate` by bins**: a histogram of a number or timestamp field, driven over HTTP
against an oracle that knows each item's value from the corpus and the edge rules from the
contract.

The corpus is `declared_fixture`'s, every value a function of the item's source id, built in part
and ingested in part. Its fields cover every route a histogram reads: `score` (`f64`) drawn and
indexed, `rank` (`i32`) and `big` (`u64`, past 2^63) drawn alone, `weight` (`f32`) and `seen`
(`timestamp_us`) indexed alone, and `when` (`timestamp_us`) drawn alone. Each field's smallest and
largest values are held by items labelled `pa`, which two of the three principals cannot see.

The oracle derives, from the contract's rules alone and in exact arithmetic:

- **Default edges** from the values of the principal's visible items: on a number field the
  narrowest width of 1, 2, 2.5 or 5 times a power of ten that needs at most `bins` bins, from the
  multiple at or below the smallest value; on a timestamp field the finest calendar width that
  does, its bins starting where the contract says, read off Python's own calendar.
- **Edges of a range**, by the contract's formula: on an integer or timestamp field in exact
  integer arithmetic, so a bound and a value past 2^53 are placed exactly; on a float field the
  readable multiples where the bounds are two of them, and otherwise the weighed formula.
- **Counts**: each item of the set placed by comparing its value with the served edges, a bin
  holding its lower edge and not its upper, the last bin both; NaN in `rest`; no value in `none`.
- **Edge types**: `int64` on a signed integer field, `uint64` on an unsigned one, `float64` on a
  float field and on an integer field whose range has a fractional bound, and `timestamp[us, UTC]`
  on a timestamp field.
- **One page**: a histogram arrives as one page whatever `page_rows` says, alone or between
  groupings that page.

Each table is compared row for row, with every principal, under no filter and two filters, against
a reference of the whole visible set. The edges must be the same under every filter.

What it does not cover, so the gap is stated: a suppression between pages, which the engine's
tests cover, and timing.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import io
import math
from fractions import Fraction

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

import declared_fixture as fx
from declared_fixture import Column, Corpus, Deployment
from oracle.wire import split_aggregate_frames

BUILT = list(range(fx.N_BUILT))
INGESTED = list(range(fx.N_BUILT, fx.N_ITEMS))

#: Items only the `everyone` principal sees, which hold every field's extremes.
PA = [i for i in range(fx.N_ITEMS) if fx.access_of(i) == "pa"][:12]

EPOCH = dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)
SECOND = 1_000_000
DAY = 86_400 * SECOND


def _h(name: str, i: int) -> int:
    return int(hashlib.sha256(f"bins:{name}:{i}".encode()).hexdigest()[:15], 16)


def _us(year: int, month: int = 1, day: int = 1) -> int:
    """Microseconds from the epoch to the start of a date of the proleptic Gregorian calendar,
    whose 400 years always hold 146,097 days."""
    if year < 1:
        return _us(year + 400, month, day) - 146_097 * DAY
    return (dt.datetime(year, month, day, tzinfo=dt.timezone.utc) - EPOCH) // dt.timedelta(
        microseconds=1
    )


def _planted(low: int, high: int, lo, hi, other):
    def value(i: int):
        if i == PA[low]:
            return lo
        if i == PA[high]:
            return hi
        return other(i)

    return value


COLUMNS = [
    Column(
        "score", "f64", pa.float64(),
        _planted(0, 1, -1e6, 1e6, lambda i: None if i % 13 == 0 else (
            # NaN only where the build reads it: JSON, which the ingest takes, has none.
            math.nan if i % 97 == 5 and i < fx.N_BUILT else (_h("score", i) % 20_000) / 100 - 50
        )),
        index=True, render=True,
    ),
    Column(
        "rank", "i32", pa.int32(),
        _planted(2, 3, -5_000_000, 5_000_000,
                 lambda i: None if i % 11 == 0 else _h("rank", i) % 2001 - 1000),
        render=True,
    ),
    Column(
        "weight", "f32", pa.float32(),
        _planted(4, 5, -65_536.0, 65_536.0,
                 lambda i: None if i % 7 == 0 else (_h("weight", i) % 4096) / 16),
        index=True,
    ),
    Column(
        "seen", "timestamp_us", pa.timestamp("us"),
        _planted(6, 7, _us(1950), _us(2090), lambda i: None if i % 17 == 0 else (
            _us(2018) + (_h("seen", i) % (4 * 365 * 86_400)) * SECOND
        )),
        index=True,
    ),
    Column(
        "when", "timestamp_us", pa.timestamp("us"),
        _planted(8, 9, _us(2000), _us(2040), lambda i: None if i % 19 == 0 else (
            _us(2024, 5, 1) + (_h("when", i) % (72 * 3600)) * SECOND
        )),
        render=True,
    ),
    Column(
        "big", "u64", pa.uint64(),
        lambda i: None if i % 23 == 0 else 2**63 + _h("big", i) % 10**15,
        render=True,
    ),
]
BY_NAME = {c.name: c for c in COLUMNS}
TIMESTAMPS = {"seen", "when"}
INTEGERS = {"rank", "big"}
EDGE_TYPES = {
    "score": pa.float64(),
    "weight": pa.float64(),
    "rank": pa.int64(),
    "big": pa.uint64(),
    "seen": pa.timestamp("us", tz="UTC"),
    "when": pa.timestamp("us", tz="UTC"),
}

FILTERS = {
    "none": (None, lambda i: True),
    "rank": ({"rank": {"range": {"gte": 0}}}, lambda i: (r := BY_NAME["rank"].value(i)) is not None and r >= 0),
    "score": ({"score": {"range": {"lt": 50}}}, lambda i: (s := BY_NAME["score"].value(i)) is not None and s < 50),
}


@pytest.fixture(scope="module")
def deployment(tmp_path_factory):
    blocks = [fx.plain_view_toml(), fx.fx_column().toml(), *(c.toml() for c in COLUMNS)]
    points = {fx.WORLD: (BUILT, [fx.fx_column(), *COLUMNS], 0, fx.WORLD_EXTENT)}
    d = Deployment(tmp_path_factory.mktemp("aggregate-bins"), Corpus(blocks, points))
    try:
        d.rows("/control/ingest", fx.point_rows(INGESTED, [fx.fx_column(), *COLUMNS]), "bins")
        d.publish()
        yield d
    finally:
        Deployment.stop_all()


def visible(principal: str) -> list[int]:
    grants = fx.PRINCIPALS[principal]
    return [i for i in range(fx.N_ITEMS) if fx.access_of(i) in grants]


# ---------------------------------------------------------------------------------------------
# The oracle: edges from the contract's rules, counts by placing each value
# ---------------------------------------------------------------------------------------------

MANTISSAS = (Fraction(1), Fraction(2), Fraction(5, 2), Fraction(5))


def _power_at_or_below(x: Fraction) -> int:
    """The `e` with `10**e <= x < 10**(e + 1)`, for `x > 0`."""
    e = math.floor(math.log10(x))
    while Fraction(10) ** e > x:
        e -= 1
    while Fraction(10) ** (e + 1) <= x:
        e += 1
    return e


def readable_numbers(lo, hi, n: int, integer: bool) -> list:
    """Readable edges, exact integers on an integer field and the nearest `float64` otherwise."""
    out = int if integer else float
    lo, hi = Fraction(lo), Fraction(hi)
    if lo == hi:
        width = Fraction(10) ** _power_at_or_below(abs(lo)) if lo else Fraction(1)
        first = math.floor(lo / width) * width
        return [out(first), out(first + width)]
    start = _power_at_or_below((hi - lo) / n) - 1
    for e in range(start, start + 40):
        for m in MANTISSAS:
            width = m * Fraction(10) ** e
            if integer and (width < 1 or width.denominator != 1):
                continue
            first = math.floor(lo / width) * width
            if integer:
                bins = math.floor((hi - first) / width) + 1
            else:
                bins = max(1, math.ceil((hi - first) / width))
            if bins <= n:
                return [out(first + k * width) for k in range(bins + 1)]
    # With one bin, values either side of 0 have no readable width, since 0 is a multiple of
    # every width.
    assert n == 1 and lo < 0 <= hi, "no readable width"
    return [out(lo), out(hi + 1 if integer else hi)]


def _calendar_steps():
    """The contract's timestamp widths, finest first: `(kind, size, offset)`."""
    for e in range(6):
        for m in (1, 2, 5):
            if m * 10**e <= 500_000:
                yield "fixed", m * 10**e, 0
    for unit in (SECOND, 60 * SECOND):
        for k in (1, 2, 5, 10, 15, 30):
            yield "fixed", k * unit, 0
    for k in (1, 2, 3, 6, 12):
        yield "fixed", k * 3600 * SECOND, 0
    for k in (1, 2):
        yield "fixed", k * DAY, 0
    # 1970-01-05 was a Monday.
    yield "fixed", 7 * DAY, 4 * DAY
    for k in (1, 2, 3, 6):
        yield "months", k, 0
    for e in range(6):
        for m in (1, 2, 5):
            yield "years", m * 10**e, 0


def _civil(t: int) -> dt.datetime:
    return EPOCH + dt.timedelta(microseconds=t)


def readable_times(lo: int, hi: int, n: int) -> list[int]:
    if lo == hi:
        first = lo // DAY * DAY
        return [first, first + DAY]
    for kind, size, offset in _calendar_steps():
        if kind == "fixed":
            first, last = (lo - offset) // size, (hi - offset) // size
            edge = lambda k, size=size, offset=offset: k * size + offset  # noqa: E731
        elif kind == "months":
            month = lambda t: _civil(t).year * 12 + _civil(t).month - 1  # noqa: E731
            first, last = month(lo) // size, month(hi) // size
            edge = lambda k, size=size: _us((k * size) // 12, (k * size) % 12 + 1)  # noqa: E731
        else:
            first, last = _civil(lo).year // size, _civil(hi).year // size
            edge = lambda k, size=size: _us(k * size)  # noqa: E731
        if last - first + 1 <= n:
            return [edge(k) for k in range(first, last + 2)]
    raise AssertionError("no calendar width")


def default_edges(column: str, items: list[int], n: int) -> list:
    values = [
        v for i in items
        if (v := BY_NAME[column].value(i)) is not None and not (isinstance(v, float) and not math.isfinite(v))
    ]
    if not values:
        return []
    if column in TIMESTAMPS:
        return readable_times(min(values), max(values), n)
    return readable_numbers(min(values), max(values), n, column in INTEGERS)


def _readable(k: float, m: float, e: int) -> float:
    """A readable edge as the contract computes one in `float64`."""
    return k * m * 10.0**e if e >= 0 else k * m / 10.0 ** (-e)


def _fractional(bound) -> bool:
    return isinstance(bound, float) and not bound.is_integer()


def range_edges(column: str, lower, upper, n: int) -> list:
    """A range's edges; on an integer field with a fractional bound, cut as on a float field."""
    if column in TIMESTAMPS or (column in INTEGERS and not (_fractional(lower) or _fractional(upper))):
        lower, upper = int(lower), int(upper)
        return [lower + (upper - lower) * i // n for i in range(n + 1)]
    lower, upper = float(lower), float(upper)
    for e in range(-30, 31):
        for m in (1.0, 2.0, 2.5, 5.0):
            first = round(lower / _readable(1.0, m, e))
            if _readable(first, m, e) == lower and _readable(first + n, m, e) == upper:
                return [_readable(first + i, m, e) for i in range(n + 1)]
    return [
        lower if i == 0 else upper if i == n else lower * (1 - i / n) + upper * (i / n)
        for i in range(n + 1)
    ]


def place(value, edges: list) -> int | None:
    """The bin `value` falls in, or `None` for `rest`."""
    bins = len(edges) - 1
    for b in range(bins):
        if edges[b] <= value and (value < edges[b + 1] or (b == bins - 1 and value == edges[b + 1])):
            return b
    return None


def expected(column: str, edges: list, items: list[int], reference: list[int]):
    bins = max(len(edges) - 1, 0)

    def tally(of: list[int]) -> list[int]:
        counts = [0] * (bins + 2)
        for i in of:
            value = BY_NAME[column].value(i)
            if value is None:
                counts[bins + 1] += 1
            else:
                b = place(value, edges) if bins else None
                counts[bins if b is None else b] += 1
        return counts

    got, ref = tally(items), tally(reference)
    rows = []
    for g in range(bins + 2):
        if g < bins or got[g] or ref[g]:
            group = "listed" if g < bins else "rest" if g == bins else "none"
            lower, upper = (edges[g], edges[g + 1]) if g < bins else (None, None)
            rows.append((group, lower, upper, got[g], ref[g]))
    head = {"total": len(items), "reference_total": len(reference), "groups": sum(1 for c in got[:bins] if c)}
    return head, rows


# ---------------------------------------------------------------------------------------------
# Reading a table
# ---------------------------------------------------------------------------------------------


def edge_type(by: dict) -> pa.DataType:
    """The type a histogram's edges are served as, by the field and its range."""
    if by["field"] in INTEGERS and any(_fractional(b) for b in by.get("range", [])):
        return pa.float64()
    return EDGE_TYPES[by["field"]]


def table_rows(batches: list, want_type: pa.DataType) -> list[tuple]:
    rows = []
    for records in batches:
        batch = ipc.open_stream(io.BytesIO(records)).read_next_batch()
        columns = {}
        for name in batch.schema.names:
            column = batch.column(name)
            if name in ("lower", "upper"):
                assert column.type == want_type, (name, column.type)
            if pa.types.is_timestamp(column.type):
                column = column.cast(pa.int64())
            columns[name] = column.to_pylist()
        assert "key" not in columns and "title" not in columns
        rows += list(zip(columns["group"], columns["lower"], columns["upper"],
                         columns["count"], columns["reference_count"]))
    return rows


def read_table(server, token: str, body: dict) -> tuple[dict, list[tuple]]:
    """A request of one histogram, which is answered in one page whatever `page_rows` says."""
    resp = server.aggregate(token, **body)
    assert resp.status_code == 200, resp.text
    decoded = split_aggregate_frames(resp.content)
    assert decoded.trailer["next"] is None, "a histogram is one response"
    [(head, pages)] = decoded.tables
    assert len(pages) == 1, f"a histogram is one page, not {len(pages)}"
    rows = table_rows([records for records, _end in pages], edge_type(body["groupings"][0]["by"]))
    return {k: v for k, v in head.items() if k not in ("grouping", "resumed")}, rows


# ---------------------------------------------------------------------------------------------
# The tests
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal", list(fx.PRINCIPALS))
@pytest.mark.parametrize("column", [c.name for c in COLUMNS])
def test_default_bins_are_the_oracles_and_hold_still(deployment, principal, column):
    token = deployment.server.authorise(list(fx.PRINCIPALS[principal]))["token"]
    seen = visible(principal)
    for n in (1, 10, 37):
        edges = default_edges(column, seen, n)
        held = None
        for name, (filters, keep) in FILTERS.items():
            body = {"view": fx.WORLD, "reference": {}, "page_rows": 4,
                    "groupings": [{"by": {"field": column, "bins": n}}]}
            if filters is not None:
                body["filters"] = filters
            head, rows = read_table(deployment.server, token, body)
            what = f"{principal} / {column} / {n} bins / filter {name}"
            want_head, want_rows = expected(column, edges, [i for i in seen if keep(i)], seen)
            assert head == want_head, f"{what}: head {head}, expected {want_head}"
            assert rows == want_rows, f"{what}: served {rows}, expected {want_rows}"
            served = [(r[1], r[2]) for r in rows if r[0] == "listed"]
            assert held is None or served == held, f"{what}: the edges moved"
            held = served
    # The extremes, held by items this principal cannot see, lie outside its edges.
    if principal != "everyone" and column != "big":
        low = PA[{"score": 0, "rank": 2, "weight": 4, "seen": 6, "when": 8}[column]]
        high = PA[PA.index(low) + 1]
        value = BY_NAME[column].value
        assert value(low) < edges[0] and edges[-1] < value(high), f"{column}: {edges}"


def _a_big_value() -> int:
    """A `big` value the `two` principal sees, past 2^63."""
    return next(v for i in visible("two") if (v := BY_NAME["big"].value(i)) is not None)


RANGES = [
    ("score", -10, 10.5, 4),
    ("score", -10, 10, 4),
    ("rank", -100, 100, 8),
    # 201 values in 8 bins: widths of 25 and 26.
    ("rank", -100, 101, 8),
    # A fractional bound: the bins are cut, and served, in float64.
    ("rank", -100.5, 99, 7),
    ("weight", 0, 200.5, 3),
    # Multiples of 0.25 that the values, sixteenths, sit on.
    ("weight", 0.25, 2.25, 8),
    ("big", "9223372036854775808", "9223872036854775808", 5),
    # Bounds that no power of two divides, past 2^63, in bins whose widths differ by one.
    ("big", "9223372036854775809", "9223872036854775810", 7),
    ("seen", _us(2019), _us(2020) + 1, 6),
    ("when", str(_us(2024, 5, 2)), _us(2024, 5, 3), 24),
]


@pytest.mark.parametrize("case", RANGES, ids=lambda c: f"{c[0]}-{c[1]}-{c[2]}-{c[3]}")
def test_a_range_is_the_oracles(deployment, case):
    column, lower, upper, n = case
    token = deployment.server.authorise(list(fx.PRINCIPALS["two"]))["token"]
    seen = visible("two")
    body = {"view": fx.WORLD, "reference": {}, "filters": FILTERS["rank"][0],
            "groupings": [{"by": {"field": column, "bins": n, "range": [lower, upper]}}]}
    head, rows = read_table(deployment.server, token, body)
    edges = range_edges(column, lower, upper, n)
    want_head, want_rows = expected(column, edges, [i for i in seen if FILTERS["rank"][1](i)], seen)
    assert (head, rows) == (want_head, want_rows), case
    assert any(r[0] == "rest" for r in rows), f"{case}: the range leaves values outside it"


def test_a_value_past_2_to_the_53_is_placed_by_its_exact_bin(deployment):
    """Bins one wide around a `u64` value past 2^63: the value is in its own bin, which no `float64`
    edge could tell apart from its neighbours."""
    token = deployment.server.authorise(list(fx.PRINCIPALS["two"]))["token"]
    seen = visible("two")
    v = _a_big_value()
    body = {"view": fx.WORLD, "reference": {},
            "groupings": [{"by": {"field": "big", "bins": 3, "range": [str(v - 1), str(v + 2)]}}]}
    head, rows = read_table(deployment.server, token, body)
    assert [(r[1], r[2]) for r in rows if r[0] == "listed"] == [(v - 1, v), (v, v + 1), (v + 1, v + 2)]
    assert (head, rows) == expected("big", [v - 1, v, v + 1, v + 2], seen, seen)
    assert rows[1][3] >= 1, "the value's own bin holds it"


def test_a_histogram_is_one_page_between_groupings_that_page(deployment):
    """A grouping by cells pages two rows at a time on either side of two histograms: each
    histogram still arrives as one page, the same table it is read alone, and the response's
    `pages` counts it as one."""
    token = deployment.server.authorise(list(fx.PRINCIPALS["two"]))["token"]
    histograms = [{"by": {"field": "rank", "bins": 37}}, {"by": {"field": "seen", "bins": 10}}]
    cells = {"cells": {"depth": 6}}
    groupings = [cells, histograms[0], cells, histograms[1]]
    alone = [
        read_table(deployment.server, token, {"view": fx.WORLD, "reference": {}, "groupings": [h]})
        for h in histograms
    ]
    body = {"view": fx.WORLD, "reference": {}, "groupings": groupings, "page_rows": 2, "pages": 3}
    pages_of: dict[int, list] = {}
    heads: dict[int, dict] = {}
    for _ in range(10_000):
        resp = deployment.server.aggregate(token, **body)
        assert resp.status_code == 200, resp.text
        decoded = split_aggregate_frames(resp.content)
        assert sum(len(pages) for _head, pages in decoded.tables) <= 3
        for head, pages in decoded.tables:
            heads.setdefault(head["grouping"], head)
            pages_of.setdefault(head["grouping"], []).extend(records for records, _end in pages)
        if decoded.trailer["next"] is None:
            break
        body["cursor"] = decoded.trailer["next"]
    assert len(pages_of[0]) > 1 and len(pages_of[2]) > 1, "the cells page"
    for g, h, (head, rows) in ((1, histograms[0], alone[0]), (3, histograms[1], alone[1])):
        assert len(pages_of[g]) == 1, f"grouping {g} in {len(pages_of[g])} pages"
        served = {k: v for k, v in heads[g].items() if k not in ("grouping", "resumed")}
        assert (served, table_rows(pages_of[g], edge_type(h["by"]))) == (head, rows)


def test_a_grouping_by_bins_that_cannot_be_served_is_refused(deployment):
    token = deployment.server.authorise(list(fx.PRINCIPALS["everyone"]))["token"]
    for by in (
        {"field": "fx", "bins": 4, "top": 2},
        {"field": "score", "bins": 0},
        {"field": "score", "bins": 4, "range": [3, 3]},
        {"field": "score", "bins": 4, "range": [3, 1]},
        {"field": "seen", "bins": 4, "range": [0.5, 9]},
        {"field": "nothing", "bins": 4},
    ):
        resp = deployment.server.aggregate(token, view=fx.WORLD, groupings=[{"by": by}])
        assert resp.status_code == 422 and resp.json()["error"] == "contract", (by, resp.text)
    resp = deployment.server.aggregate(
        token, view=fx.WORLD, groupings=[{"by": {"field": "score", "bins": 2}, "cells": {"depth": 3}}]
    )
    assert resp.status_code == 422, resp.text
