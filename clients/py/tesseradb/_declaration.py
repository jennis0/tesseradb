"""The declaration: blocks, the typed verbs' compilation into them, and the TOML (§4).

Each `declare_*` verb builds one block of `configuration.md`'s declaration and appends it here.
The SDK writes the TOML and `tessera check` reads that file, so the mapping from verb to block is
checked by the binary rather than mirrored in Python.

What the TOML always says (§4.8): every source name on every block, the allocation view, the value
set on every layer, and the disclosure controls on every layer and vocabulary, whether the user
said them or a default did. A reader of `schema.toml` sees the whole declaration without knowing
the SDK's defaults.
"""

from __future__ import annotations

from typing import Any, Iterable, Sequence

from ._refusal import Refusal
from ._toml import Inline

KINDS = ("view", "view_group", "vocabulary", "attribute", "layer")

#: A layer that named no `views` takes every view, resolved when the document is written so that a
#: view declared after the layer is included.
VIEWS_ALL = "__views_all__"

#: `anchor=True` on a view, kept beside the block and never written: the TOML says
#: `allocation_view` instead.
ANCHOR = "__anchor__"

HIERARCHY_KINDS = ("flat", "nested", "dag", "stacked", "tiered")
LEVELLED = ("stacked", "tiered")
SHAPE_KINDS = ("bbox", "circle", "ellipse", "polygon")
LAYOUTS = ("rows", "column", "list")
SPACES = ("view", "wgs84")

#: What a layer's engine derives per viewer when the caller names nothing (§4.6). A spatial layer
#: takes this list without `hull`: an artifact has one drawn geometry, served through one
#: `shape_x`/`shape_y` column pair, so a derived hull beside a membership shape is refused at the
#: build. ⊘ python-sdk.md §4.6 states one default for every membership; the binary refuses it on a
#: spatial layer, and this is the narrowing rather than a refusal at the call.
DERIVED = ("centroid", "box", "hull")
SPATIAL_DERIVED = ("centroid", "box")


class Declaration:
    def __init__(self) -> None:
        self.blocks: dict[str, list[dict]] = {kind: [] for kind in KINDS}

    def add(self, kind: str, block: dict) -> dict:
        if kind not in KINDS:
            raise Refusal(f"declare: {kind!r} is not a block kind: {', '.join(KINDS)}")
        block = dict(block)
        name = block.get("name")
        if name is not None and any(b.get("name") == name for b in self.blocks[kind]):
            raise Refusal(f"declare: a {kind} named {name!r} is already declared")
        self.blocks[kind].append(block)
        return block

    def layer(self, name: str) -> dict:
        for block in self.blocks["layer"]:
            if block.get("name") == name:
                return block
        raise Refusal(f"no layer named {name!r} is declared")

    def view_names(self) -> list[str]:
        return [block["name"] for block in self.blocks["view"]]

    def attribute_names(self) -> set[str]:
        return {block["name"] for block in self.blocks["attribute"]}

    def vocabulary_names(self) -> set[str]:
        return {block["name"] for block in self.blocks["vocabulary"]}

    def allocation_view(self) -> str | None:
        """The first declared view unless another says `anchor=True` (decision 0112).

        Written into the TOML in either case, so a rebuild that reorders the blocks cannot re-key
        the corpus.
        """
        anchored = [b["name"] for b in self.blocks["view"] if b.get(ANCHOR)]
        if anchored:
            return anchored[0]
        names = self.view_names()
        return names[0] if names else None

    def document(
        self,
        sources: dict[str, str],
        default_source: str | None,
        inferred_attributes: Sequence[dict] = (),
        inferred_vocabularies: Sequence[dict] = (),
    ) -> dict[str, Any]:
        document: dict[str, Any] = {}
        if sources:
            document["sources"] = dict(sources)
        defaults: dict[str, Any] = {}
        if default_source is not None:
            defaults["source"] = default_source
        allocation_view = self.allocation_view()
        if allocation_view is not None:
            defaults["allocation_view"] = allocation_view
        if defaults:
            document["defaults"] = defaults

        views = self.view_names()
        declared_attributes = self.attribute_names()
        declared_vocabularies = self.vocabulary_names()
        document["view"] = [
            {k: v for k, v in block.items() if k != ANCHOR} for block in self.blocks["view"]
        ]
        if self.blocks["view_group"]:
            document["view_group"] = [dict(b) for b in self.blocks["view_group"]]
        vocabularies = [dict(b) for b in self.blocks["vocabulary"]]
        vocabularies += [
            dict(b) for b in inferred_vocabularies if b["name"] not in declared_vocabularies
        ]
        if vocabularies:
            document["vocabulary"] = vocabularies
        attributes = [dict(b) for b in self.blocks["attribute"]]
        attributes += [dict(b) for b in inferred_attributes if b["name"] not in declared_attributes]
        if attributes:
            document["attribute"] = attributes
        layers = []
        for block in self.blocks["layer"]:
            block = dict(block)
            if block.get("views") == VIEWS_ALL:
                block["views"] = list(views)
            layers.append(block)
        if layers:
            document["layer"] = layers
        return document


# ------------------------------------------------------------------ the typed verbs' blocks


def view_block(
    name: str,
    source: str | None,
    x: str = "x",
    y: str = "y",
    access: str | None = None,
    default_label: str | None = "public",
    extent: Any = None,
    projection: str = "none",
    visibility: Any = "public",
    anchor: bool = False,
    title: str | None = None,
) -> dict:
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["projection"] = projection
    if source is not None:
        block["source"] = source
    block["fields"] = _coordinate_fields(x, y, projection)
    block["extent"] = _extent(extent, projection)
    point_visibility: dict[str, str] = {}
    if access is not None:
        point_visibility["field"] = access
    if default_label is not None:
        point_visibility["default"] = default_label
    if not point_visibility:
        raise Refusal(
            f"view {name!r}: a view names no label for any point. Give access= or default_label="
        )
    block["point_visibility"] = Inline(point_visibility)
    block["visibility"] = visibility
    block[ANCHOR] = anchor
    return block


def _coordinate_fields(x: str, y: str, projection: str) -> Inline:
    """Under a projection the coordinate columns are `lon` and `lat`, in that order (§1)."""
    if projection == "none":
        return Inline({"x": x, "y": y})
    return Inline({"lon": "lon" if x == "x" else x, "lat": "lat" if y == "y" else y})


def _extent(extent: Any, projection: str) -> Any:
    if extent is not None:
        return Inline(extent) if isinstance(extent, dict) else extent
    if projection != "none":
        # A projected view takes `"auto"` or a longitude/latitude box; the margin spelling is
        # refused, the outward snap already supplying headroom.
        return "auto"
    # The frame is fitted to the staged rows and widened by half the fitted box's width on each
    # side (§6.1, assumed). A frame is index configuration and does not change for the life of the
    # view, so the headroom is for the rows a notebook adds later.
    return Inline({"auto": True, "margin": 0.5})


def vocabulary_block(
    name: str,
    width: str | None = None,
    closed: bool = False,
    visibility: str = "public",
    source: str | None = None,
    values: Any = None,
    reserved: Sequence[int] | None = None,
    fields: dict | None = None,
    title: str | None = None,
) -> dict:
    if closed and source is None and values is None:
        raise Refusal(
            f"vocabulary {name!r}: a closed value set reads its values from a source or carries "
            f"them inline. Give source= or values="
        )
    if visibility not in ("public", "derived"):
        raise Refusal(
            f"vocabulary {name!r}: visibility is 'public' or 'derived' — one axis, two settings, "
            f"and the slot takes no label (decision 0090). Not {visibility!r}"
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["width"] = width or "u16"
    block["value_set"] = "closed" if closed else "open"
    block["visibility"] = visibility
    if source is not None:
        block["source"] = source
    if fields is not None:
        block["fields"] = Inline(fields)
    if values is not None:
        # A list of keys, or a `key = code` table pinning each code so a rebuild preserves it.
        block["values"] = Inline(values) if isinstance(values, dict) else list(values)
    if reserved:
        block["reserved"] = [int(code) for code in reserved]
    return block


def attribute_block(
    name: str,
    type: str,
    source: str | None = None,
    field: str | None = None,
    vocabulary: str | None = None,
    render: bool | None = None,
    index: bool | None = None,
    analyser: str | None = None,
    scope: Any = "entity",
    fields: dict | None = None,
    entity_id_field: str | None = None,
    title: str | None = None,
) -> dict:
    if type == "category" and vocabulary is None:
        raise Refusal(f"attribute {name!r}: a category names its vocabulary. Give vocabulary=")
    if scope != "entity":
        raise Refusal(
            f"attribute {name!r}: not built yet, a group-scoped attribute. "
            f"declare('attribute', block) writes the block as given"
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["type"] = type
    if vocabulary is not None:
        block["vocabulary"] = vocabulary
    if field is not None:
        block["field"] = field
    if source is not None:
        block["source"] = source
    if entity_id_field is not None:
        block["entity_id_field"] = entity_id_field
    if analyser is not None:
        block["analyser"] = analyser
    if fields is not None:
        block["fields"] = Inline(fields)
    if render is not None:
        block["render"] = render
    if index is not None:
        block["index"] = index
    return block


def layer_block(
    name: str,
    kind: str,
    points_source: str | None,
    views: Iterable[str] | None = None,
    source: str | None = None,
    members: str | None = None,
    from_column: str | None = None,
    artifacts: Any = None,
    membership: Any = "enumerated",
    value_set: str | None = None,
    levels: Sequence[Any] | None = None,
    prune_children: bool = False,
    shape: Any = None,
    default_space: str = "view",
    layout: str | None = None,
    visibility: Any = "public",
    artifact_visibility: Any = "inherited",
    require_member_visibility: Any = "none",
    withdraw_on_member_deletion: bool = False,
    depends_on: Sequence[str] | None = None,
    computed: Sequence[str] = DERIVED,
    supplied: Sequence[Any] | None = None,
    scope: Any = "entity",
    fields: dict | None = None,
    entity_field: str | None = None,
    title: str | None = None,
) -> dict:
    if kind not in HIERARCHY_KINDS:
        raise Refusal(f"layer {name!r}: {kind!r} is not a hierarchy kind: {HIERARCHY_KINDS}")
    membership_value, how = _membership(name, membership)
    group = _scope(name, scope, fields)
    if withdraw_on_member_deletion:
        raise Refusal(
            f"layer {name!r}: withdraw_on_member_deletion is specified and not built "
            f"(annotation-write-cycle.md §6.1). The fold has no artifact-withdrawal path, so a "
            f"deleted member shrinks the membership and the artifact stands. Drop the parameter"
        )
    if kind in LEVELLED and not levels:
        raise Refusal(f"layer {name!r}: a {kind} layer declares its levels. Give levels=")
    if levels and kind in ("nested", "dag"):
        raise Refusal(
            f"layer {name!r}: a {kind} layer's structure is its edges, so levels are refused. "
            f"Drop levels=, or declare the layer as tiered"
        )
    if layout is not None and layout not in LAYOUTS:
        raise Refusal(f"layer {name!r}: the serving-layout pin is one of {LAYOUTS}, not {layout!r}")

    shape_block = _shape(name, shape, how)
    if how == "spatial":
        computed = SPATIAL_DERIVED if computed is DERIVED else computed
        if "hull" in computed:
            raise Refusal(
                f"layer {name!r}: an artifact has one drawn geometry, served through one shape "
                f"column pair, so a derived hull beside a membership shape is refused at the "
                f'build. Drop "hull" from computed='
            )
    if how != "spatial" and default_space != "view":
        raise Refusal(
            f"layer {name!r}: default_space= is the space a shape is written in, and this layer "
            f'declares no shape. Give membership="spatial" with shape=, or drop default_space='
        )
    if how == "attribute":
        _refuse_beside_an_attribute_membership(name, kind, levels, supplied, depends_on, layout,
                                               artifact_visibility, members, from_column)
    rows = _artifact_rows(name, artifacts, how)
    _refuse_a_route_clash(name, how, source, members, from_column, rows, points_source)

    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    if source is not None:
        block["source"] = source
    if group is not None:
        block["scope"] = Inline({"group": group})
    block["views"] = list(views) if views is not None else VIEWS_ALL
    block["membership"] = membership_value
    if how == "spatial":
        block["default_space"] = default_space
    block["hierarchy"] = Inline({"kind": kind, "prune_children": prune_children})
    # Written whenever the SDK chose it, since under `open` a mistyped key is a permanent artifact.
    block["value_set"] = value_set or _value_set(how, source, rows)
    if layout is not None:
        block["layout"] = layout
    block["visibility"] = visibility
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    if depends_on:
        block["depends_on"] = list(depends_on)
    if fields is not None:
        block["fields"] = Inline(fields)
    if how != "attribute":
        # A predicate layer's artifacts are the column's distinct values, so it declares no
        # content: the surface refuses one, there being nothing to carry it.
        content: dict[str, Any] = {"computed": list(computed)}
        if supplied:
            content["supplied"] = [_supplied(name, entry) for entry in supplied]
            block["content"] = content
        else:
            block["content"] = Inline(content)
    if levels:
        block["levels"] = [_level(entry) for entry in levels]
    if shape_block is not None:
        block["shape"] = shape_block
    if rows is not None:
        block["artifacts"] = rows
    if from_column is not None:
        # At the first commit a from-column layer compiles to `[layer.members]` reading the points
        # source with the column as `key` (§4.6).
        block["members"] = {
            "source": points_source,
            "fields": Inline({"key": from_column, "entity": entity_field or "entity_id"}),
        }
    elif members is not None:
        block["members"] = {"source": members}
    return block


def _membership(name: str, membership: Any) -> tuple[Any, str]:
    """`enumerated`, `spatial`, or the predicate `{ attribute = <field> }`."""
    if isinstance(membership, dict):
        if set(membership) != {"attribute"}:
            raise Refusal(
                f"layer {name!r}: an attribute membership is {{'attribute': field}} and names "
                f"nothing else, not {sorted(membership)!r}"
            )
        return Inline({"attribute": membership["attribute"]}), "attribute"
    if membership not in ("enumerated", "spatial"):
        raise Refusal(
            f"layer {name!r}: membership is 'enumerated', 'spatial' or {{'attribute': field}}, "
            f"not {membership!r}"
        )
    return membership, membership


def _scope(name: str, scope: Any, fields: dict | None) -> str | None:
    """`entity`, or the view group a group-scoped layer keeps one artifact set per view of."""
    if scope == "entity" or scope is None:
        return None
    group = scope["group"] if isinstance(scope, dict) else scope
    if isinstance(scope, dict) and set(scope) != {"group"}:
        raise Refusal(
            f"layer {name!r}: a scoped layer names one view group, as {{'group': name}}, not "
            f"{sorted(scope)!r}"
        )
    if not fields or "view" not in dict(fields):
        raise Refusal(
            f"layer {name!r}: a layer scoped to group {group!r} keys its artifacts per view, so "
            f"its rows carry the view. Give fields={{'view': column}}"
        )
    return group


def _shape(name: str, shape: Any, how: str) -> dict | None:
    if shape is None:
        return None
    if how != "spatial":
        raise Refusal(
            f"layer {name!r}: a shape is the membership of a spatial layer, and nothing evaluates "
            f'one elsewhere. Give membership="spatial", or drop shape='
        )
    kind = shape["kind"] if isinstance(shape, dict) else shape
    if isinstance(shape, dict) and set(shape) != {"kind"}:
        raise Refusal(
            f"layer {name!r}: a shape declares its kind and nothing else, every kind being exact "
            f"(polygon-membership.md §6.1). Drop {sorted(set(shape) - {'kind'})!r}"
        )
    if kind not in SHAPE_KINDS:
        raise Refusal(f"layer {name!r}: a shape kind is one of {SHAPE_KINDS}, not {kind!r}")
    return {"kind": kind}


def _refuse_beside_an_attribute_membership(
    name, kind, levels, supplied, depends_on, layout, artifact_visibility, members, from_column
) -> None:
    """A predicate layer's artifacts are a column's distinct values, so most keys have no subject.

    Each of these would register a layer that is reachable and serves nothing
    (configuration.md §1, `[[layer]]`'s `membership` row).
    """
    if kind != "flat":
        raise Refusal(
            f"layer {name!r}: an attribute membership has no edges to carry a hierarchy, so its "
            f'kind is "flat", not {kind!r}'
        )
    for value, what, instead in (
        (levels, "levels=", "declare the layer as tiered over a members table"),
        (supplied, "supplied=", "supply the content on an enumerated layer"),
        (depends_on, "depends_on=", "declare the dependency on an enumerated layer"),
        (layout, "layout=", "drop it: the column is the membership, so there is no second form"),
        (members, "members=", "drop it: the column is the membership"),
        (from_column, "from_column=", "drop it: the column is the membership"),
    ):
        if value:
            raise Refusal(
                f"layer {name!r}: an attribute membership derives its artifacts from the column, "
                f"so {what} names nothing it carries. Instead, {instead}"
            )
    if isinstance(artifact_visibility, dict) and artifact_visibility.get("field"):
        raise Refusal(
            f"layer {name!r}: an attribute membership publishes no artifact rows, so there is no "
            f"column for artifact_visibility to read. Give a label or 'inherited'"
        )


def _refuse_a_route_clash(name, how, source, members, from_column, rows, points_source) -> None:
    if source is not None and rows is not None:
        raise Refusal(
            f"layer {name!r}: artifacts= is the roster, so it takes no source=. Drop one of them"
        )
    if from_column is not None and (source is not None or members is not None):
        raise Refusal(
            f"layer {name!r}: from_column= is the membership, so it takes no source= or members="
        )
    if from_column is not None and points_source is None:
        raise Refusal(
            f"layer {name!r}: from_column= reads the points source, and none is staged as the "
            f"default or named by a view"
        )
    if how == "spatial" and (members is not None or from_column is not None):
        raise Refusal(
            f"layer {name!r}: a spatial artifact's shape is its whole membership, resolved per "
            f"request, so there is no stored member set. Drop members= and from_column=, or "
            f'declare the layer membership="enumerated"'
        )
    if how == "attribute":
        return
    if any(route is not None for route in (from_column, members, source, rows)):
        return
    if how == "spatial":
        raise Refusal(
            f"layer {name!r}: a spatial layer's artifacts each carry a shape, so it names the "
            f"table they are in or carries them inline. Give source= or artifacts="
        )
    raise Refusal(
        f"layer {name!r}: an enumerated layer's membership comes from a column (from_column=) or "
        f"from tables (source= and members=)"
    )


def _value_set(how: str, source: str | None, rows: Any) -> str:
    # A predicate layer's artifacts are the column's values, which the vocabulary already bounds.
    if how == "attribute":
        return "closed"
    return "closed" if (source is not None or rows is not None) else "open"


#: The artifact table's own columns, which an inline row spells canonically (configuration.md §1).
ARTIFACT_KEYS = (
    "key", "level", "members", "excluding", "bbox", "circle", "ellipse", "wkt", "space",
    "contents", "parent", "attached_layer", "attached_level", "attached_key",
)


def _artifact_rows(layer: str, artifacts: Any, how: str) -> list[dict] | None:
    """Inline `artifacts=`: a list of dicts, or a frame carrying the artifact table's columns."""
    if artifacts is None:
        return None
    if not isinstance(artifacts, (list, tuple)):
        artifacts = _frame_rows(artifacts)
    rows = [_artifact_row(layer, row, how) for row in artifacts]
    if not rows:
        raise Refusal(
            f"layer {layer!r}: artifacts= carries no row. Leave it out to declare an empty layer"
        )
    return rows


def _frame_rows(frame: Any) -> list[dict]:
    """A pyarrow table, or anything `pa.table` takes, as one dict per row without its nulls."""
    import pyarrow as pa

    table = frame if isinstance(frame, pa.Table) else pa.table(frame)
    columns = {name: table[name].to_pylist() for name in table.column_names}
    return [
        {name: values[i] for name, values in columns.items() if values[i] is not None}
        for i in range(table.num_rows)
    ]


SHAPE_FIELDS = ("bbox", "circle", "ellipse", "wkt")


def _artifact_row(layer: str, row: Any, how: str) -> dict:
    row = dict(row)
    attached = row.pop("attached_to", None)
    if attached is not None:
        row["attached_layer"] = attached["layer"]
        row["attached_key"] = attached["key"]
        if attached.get("level") is not None:
            row["attached_level"] = int(attached["level"])
    unknown = [key for key in row if key not in ARTIFACT_KEYS]
    if unknown:
        raise Refusal(
            f"layer {layer!r}: an inline artifact is spelled with the artifact table's own keys, "
            f"so {sorted(unknown)!r} names nothing. The keys are {list(ARTIFACT_KEYS)}"
        )
    if "key" not in row:
        raise Refusal(f"layer {layer!r}: an inline artifact names itself. Give key=")
    if "members" in row and "excluding" in row:
        raise Refusal(
            f"layer {layer!r}, artifact {row['key']!r}: a membership is spelled by inclusion or "
            f"by exclusion, and both on one row name two sets. Drop members= or excluding="
        )
    carried = [field for field in SHAPE_FIELDS if field in row]
    if carried and how != "spatial":
        raise Refusal(
            f"layer {layer!r}, artifact {row['key']!r}: {carried[0]} is a shape, and this layer "
            f'evaluates none. Give membership="spatial" with shape=, or drop it'
        )
    if len(carried) > 1:
        raise Refusal(
            f"layer {layer!r}, artifact {row['key']!r}: an artifact carries its shape in its "
            f"layer's kind's field and no other, so {sorted(carried)!r} is two shapes"
        )
    if "space" in row and row["space"] not in SPACES:
        raise Refusal(
            f"layer {layer!r}, artifact {row['key']!r}: a shape is written in {SPACES[0]!r} or "
            f"{SPACES[1]!r}, not {row['space']!r}"
        )
    return {key: row[key] for key in ARTIFACT_KEYS if key in row}


def labels_block(
    name: str,
    source: str,
    members: str | None = None,
    content_requires: str | None = None,
    type: str = "text",
    require_member_visibility: Any = "none",
    artifact_visibility: Any = "inherited",
    title: str | None = None,
) -> dict:
    """The `[layer.labels]` block: a label set over the clustering it hangs from (§4.7)."""
    if content_requires is None:
        content_requires = "all" if members is not None else "inherited"
    if content_requires not in ("all", "inherited"):
        raise Refusal(
            f"labels {name!r}: the content gate is 'all' or 'inherited'; 'none' at that grain "
            f"would mean 'inherited'"
        )
    if content_requires == "all" and members is None:
        raise Refusal(
            f"labels {name!r}: content_requires='all' says the text was generated from its "
            f"members, so it names the generating set. Give members="
        )
    if content_requires == "inherited" and members is not None:
        raise Refusal(
            f"labels {name!r}: content_requires='inherited' says the text is true whether or not "
            f"any member exists, so it declares no generating set. Drop members="
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["source"] = source
    block["type"] = type
    block["membership"] = "enumerated"
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    block["content"] = {"require_member_visibility": content_requires}
    if members is not None:
        block["members"] = {"source": members}
    return block


def _artifact_visibility(value: Any) -> Any:
    if isinstance(value, dict):
        return Inline(value)
    return Inline({"default": value})


def _requirement(value: Any) -> Any:
    if isinstance(value, dict):
        return Inline(value)
    if value not in ("all", "any", "none"):
        raise Refusal(
            f"require_member_visibility: 'all', 'any', 'none', {{'fraction': p}} or "
            f"{{'count': n}}, not {value!r}"
        )
    return value


def _supplied(layer: str, entry: Any) -> dict:
    if isinstance(entry, dict):
        block = dict(entry)
    else:
        name, type, gate = entry
        block = {"name": name, "type": type, "require_member_visibility": gate}
    if block.get("require_member_visibility") not in ("all", "inherited"):
        raise Refusal(
            f"layer {layer!r}: supplied content is gated 'all' or 'inherited', not "
            f"{block.get('require_member_visibility')!r}"
        )
    return block


def _level(entry: Any) -> dict:
    if isinstance(entry, dict):
        return dict(entry)
    level, title, *rest = entry
    block: dict[str, Any] = {"level": level}
    if title is not None:
        block["title"] = title
    if rest and rest[0] is not None:
        block["zoom"] = list(rest[0])
    return block
