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

from ._sources import Refusal
from ._toml import Inline, dumps

KINDS = ("view", "view_group", "vocabulary", "attribute", "layer")

#: A layer that named no `views` takes every view, resolved when the document is written so that a
#: view declared after the layer is included.
VIEWS_ALL = "__views_all__"

#: `anchor=True` on a view, kept beside the block and never written: the TOML says
#: `allocation_view` instead.
ANCHOR = "__anchor__"

HIERARCHY_KINDS = ("flat", "nested", "dag", "stacked", "tiered")
LEVELLED = ("stacked", "tiered")


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

    def to_toml(self, *args, **kwargs) -> str:
        return dumps(self.document(*args, **kwargs))


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
        block["values"] = list(values) if not isinstance(values, dict) else Inline(values)
    if reserved:
        block["reserved"] = list(reserved)
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
            f"attribute {name!r}: a group-scoped attribute is stage S5 of python-sdk.md §12. "
            f"Write the block through declare('attribute', …) until it lands"
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
    computed: Sequence[str] = ("centroid", "box", "hull"),
    supplied: Sequence[Any] | None = None,
    scope: Any = "entity",
    fields: dict | None = None,
    entity_field: str | None = None,
    title: str | None = None,
) -> dict:
    _refuse_later_stages(name, membership, shape, artifacts, layout, scope)
    if kind not in HIERARCHY_KINDS:
        raise Refusal(f"layer {name!r}: {kind!r} is not a hierarchy kind: {HIERARCHY_KINDS}")
    if kind in LEVELLED and not levels:
        raise Refusal(f"layer {name!r}: a {kind} layer declares its levels. Give levels=")
    if levels and kind in ("nested", "dag"):
        raise Refusal(
            f"layer {name!r}: a {kind} layer's structure is its edges, so levels are refused"
        )
    routes = [r for r in (from_column, members, source) if r is not None]
    if not routes:
        raise Refusal(
            f"layer {name!r}: an enumerated layer's membership comes from a column "
            f"(from_column=) or from tables (source= and members=)"
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

    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    if source is not None:
        block["source"] = source
    block["views"] = list(views) if views is not None else VIEWS_ALL
    block["membership"] = membership
    block["hierarchy"] = Inline({"kind": kind, "prune_children": prune_children})
    # Written whenever the SDK chose it, since under `open` a mistyped key is a permanent artifact.
    block["value_set"] = value_set or ("closed" if source is not None else "open")
    block["visibility"] = visibility
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    if withdraw_on_member_deletion:
        block["withdraw_on_member_deletion"] = True
    if depends_on:
        block["depends_on"] = list(depends_on)
    if fields is not None:
        block["fields"] = Inline(fields)
    content: dict[str, Any] = {"computed": list(computed)}
    if supplied:
        content["supplied"] = [_supplied(name, entry) for entry in supplied]
        block["content"] = content
    else:
        block["content"] = Inline(content)
    if levels:
        block["levels"] = [_level(entry) for entry in levels]
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


def _refuse_later_stages(name, membership, shape, artifacts, layout, scope) -> None:
    if membership != "enumerated":
        raise Refusal(
            f"layer {name!r}: spatial and attribute membership are stage S4 of python-sdk.md §12. "
            f"Write the block through declare('layer', …) until it lands"
        )
    for value, what in ((shape, "shape="), (artifacts, "inline artifacts"), (layout, "layout=")):
        if value:
            raise Refusal(
                f"layer {name!r}: {what} is stage S4 of python-sdk.md §12. Write the block "
                f"through declare('layer', …) until it lands"
            )
    if scope != "entity":
        raise Refusal(
            f"layer {name!r}: a group-scoped layer is stage S5 of python-sdk.md §12. Write the "
            f"block through declare('layer', …) until it lands"
        )


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
