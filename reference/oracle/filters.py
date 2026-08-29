"""The filter expression, written as a **definition** — decision 0062's boolean tree over
per-entity attribute values, evaluated by a per-entity walk inside a candidate set.

**Pinned to decision 0062 and contracts §3.2 r26.** A node is a leaf — one column name mapped to
one operator — or a combinator, `all_of` / `any_of`, over sub-expressions. A category leaf takes
`eq` and `in`, whose values are the vocabulary's key (a string) or its code (an integer), freely
mixed; a `keyword` leaf — the one string family — takes `eq`, `in`, `prefix` and `contains`
against the value the item carries. Empty combinators are their operators' identities and differ: `all_of: []` matches the whole
candidate, `any_of: []` matches nothing. `match` is specified and unbuilt, so this module refuses it
the way the server does — by raising, never by evaluating a guess. The `region` leaf
(`selection-operand.md`, stage 4 of the shape work) is a shape — a polygon or a box in the view's
own coordinates — or a published artifact named by its `tessera_id`, evaluated here as an even-odd
walk over the entity's **stored** position on the quantised grid with a point on an edge inside
([`RegionColumn`]), and by artifact through the fixture's own membership; `none_of` over it is the
complement within the rowed entities, every one of which carries a position.

## What makes this a second implementation rather than a transcription

The engine evaluates a filter against the bundle's `attrs/<column>/` artefact — a value column
scanned under the candidate mask, which for a `keyword` holds `u32` ordinals into that layer's own
sorted dictionary rather than the strings themselves, with derived Roaring postings accelerating
the category case (filter-index §2). This module never opens that artefact. Its attribute values
come from the **fixture's own generation functions**: the conformance corpus is synthesised from a
seed, so the fixture knows what value every entity was *given*, upstream of what the build
*stored*. The two derivations meet only at the served surface, which is what makes agreement a differential — a
build that wrote the wrong value into `attrs/` and then served consistently by its own wrong
bytes disagrees with this oracle rather than being agreed with. (filter-index §9 offers the value
column itself as the conformance relation; taking the fixture's inputs instead is strictly
stronger for a synthesised corpus, for exactly the reason the geometry oracle reads the points
file the build consumed rather than `columns.arrow`.)

The evaluation is a literal per-entity walk — no bitmaps, no postings, no caching, no set algebra
beyond the union and intersection the combinators *are*. The engine's masked scan and derived
postings must reach the same sets by a different construction; if this module ever acquires an
accelerator, it stops being the definition.

## What the differential built on this covers, and what it does not

It covers: I12's mask half (`M_sel ⊆ M_auth`, per tile and per served set); the composition rule
(`all_of`/`any_of`, nested, with the empty identities); key-or-code equivalence; and the
unknown-value / hidden-value / valueless-value outcome equivalence (C11, per-point-attributes
§3.8). It does **not** cover: work-indistinguishability (a timing property, measured in probes and
deliberately not asserted by a conformance test — surface §9's C11 row); I12's frontier half and
I3 (no label service exists); Rule S over filter results (the suite's overlay machinery drives
suppression against its own servers, and no filter test drives it yet — stated in the test module
rather than silently absent).

**Post-build layers are the caller's to model, and need nothing here.** A flush publishes its own
extent, so a column the engine serves is the base plus every live extent; the per-entity definition
is unchanged by that — an entity holds one value whichever layer stores it — so a driver that
ingests adds the ingested entities to the column's `values` and asks the same question.
`conformance/tests/test_keyword_layers.py` does that across a base, two flush extents and a
compaction fold, which is where the layering is under test rather than assumed away.

## The mask is an input, not a product

`evaluate` takes the candidate set — `M_auth`, already composed — and returns a subset of it.
Deny-precedence composition is `mask.ChangeSet`'s job and happens before this module is asked
anything; a caller handing in a raw fragment would reproduce exactly the fragment-intersection
bug surface §5.1 exists to forbid, so the parameter is named `candidate` and documented as the
composed verdict.
"""

from __future__ import annotations

from dataclasses import dataclass


class UnknownColumn(Exception):
    """A leaf names a column the schema does not declare as filterable.

    Contracts §3.2: an unknown *column* is a `422`, an unknown *value* is an empty operand — the
    two must not be conflated, because refusing a value would make the filter surface an
    existence oracle over exactly what `visibility = "derived"` hides. This module mirrors the
    distinction: an unknown column raises (the test asserts the server's 422), while an unknown
    value falls out of the arithmetic below as an operand matching nothing.
    """


class UnbuiltOperator(Exception):
    """An operator the column's family does not take — `match`, `prefix` on a category, a range on
    a keyword — or one that is specified and unbuilt.

    Raised rather than evaluated, because guessing at semantics is how an oracle stops disagreeing:
    an oracle that pre-implemented a guess would ratify whichever behaviour the engine happened to
    ship. The server's answer to the same request is a `422`, for the reason contracts §3.2 gives —
    a column's family is deployment schema, so refusing discloses nothing, where refusing a *value*
    would be an existence oracle over the viewer's data.
    """


@dataclass(frozen=True)
class CategoryColumn:
    """One category column as the fixture planted it: per-entity **keys**, and the declared
    vocabulary's key→code pinning.

    `values` maps entity id → value key, with absent entities simply missing (the fixture's
    generation function returned `None`; an absent value matches no predicate, which is the
    presence-bitmap rule arrived at from the definition side). `codes` is the fixture's own
    `[vocabulary.values]` block — the declaration is the authority on codes, so resolving a code
    operand through it shares nothing with the bundle's stored column.
    """

    values: dict[int, str]
    codes: dict[str, int]

    def matches(self, entity: int, operand: "str | int") -> bool:
        """Does `entity` carry the value `operand` names — by key or by code, freely mixed?

        A key that is not in the vocabulary, or a code no value is pinned to, names nothing and
        matches nothing: the unknown-value rule, falling out of the arithmetic rather than being
        a branch. A key is never an integer (contracts §3.2), so the JSON type is the
        disambiguator and nothing else is.
        """
        held = self.values.get(entity)
        if held is None:
            return False
        if isinstance(operand, bool):
            # `True == 1` in Python; JSON `true` is not a code and must not resolve as one.
            return False
        if isinstance(operand, int):
            return self.codes.get(held) == operand
        return held == operand


def _string_matches(held: str, operator: str, operand, family: str) -> bool:
    """The four string predicates, over the bytes an entity holds.

    Written apart from the class that calls it because it is the *definition* of what the four
    operators mean, and records §4.4's `text` family — ⊘ specified, not built — will take its own
    subset of them. `in` is `eq` over a list: a generalisation of equality, not a category-only
    one, so a string family takes it too.
    """
    if operator == "eq":
        return held == operand
    if operator == "in":
        return any(held == value for value in operand)
    if operator == "prefix":
        return held.startswith(operand)
    if operator == "contains":
        return operand in held
    raise UnbuiltOperator(f"{family} operator {operator!r}")


@dataclass(frozen=True)
class KeywordColumn:
    """One `keyword` column as the fixture planted it: per-entity strings, byte-compared.

    **This holds no dictionary and no ordinals, and that absence is the whole of what makes the
    keyword differential a differential** (records §10). The engine stores a `u32` ordinal per
    entity into a *per-layer* sorted dictionary, resolves each needle inside the layer it is about
    to scan, and unions the layers; this module holds the strings the fixture planted and compares
    them. So the two sides agree only if the build interned, front-coded, renumbered and resolved
    correctly at every layer — an implementation that wrote one dictionary and then answered
    consistently by its own wrong ordinals disagrees here rather than being agreed with. Teaching
    this class the artefact's structure — a sorted key list, an ordinal per entity, a resolve —
    would make it a transcription and the agreement vacuous.

    **The only string family there is.** `utf8` is retired as a declared type, so every string
    column a deployment can declare is a keyword and publishes `keyword`; records §4.4's `text` is
    ⊘ specified and not built, and will be a second class here when it lands rather than an
    operator added to this one.
    """

    values: dict[int, str]

    def matches(self, entity: int, operator: str, operand) -> bool:
        held = self.values.get(entity)
        if held is None:
            # Absence is absence from the layer's presence bitmap, and no predicate matches it —
            # including `contains ""`, which matches every *value* and therefore every entity that
            # has one, never an entity that has none.
            return False
        return _string_matches(held, operator, operand, "keyword")


@dataclass(frozen=True)
class RegionColumn:
    """The `region` leaf's definition (`selection-operand.md` §5; `polygon-membership.md` §8),
    from the fixture's own inputs: each entity's **stored** position — the source coordinate
    quantised through the build's `fixed32`, which is what membership is of — and, for the leaf
    by artifact, each published shape's member set as the fixture computed it.

    `positions` maps entity id → `(qx, qy)` on the 32-bit grid; an entity absent here has no
    row (buffered, or in no view) and matches neither the region nor its negation, which is
    `filter-index.md` §5's ruling applied without exception. `artifacts` maps a `tessera_id`
    (as its decimal string) → the member entities of the artifact **this principal is served**;
    an id absent here is an empty operand — for an unknown id, a suppressed artifact, one
    withheld by criterion, exactly as the server answers. `quantise` is the build's `fixed32`
    over the fixture's extent.
    """

    positions: dict[int, tuple[int, int]]
    artifacts: dict[str, set[int]]
    quantise: "callable"

    def matches(self, entity: int, operand: dict) -> bool:
        """Is `entity`'s stored position inside the shape, or `entity` a member of the artifact?"""
        held = self.positions.get(entity)
        if held is None:
            return False
        if "artifact" in operand:
            return entity in self.artifacts.get(str(operand["artifact"]), set())
        px, py = held
        if "bbox" in operand:
            x0, y0, x1, y1 = operand["bbox"]
            return self.quantise(x0) <= px <= self.quantise(x1) and self.quantise(y0) <= py <= self.quantise(y1)
        if "polygon" in operand:
            return _inside_polygon(px, py, [[(x, y) for x, y in operand["polygon"]]], self.quantise)
        if "circle" in operand or "ellipse" in operand:
            raise UnbuiltOperator("the oracle evaluates a region's polygon, bbox and artifact spellings")
        raise ValueError(f"a region leaf is one of polygon, bbox, circle, ellipse or artifact: {operand!r}")


@dataclass(frozen=True)
class NumericColumn:
    """One numeric column as the fixture planted it: per-entity values, absent entities missing.
    `eq`, `in` and `range` — the last a bounds object of `gte`/`gt`/`lte`/`lt`, each side at most
    one — compared as Python numbers, which is IEEE's own comparison for the floats and exact for
    the integers, so a NaN satisfies no bound and no equality without a branch saying so."""

    values: dict[int, "int | float"]

    def matches(self, entity: int, operator: str, operand) -> bool:
        held = self.values.get(entity)
        if held is None:
            return False
        if operator == "eq":
            return held == operand
        if operator == "in":
            return any(held == v for v in operand)
        if operator == "range":
            if not isinstance(operand, dict) or not operand:
                raise ValueError("a range carries at least one bound")
            if "gte" in operand and not held >= operand["gte"]:
                return False
            if "gt" in operand and not held > operand["gt"]:
                return False
            if "lte" in operand and not held <= operand["lte"]:
                return False
            if "lt" in operand and not held < operand["lt"]:
                return False
            return True
        raise UnbuiltOperator(f"numeric operator {operator!r}")


def _on_segment(px, py, ax, ay, bx, by) -> bool:
    if (bx - ax) * (py - ay) - (by - ay) * (px - ax) != 0:
        return False
    return min(ax, bx) <= px <= max(ax, bx) and min(ay, by) <= py <= max(ay, by)


def _inside_polygon(px: int, py: int, rings, quantise) -> bool:
    """Even-odd over every ring, a point on an edge inside, vertices quantised as the build
    quantises them; integer arithmetic throughout. The half-open ray: an edge counts where
    exactly one end is above the ray. The same walk `conformance/tests/test_shape_membership.py`
    keeps for published shapes, restated here for the drawn one."""
    parity = False
    for ring in rings:
        qr = [(quantise(x), quantise(y)) for x, y in ring]
        n = len(qr)
        for i in range(n):
            ax, ay = qr[i]
            bx, by = qr[(i + 1) % n]
            if _on_segment(px, py, ax, ay, bx, by):
                return True
            if (ay > py) != (by > py):
                lhs = (bx - ax) * (py - ay)
                rhs = (px - ax) * (by - ay)
                if (lhs > rhs) if (by - ay) > 0 else (lhs < rhs):
                    parity = not parity
    return parity


def _carries_a_value(column, entity: int) -> bool:
    """Does `entity` hold any value in this column at all?

    **The predicate `none_of` rests on** (decision 0066). Every column kind records absence the
    same way here — the entity is simply not in `values` — which mirrors the artefact, where a
    category spends its reserved code 0 and every other family is left out of the presence bitmap.
    A region is total over rowed entities — every one carries a position — so its presence is
    the entity having a row at all (`selection-operand.md` §5).
    """
    if isinstance(column, RegionColumn):
        return entity in column.positions
    return entity in column.values


def _columns_named(expr: dict) -> set[str]:
    """Every column named anywhere under `expr` — what the one-column rule is checked against."""
    if not isinstance(expr, dict) or len(expr) != 1:
        raise ValueError(f"a filter node is one key: {expr!r}")
    (name, body), = expr.items()
    if name in ("all_of", "any_of", "none_of"):
        return set().union(*(_columns_named(sub) for sub in body)) if body else set()
    return {name}


def _leaf_matches(column, operator: str, operand, entity: int) -> bool:
    """One leaf against one entity — the smallest unit of the definition."""
    if isinstance(column, CategoryColumn):
        if operator == "eq":
            return column.matches(entity, operand)
        if operator == "in":
            return any(column.matches(entity, value) for value in operand)
        raise UnbuiltOperator(f"category operator {operator!r}")
    if isinstance(column, KeywordColumn):
        return column.matches(entity, operator, operand)
    if isinstance(column, NumericColumn):
        return column.matches(entity, operator, operand)
    raise TypeError(f"not a filter column: {column!r}")


def _region_matches(column, body: dict, entity: int) -> bool:
    """The `region` leaf against one entity — its body is the shape, not an operator."""
    if not isinstance(column, RegionColumn):
        raise UnknownColumn("region")
    if not isinstance(body, dict):
        raise ValueError(f"a region leaf is an object: {body!r}")
    return column.matches(entity, body)


def matches(expr: dict, columns: dict, entity: int) -> bool:
    """Does `entity` satisfy `expr`? — decision 0062's tree, one entity at a time.

    `expr` is the wire form exactly: `{"all_of": [...]}`, `{"any_of": [...]}`, `{"none_of": [...]}`,
    or a leaf
    `{"<column>": {"<operator>": <operand>}}`. `all` over an empty list is `True` and `any` is
    `False`, which are precisely the empty-combinator identities contracts §3.2 specifies — the
    definition inherits them from the quantifiers rather than special-casing them.
    """
    if not isinstance(expr, dict) or len(expr) != 1:
        raise ValueError(f"a filter node is one key: {expr!r}")
    (name, body), = expr.items()
    if name == "all_of":
        return all(matches(sub, columns, entity) for sub in body)
    if name == "any_of":
        return any(matches(sub, columns, entity) for sub in body)
    if name == "none_of":
        # **Carries a value in this column, and none of these matches it** — decision 0066, and
        # deliberately not `not any(...)`. The complement would admit every entity whose value is
        # merely *unreachable*, which inverts the failure arithmetic `filter-index.md` §5 rests on;
        # requiring presence keeps a negation positive. Written from the decision rather than from
        # the engine: an oracle that transcribed the implementation would ratify whatever it does.
        named = _columns_named(expr)
        if len(named) != 1:
            raise ValueError(
                f"a none_of names exactly one column, not {sorted(named)} — it requires the item "
                "to carry a value in the column it negates (decision 0066)"
            )
        (column_name,) = named
        if column_name not in columns:
            raise UnknownColumn(column_name)
        if not _carries_a_value(columns[column_name], entity):
            return False
        return not any(matches(sub, columns, entity) for sub in body)
    if name not in columns:
        raise UnknownColumn(name)
    if name == "region":
        # The reserved word: a shape, or a published artifact, as one leaf (selection-operand §2).
        return _region_matches(columns[name], body, entity)
    if not isinstance(body, dict) or len(body) != 1:
        raise ValueError(f"a leaf maps one column to one operator: {expr!r}")
    (operator, operand), = body.items()
    return _leaf_matches(columns[name], operator, operand, entity)


def evaluate(expr: dict, columns: dict, candidate: set[int]) -> set[int]:
    """`M_sel` for one expression: the members of `candidate` that satisfy `expr`.

    `candidate` is the **composed** authorised set — `M_auth` after deny precedence, never a raw
    fragment (see the module doc). Every result is a subset of it by construction, which is I12's
    mask half holding structurally in the oracle exactly as decision 0062 argues it holds in the
    engine: the leaves evaluate inside the candidate and union and intersection of subsets are
    subsets. The differential's job is to show the engine's masked scan agrees.
    """
    return {entity for entity in candidate if matches(expr, columns, entity)}
