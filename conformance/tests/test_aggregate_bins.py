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
- **Edges of a range**, by the contract's formula.
- **Counts**: each item of the set placed by comparing its value with the served edges, a bin
  holding its lower edge and not its upper, the last bin both; NaN in `rest`; no value in `none`.

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


def readable_numbers(lo, hi, n: int, integer: bool) -> list[float]:
    lo, hi = Fraction(lo), Fraction(hi)
    if lo == hi:
        width = Fraction(10) ** _power_at_or_below(abs(lo)) if lo else Fraction(1)
        first = math.floor(lo / width) * width
        return [float(first), float(first + width)]
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
                return [float(first + k * width) for k in range(bins + 1)]
    # One bin over values either side of 0, which no multiple of a width starts below.
    assert n == 1 and lo < 0 < hi, "no readable width"
    return [float(lo), float(hi + 1 if integer else hi)]


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


def range_edges(column: str, lower, upper, n: int) -> list:
    if column in TIMESTAMPS:
        return [lower + (upper - lower) * i // n for i in range(n + 1)]
    lower, upper = float(lower), float(upper)
    return [lower if i == 0 else upper if i == n else lower + (upper - lower) * (i / n) for i in range(n + 1)]


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


def read_table(server, token: str, body: dict) -> tuple[dict, list[tuple]]:
    head = None
    rows: list[tuple] = []
    body = dict(body)
    for _ in range(1000):
        resp = server.aggregate(token, **body)
        assert resp.status_code == 200, resp.text
        decoded = split_aggregate_frames(resp.content)
        for table_head, pages in decoded.tables:
            head = head or table_head
            for records, _end in pages:
                batch = ipc.open_stream(io.BytesIO(records)).read_next_batch()
                columns = {}
                for name in batch.schema.names:
                    column = batch.column(name)
                    if pa.types.is_timestamp(column.type):
                        assert column.type == pa.timestamp("us", tz="UTC"), column.type
                        column = column.cast(pa.int64())
                    columns[name] = column.to_pylist()
                assert "key" not in columns and "title" not in columns
                rows += list(zip(columns["group"], columns["lower"], columns["upper"],
                                 columns["count"], columns["reference_count"]))
        if decoded.trailer["next"] is None:
            return {k: v for k, v in head.items() if k not in ("grouping", "resumed")}, rows
        body["cursor"] = decoded.trailer["next"]
    raise AssertionError("a read that never ends")


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


RANGES = {
    "score": (-10, 10.5, 4),
    "rank": (-100, 100, 8),
    "weight": (0, 200.5, 3),
    "big": ("9223372036854775808", "9223872036854775808", 5),
    "seen": (_us(2019), _us(2020) + 1, 6),
    "when": (str(_us(2024, 5, 2)), _us(2024, 5, 3), 24),
}


@pytest.mark.parametrize("column", list(RANGES))
def test_a_range_is_the_oracles(deployment, column):
    token = deployment.server.authorise(list(fx.PRINCIPALS["two"]))["token"]
    seen = visible("two")
    lower, upper, n = RANGES[column]
    body = {"view": fx.WORLD, "reference": {}, "filters": FILTERS["rank"][0],
            "groupings": [{"by": {"field": column, "bins": n, "range": [lower, upper]}}]}
    head, rows = read_table(deployment.server, token, body)
    edges = range_edges(column, int(lower) if isinstance(lower, str) else lower,
                        int(upper) if isinstance(upper, str) else upper, n)
    want_head, want_rows = expected(column, edges, [i for i in seen if FILTERS["rank"][1](i)], seen)
    assert (head, rows) == (want_head, want_rows), column
    assert any(r[0] == "rest" for r in rows), f"{column}: the range leaves values outside it"


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
