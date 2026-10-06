"""`POST /v1/aggregate`'s tables, written as a definition: literal counts over sets of entities.

A table is computed from four inputs the caller derives from the fixture, never from the bundle's
stored columns or postings: the set, the reference set where one is asked for (both already
evaluated inside the principal's visible set, as `oracle.filters.evaluate` gives them), the groups
(each a key and the set of entities in it), and each entity's 64-bit position in the view.

The rules, as the contract states them:

- **Listed groups.** Under `top`, the `n` groups with the most items of the set, ties by key
  ascending, among those with at least one. Under a named list, the named groups in the order
  named, each once, that the principal may be listed; a named group with no item still has a row.
- **`rest` and `none`.** `rest` is the items in some group and in no listed one; `none` the items
  in no group. Each appears only where it holds an item of the set or of the reference.
- **Cells.** A cell at depth `d` is the top `2d` bits of the position. With cells, each group's
  rows are its non-empty cells, ascending, and a row appears where either count is non-zero.
  With an area, a rectangle of depth-`d` cell coordinates, only its cells have rows, each
  counting all of its items; the head and the ranking of groups stay the whole set's.
- **Lift.** `(count / total) / (reference_count / reference_total)`, `None` where
  `reference_count` or `total` is zero.

Every figure is a `len()` of a set intersection; nothing is cached and no bitmap is used.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Row:
    group: str | None
    key: object
    cell: int | None
    count: int
    reference_count: int | None
    lift: float | None


def cell_of(position: int, depth: int) -> int:
    """The first `2 * depth` bits of a 64-bit position, right-aligned."""
    return position >> (64 - 2 * depth) if depth else 0


def deinterleave(cell: int) -> tuple[int, int]:
    """A cell prefix's two axes: its even bits and its odd bits, read one bit at a time."""
    x = y = 0
    for bit in range(32):
        x |= ((cell >> (2 * bit)) & 1) << bit
        y |= ((cell >> (2 * bit + 1)) & 1) << bit
    return x, y


def area_of(bbox, depth: int, extent) -> tuple[int, int, int, int]:
    """The inclusive cell coordinates at `depth` a bbox `[x0, y0, x1, y1]` spans, over an extent
    `(x_min, x_max, y_min, y_max)`: each corner's 32-bit quantisation, cut to `depth` bits."""
    from . import morton
    x_min, x_max, y_min, y_max = extent
    x0, y0, x1, y1 = bbox

    def cut(q: int) -> int:
        return q >> (32 - depth) if depth else 0

    xs = sorted((cut(morton.fixed32(x0, x_min, x_max)), cut(morton.fixed32(x1, x_min, x_max))))
    ys = sorted((cut(morton.fixed32(y0, y_min, y_max)), cut(morton.fixed32(y1, y_min, y_max))))
    return xs[0], xs[1], ys[0], ys[1]


def lift(count: int, total: int, reference_count: int, reference_total: int) -> float | None:
    if reference_count == 0 or total == 0:
        return None
    return (count / total) / (reference_count / reference_total)


def table(
    *,
    items: set[int],
    reference: set[int] | None,
    groups: dict[object, set[int]] | None,
    pick: tuple[str, object] | None,
    listable=lambda key: True,
    depth: int | None,
    position: dict[int, int],
    area: tuple[int, int, int, int] | None = None,
) -> tuple[dict, list[Row]]:
    """One grouping's table head and rows.

    `groups` maps each group's key to its entities; `None` is a grouping with no outer level.
    `pick` is `("top", n)` or `("named", [key, ...])`. `listable(key)` says whether a named key is
    one the principal may be listed, and a key absent from `groups` is listable when it passes.
    `area` is `(x0, x1, y0, y1)`, inclusive cell coordinates at `depth`.
    """
    def in_area(entity: int) -> bool:
        if area is None:
            return True
        x, y = deinterleave(cell_of(position[entity], depth))
        return area[0] <= x <= area[1] and area[2] <= y <= area[3]
    total = len(items)
    reference_total = len(reference) if reference is not None else None
    head: dict = {"total": total}
    if reference is not None:
        head["reference_total"] = reference_total

    if groups is None:
        rows = _rows(None, None, items, reference, depth, position, total, reference_total, True,
                     in_area)
        return head, rows

    head["groups"] = sum(1 for members in groups.values() if members & items)
    kind, arg = pick
    if kind == "top":
        ranked = sorted(
            (key for key, members in groups.items() if members & items),
            key=lambda key: (-len(groups[key] & items), key),
        )
        listed = ranked[:arg]
        always = False
    else:
        listed = []
        for key in arg:
            if key not in listed and listable(key):
                listed.append(key)
        always = True

    grouped = set().union(*groups.values()) if groups else set()
    in_listed = set().union(*(groups.get(key, set()) for key in listed)) if listed else set()
    rows: list[Row] = []
    for key in listed:
        members = groups.get(key, set())
        rows += _rows(
            "listed", key, items & members,
            reference & members if reference is not None else None,
            depth, position, total, reference_total, always, in_area,
        )
    for name, members in (("rest", grouped - in_listed), ("none", None)):
        in_set = items & members if members is not None else items - grouped
        in_reference = None
        if reference is not None:
            in_reference = reference & members if members is not None else reference - grouped
        rows += _rows(name, None, in_set, in_reference, depth, position, total, reference_total, False,
                      in_area)
    return head, rows


def _rows(group, key, in_set, in_reference, depth, position, total, reference_total, always,
          in_area=lambda entity: True):
    """One group's rows: one without cells, or one per non-empty cell in the area."""
    if depth is None:
        count = len(in_set)
        reference_count = len(in_reference) if in_reference is not None else None
        if not always and group is not None and count == 0 and not reference_count:
            return []
        return [_row(group, key, None, count, reference_count, total, reference_total)]
    in_set = {entity for entity in in_set if in_area(entity)}
    in_reference = {entity for entity in in_reference if in_area(entity)} if in_reference is not None else None
    counts: dict[int, int] = {}
    for entity in in_set:
        cell = cell_of(position[entity], depth)
        counts[cell] = counts.get(cell, 0) + 1
    reference_counts: dict[int, int] = {}
    for entity in in_reference or ():
        cell = cell_of(position[entity], depth)
        reference_counts[cell] = reference_counts.get(cell, 0) + 1
    return [
        _row(
            group, key, cell, counts.get(cell, 0),
            reference_counts.get(cell, 0) if in_reference is not None else None,
            total, reference_total,
        )
        for cell in sorted(set(counts) | set(reference_counts))
    ]


def _row(group, key, cell, count, reference_count, total, reference_total) -> Row:
    return Row(
        group=group,
        key=key,
        cell=cell,
        count=count,
        reference_count=reference_count,
        lift=lift(count, total, reference_count, reference_total)
        if reference_count is not None
        else None,
    )


def drawn_cut(parents: dict, passing: set, budget: int | None, prune: bool) -> set:
    """What a treed layer draws, from its stored parent lists and the nodes that pass, read
    straight from the rule: the viewer's tree is the passing nodes, each linked to its nearest
    passing ancestors on every path, and depth is counted in passing nodes. At depth `d` each
    head (a passing node, or with `prune` one with no passing node beneath it) is represented on
    each of its root paths by the deepest node at or above `d`. The cut drawn is the deepest whose
    size fits `budget`, depth 0 where none does, and the finest with no budget."""

    def induced(node) -> set:
        out: set = set()
        for up in parents.get(node, []):
            out |= {up} if up in passing else induced(up)
        return out

    ups = {node: induced(node) for node in passing}

    def rung(node) -> int:
        return max((rung(up) + 1 for up in ups[node]), default=0)

    def paths(node) -> list[list]:
        if not ups[node]:
            return [[node]]
        return [path + [node] for up in ups[node] for path in paths(up)]

    def ancestors(node) -> set:
        out: set = set()
        stack = [node]
        while stack:
            for up in ups[stack.pop()]:
                if up not in out:
                    out.add(up)
                    stack.append(up)
        return out

    beneath = {up for node in passing for up in ancestors(node)}
    heads = [node for node in passing if not prune or node not in beneath]

    def at(depth: int) -> set:
        return {
            max((n for n in path if rung(n) <= depth), key=rung)
            for head in heads
            for path in paths(head)
        }

    finest = at(len(passing))
    if budget is None or len(finest) <= budget:
        return finest
    for depth in range(len(passing) - 1, -1, -1):
        if len(at(depth)) <= budget:
            return at(depth)
    return at(0)


def frontier(parents: dict, drawn: set) -> set:
    """The nodes of `drawn` with no node of `drawn` beneath them on any stored path."""
    above: set = set()
    stack = [up for node in drawn for up in parents.get(node, [])]
    while stack:
        node = stack.pop()
        if node not in above:
            above.add(node)
            stack.extend(parents.get(node, []))
    return drawn - above
