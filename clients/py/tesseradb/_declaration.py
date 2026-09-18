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

import datetime
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

#: An attribute declared at a running service, kept beside the block and never written. Such a
#: column has no acquisition half: it is filled by `POST /control/values` rather than read from a
#: file, so the block names no source (§6.2 step 1).
FILLED = "__filled__"

#: `value_set` as the caller chose it, kept beside the block and never written: where the caller
#: chose none, the layer's inserts decide it when the document is written (§4.6).
CHOSEN_VALUE_SET = "__value_set__"

HIERARCHY_KINDS = ("flat", "nested", "dag", "stacked", "tiered")
LEVELLED = ("stacked", "tiered")
SHAPE_KINDS = ("bbox", "circle", "ellipse", "polygon")
LAYOUTS = ("rows", "column", "list")
SPACES = ("view", "wgs84")

#: What a layer's engine derives per viewer when the caller names nothing, and what a spatial layer
#: derives instead: an artifact has one drawn geometry, so a hull beside a membership shape is
#: refused at the build (python-sdk.md §4.6).
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
        if kind in ("view", "view_group") and name is not None:
            # A view of a group is addressed `<group>:<key>` and a plain view by its own name
            # (decision 0113), so one name held by both would make a request mean two things.
            other = "view_group" if kind == "view" else "view"
            if any(b.get("name") == name for b in self.blocks[other]):
                held = "view group" if other == "view_group" else "view"
                raise Refusal(
                    f"declare: {name!r} is already declared as a {held}, and a group and a plain "
                    f"view share one name space. Rename this one"
                )
        self.blocks[kind].append(block)
        return block

    def layer(self, name: str) -> dict:
        for block in self.blocks["layer"]:
            if block.get("name") == name:
                return block
        raise Refusal(f"no layer named {name!r} is declared")

    def group(self, name: str) -> dict:
        for block in self.blocks["view_group"]:
            if block.get("name") == name:
                return block
        raise Refusal(f"no view group named {name!r} is declared")

    def view_names(self) -> list[str]:
        return [block["name"] for block in self.blocks["view"]]

    def group_names(self) -> list[str]:
        return [block["name"] for block in self.blocks["view_group"]]

    def attribute_names(self) -> set[str]:
        return {block["name"] for block in self.blocks["attribute"]}

    def vocabulary_names(self) -> set[str]:
        return {block["name"] for block in self.blocks["vocabulary"]}

    def allocation_view(self) -> str | None:
        """The first declared view unless another says `anchor=True` (decision 0112).

        Written into the TOML in either case, so a rebuild that reorders the blocks cannot re-key
        the corpus. A declaration carrying groups alone names none the SDK could write — a
        group's views are the distinct values of its roster — and the build chooses.
        """
        anchored = [b["name"] for b in self.blocks["view"] if b.get(ANCHOR)]
        if anchored:
            return anchored[0]
        names = self.view_names()
        return names[0] if names else None

    def document(self, sources: dict[str, str]) -> dict[str, Any]:
        """The declaration as TOML's own shape (§4.8).

        **`[defaults].source` is never written.** Every block names the source its own inserts
        gave it, which is what lets a reader of `schema.toml` see the whole declaration without
        knowing the SDK's defaults; a block with no insert names none and is declared and empty,
        which configuration.md §2 allows.
        """
        document: dict[str, Any] = {}
        if sources:
            document["sources"] = dict(sources)
        defaults: dict[str, Any] = {}
        allocation_view = self.allocation_view()
        if allocation_view is not None:
            defaults["allocation_view"] = allocation_view
        if defaults:
            document["defaults"] = defaults

        # A layer that named no views takes every one: the plain views by name, and each group by
        # its own, which draws the layer on every view of it, present and future (views.md §3.5).
        views = self.view_names() + self.group_names()
        if self.blocks["view"]:
            document["view"] = [
                {k: v for k, v in block.items() if k != ANCHOR} for block in self.blocks["view"]
            ]
        if self.blocks["view_group"]:
            document["view_group"] = [dict(b) for b in self.blocks["view_group"]]
        if self.blocks["vocabulary"]:
            document["vocabulary"] = [dict(b) for b in self.blocks["vocabulary"]]
        if self.blocks["attribute"]:
            # `FILLED` is the SDK's own mark on a column declared at a running service and is
            # never written: such a block names no source, its cells being filled by the values
            # route rather than read from a file (§6.2 step 1).
            document["attribute"] = [
                {k: v for k, v in block.items() if k != FILLED}
                for block in self.blocks["attribute"]
            ]
        layers = []
        for block in self.blocks["layer"]:
            block = dict(block)
            if block.get("views") == VIEWS_ALL:
                block["views"] = list(views)
            if isinstance(block.get("labels"), dict):
                block["labels"] = dict(block["labels"])
            layers.append(block)
        if layers:
            document["layer"] = layers
        return document


# ------------------------------------------------------------------ the typed verbs' blocks


def view_block(
    name: str,
    default_label: str | None = "public",
    extent: Any = None,
    projection: str = "none",
    visibility: Any = "public",
    anchor: bool = False,
    title: str | None = None,
) -> dict:
    """One `[[view]]`: a frame, a projection and a gate, and no data (§4.2).

    Where its points are and which columns carry them come from `insert(view, table, id=, x=,
    y=, access=)`, which writes `source`, `fields` and `point_visibility.field` onto this block.
    """
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["projection"] = projection
    block["extent"] = _extent(extent, projection)
    point_visibility: dict[str, str] = {}
    if default_label is not None:
        point_visibility["default"] = default_label
    block["point_visibility"] = Inline(point_visibility)
    block["visibility"] = visibility
    block[ANCHOR] = anchor
    return block


#: The roster's own keys, which a metadata name may not take: the inline block and the roster
#: table would otherwise be ambiguous (views.md §3.2).
ROSTER_KEYS = ("key", "source", "visibility")


def view_group_block(
    name: str,
    metadata: dict | None = None,
    members: str | None = None,
    default_label: str | None = "public",
    extent: Any = None,
    projection: str | None = None,
    visibility: Any = "public",
    title: str | None = None,
) -> dict:
    """One `[[view_group]]`: a set of views sharing every setting, differing by a key (§4.3).

    The group's views and their metadata come from `insert(group, roster=table, key=, …)`, and
    its rows from `insert(group, table, id=, x=, y=, access=, view=)` with `view=` naming the
    column that says which view each row belongs to. `members` names another group whose views
    this group shares, and such a group declares no roster and no metadata of its own.
    """
    projection = projection or "none"
    declared = _metadata(name, metadata) if metadata else {}
    if members is not None and metadata:
        raise Refusal(
            f"view group {name!r}: keys, metadata and each view's own gate belong to the group "
            f"that owns them, so a group naming members={members!r} declares no metadata. "
            f"Declare it on {members!r}"
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["projection"] = projection
    if members is not None:
        block["members"] = members
    block["extent"] = _extent(extent, projection)
    point_visibility: dict[str, str] = {}
    if default_label is not None:
        point_visibility["default"] = default_label
    block["point_visibility"] = Inline(point_visibility)
    block["visibility"] = visibility
    if declared:
        block["metadata"] = Inline(
            {
                key: Inline(value) if isinstance(value, dict) else value
                for key, value in declared.items()
            }
        )
    return block


def _metadata(group: str, metadata: dict) -> dict:
    for key in metadata:
        if key in ROSTER_KEYS:
            raise Refusal(
                f"view group {group!r}: {key!r} is the roster's own key, so a per-view value of "
                f"that name would make the roster record ambiguous. Rename the metadata name"
            )
    return dict(metadata)


def metadata_names(block: dict) -> tuple[str, ...]:
    """The metadata names a group declared, which its roster insert names one column each of."""
    return tuple(dict(block.get("metadata") or {}))


def roster_body(record: dict, names: Sequence[str]) -> dict:
    """One roster row as `PUT /control/views/{group}/{key}` takes it (views.md §3.2).

    Every metadata name the group declared is here and typed against it (contracts §3.4 r55); a
    `timestamp_us` travels as microseconds since the epoch, JSON carrying no date type.
    """
    body: dict[str, Any] = {}
    if record.get("visibility") is not None:
        body["visibility"] = record["visibility"]
    body["metadata"] = {name: _metadata_value(record.get(name)) for name in names}
    return body


def _metadata_value(value: Any) -> Any:
    if isinstance(value, datetime.datetime):
        moment = value if value.tzinfo else value.replace(tzinfo=datetime.timezone.utc)
        return int(moment.timestamp() * 1_000_000)
    return value


def _extent(extent: Any, projection: str) -> Any:
    if extent is not None:
        return Inline(extent) if isinstance(extent, dict) else extent
    if projection != "none":
        # A projected view takes `"auto"` or a longitude/latitude box; the margin spelling is
        # refused, the outward snap already supplying headroom.
        return "auto"
    # The frame is fitted to the inserted rows and widened by half the fitted box's width on each
    # side (§6.1, assumed). A frame is index configuration and does not change for the life of the
    # view, so the headroom is for the rows a notebook adds later.
    return Inline({"auto": True, "margin": 0.5})


def vocabulary_block(
    name: str,
    width: str | None = None,
    closed: bool = False,
    visibility: str = "public",
    values: Any = None,
    reserved: Sequence[int] | None = None,
    title: str | None = None,
) -> dict:
    """One `[[vocabulary]]`, with no data of its own (§4.4).

    A closed set gives `values` inline or takes an `insert(name, table, key=, title=, code=)`; an
    open one minted from the data needs neither and may take an insert for titles.
    """
    if visibility not in ("public", "derived"):
        raise Refusal(
            f"vocabulary {name!r}: visibility is 'public' or 'derived'. The slot takes no access "
            f"label (decision 0090), and {visibility!r} is neither word"
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["width"] = width or "u16"
    block["value_set"] = "closed" if closed else "open"
    block["visibility"] = visibility
    if values is not None:
        # A list of keys, or a `key = code` table pinning each code so a rebuild preserves it.
        block["values"] = Inline(values) if isinstance(values, dict) else list(values)
    if reserved:
        block["reserved"] = [int(code) for code in reserved]
    return block


def attribute_block(
    name: str,
    type: str,
    render: bool | None = None,
    index: bool | None = None,
    vocabulary: str | None = None,
    analyser: str | None = None,
    scope: Any = "entity",
    title: str | None = None,
) -> dict:
    """One `[[attribute]]`: a type and its two flags, and nothing else (§4.5).

    It is filled by an `insert(name, table, id=, value=)`, or by name from a frame inserted into
    the allocation view (§3).
    """
    if type == "category" and vocabulary is None:
        raise Refusal(f"attribute {name!r}: a category names its vocabulary. Give vocabulary=")
    group = _attribute_scope(name, scope)
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["type"] = type
    if group is not None:
        block["scope"] = Inline({"group": group})
    if vocabulary is not None:
        block["vocabulary"] = vocabulary
    if analyser is not None:
        block["analyser"] = analyser
    if render is not None:
        block["render"] = render
    if index is not None:
        block["index"] = index
    return block


def _attribute_scope(name: str, scope: Any) -> str | None:
    """`entity`, or the view group whose views this column holds one value per (views.md §5)."""
    if scope == "entity" or scope is None:
        return None
    if isinstance(scope, dict) and set(scope) != {"group"}:
        raise Refusal(
            f"attribute {name!r}: a scoped attribute names one view group, as {{'group': name}}, "
            f"not {sorted(scope)!r}"
        )
    return scope["group"] if isinstance(scope, dict) else scope


def layer_block(
    name: str,
    kind: str,
    views: Iterable[str] | None = None,
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
    artifacts: Any = None,
    title: str | None = None,
) -> dict:
    """One `[[layer]]`, with no data of its own (§4.6).

    Its artifacts and its memberships come from its inserts: a key column
    (`insert(layer, table, id=, key=)`), or an artifacts table and a members table. `artifacts=`
    is the one exception and is a declaration rather than data — an authored roster written in
    the declaration itself, as configuration.md's inline `artifacts` array.
    """
    if kind not in HIERARCHY_KINDS:
        raise Refusal(f"layer {name!r}: {kind!r} is not a hierarchy kind: {HIERARCHY_KINDS}")
    membership_value, how = _membership(name, membership)
    group = _scope(name, scope)
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
                                               artifact_visibility)
    rows = _artifact_rows(name, artifacts, how)

    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    if group is not None:
        block["scope"] = Inline({"group": group})
    if views is not None:
        block["views"] = list(views)
    else:
        # A scoped layer's views may name only its own group and the groups sharing its views
        # (configuration.md §1), so "every view" is that group and nothing else.
        block["views"] = [group] if group is not None else VIEWS_ALL
    block["membership"] = membership_value
    if how == "spatial":
        block["default_space"] = default_space
    block["hierarchy"] = Inline({"kind": kind, "prune_children": prune_children})
    # Written whenever the SDK chose it, since under `open` a mistyped key is a permanent artifact.
    # A layer whose artifacts arrive in a table is closed, and one with a key column alone is open
    # (§4.6); which it is, is settled when the document is written, the inserts being made after.
    block["value_set"] = value_set or _value_set(how, rows)
    if layout is not None:
        block["layout"] = layout
    block["visibility"] = visibility
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    if depends_on:
        block["depends_on"] = list(depends_on)
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
    block[CHOSEN_VALUE_SET] = value_set is not None
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


def _scope(name: str, scope: Any) -> str | None:
    """`entity`, or the view group a group-scoped layer keeps one artifact set per view of."""
    if scope == "entity" or scope is None:
        return None
    if isinstance(scope, dict) and set(scope) != {"group"}:
        raise Refusal(
            f"layer {name!r}: a scoped layer names one view group, as {{'group': name}}, not "
            f"{sorted(scope)!r}"
        )
    return scope["group"] if isinstance(scope, dict) else scope


def _shape(name: str, shape: Any, how: str) -> dict | None:
    if shape is None:
        return None
    if how != "spatial":
        raise Refusal(
            f"layer {name!r}: a shape is the membership of a spatial layer, and nothing evaluates "
            f'one elsewhere. Give membership="spatial", or drop shape='
        )
    if isinstance(shape, dict) and set(shape) != {"kind"}:
        raise Refusal(
            f"layer {name!r}: a shape declares its kind and nothing else, every kind being exact "
            f"(polygon-membership.md §6.1). It names {sorted(shape)!r}"
        )
    kind = shape["kind"] if isinstance(shape, dict) else shape
    if kind not in SHAPE_KINDS:
        raise Refusal(f"layer {name!r}: a shape kind is one of {SHAPE_KINDS}, not {kind!r}")
    return {"kind": kind}


def _refuse_beside_an_attribute_membership(
    name, kind, levels, supplied, depends_on, layout, artifact_visibility
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


def _value_set(how: str, rows: Any) -> str:
    """`closed` where the layer's artifacts are a roster, `open` where they are minted (§4.6)."""
    # A predicate layer's artifacts are the column's values, which the vocabulary already bounds.
    if how == "attribute":
        return "closed"
    return "closed" if rows is not None else "open"


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
        artifacts = rows_of(artifacts)
    rows = [_artifact_row(layer, row, how) for row in artifacts]
    if not rows:
        raise Refusal(
            f"layer {layer!r}: artifacts= carries no row. Leave it out to declare an empty layer"
        )
    return rows


def rows_of(frame: Any) -> list[dict]:
    """A pyarrow table, or anything `pa.table` takes, as one dict per row without its nulls.

    A null is a row not naming that key rather than a value: the declaration has no spelling for
    one, and a publication record carries the keys the row wrote.
    """
    import pyarrow as pa

    table = frame if isinstance(frame, pa.Table) else pa.table(frame)
    columns = {name: table[name].to_pylist() for name in table.column_names}
    return [
        {name: values[i] for name, values in columns.items() if values[i] is not None}
        for i in range(table.num_rows)
    ]


SHAPE_FIELDS = ("bbox", "circle", "ellipse", "wkt")


ATTACHMENT_KEYS = ("layer", "key", "level")


def _artifact_row(layer: str, row: Any, how: str) -> dict:
    row = {key: value for key, value in dict(row).items() if value is not None}
    attached = row.pop("attached_to", None)
    if attached is not None:
        named = set(attached)
        if not {"layer", "key"} <= named or not named <= set(ATTACHMENT_KEYS):
            raise Refusal(
                f"layer {layer!r}: an attachment names the layer and the key it hangs from, and "
                f"the level where the target is not at level 0. It names {sorted(named)!r}"
            )
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
    content_requires: str = "inherited",
    type: str = "text",
    require_member_visibility: Any = "none",
    artifact_visibility: Any = "inherited",
    title: str | None = None,
) -> dict:
    """The `[layer.labels]` block: a label set over the clustering it hangs from (§4.7).

    Its text comes from `insert(name, {key: text})` or `insert(name, table, key=, text=)`, and,
    where the gate is `all`, its generating set from `insert(name, members=table, …)`.
    """
    if content_requires not in ("all", "inherited"):
        raise Refusal(
            f"labels {name!r}: the content gate is 'all' or 'inherited'; 'none' at that grain "
            f"would mean 'inherited'"
        )
    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    block["type"] = type
    block["membership"] = "enumerated"
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    block["content"] = {"require_member_visibility": content_requires}
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


# ------------------------------------------------------------------ what the inserts wrote


#: What configuration.md's own defaults call the identity column in each place one is read: a
#: view's `fields.entity_id` and an attribute's `entity_id_field` (§1), and a member row's
#: `fields.entity`.
CANONICAL_ENTITY_ID = "entity_id"
CANONICAL_MEMBER_ENTITY = "entity"


def bind(document: dict, inserts: Sequence[Any]) -> None:
    """Write onto every block the source and the column names its inserts gave it (§4.8).

    The SDK rewrites no file, so a column keeps whatever name it has in the user's own table and
    the declaration is what says where each one is (§3, configuration.md §1).
    """
    for insert in inserts:
        block = _block_for(document, insert)
        if block is None:
            continue
        getattr(_Bind, f"{insert.kind}_{insert.role}")(block, insert)
        for name, column in insert.named_attributes.items():
            _attribute_of_a_view(document, name, column, insert)


def bind_value_sets(document: dict, inserts: Sequence[Any]) -> None:
    """A layer's value set, where the caller chose none: its inserts decide it (§4.6)."""
    for block in document.get("layer", []):
        _settle_value_set(block, inserts)


def _block_for(document: dict, insert: Any) -> dict | None:
    if insert.kind == "labels":
        for block in document.get("layer", []):
            labels = block.get("labels")
            if isinstance(labels, dict) and labels.get("name") == insert.target:
                return labels
        return None
    for block in document.get(insert.kind, []):
        if block.get("name") == insert.target:
            return block
    return None


def _fields(block: dict, named: dict) -> None:
    """Add to a block's `fields`, which the SDK writes as an inline table."""
    named = {key: value for key, value in named.items() if value is not None}
    if not named:
        return
    block["fields"] = Inline({**dict(block.get("fields") or {}), **named})


class _Bind:
    """One method per (kind, role): what that insert writes onto its target's block."""

    @staticmethod
    def view_rows(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        _Bind._points(block, insert)

    @staticmethod
    def view_group_rows(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        _fields(block, {"view": insert.columns.get("view")})
        _Bind._points(block, insert)

    @staticmethod
    def _points(block: dict, insert: Any) -> None:
        columns = insert.columns
        if block.get("projection", "none") == "none":
            _fields(block, {"x": columns.get("x"), "y": columns.get("y")})
        else:
            _fields(block, {"lon": columns.get("lon"), "lat": columns.get("lat")})
        _fields(block, {"entity_id": columns.get("id")})
        if columns.get("access"):
            block["point_visibility"] = Inline(
                {**dict(block.get("point_visibility") or {}), "field": columns["access"]}
            )

    @staticmethod
    def view_group_roster(block: dict, insert: Any) -> None:
        named = {"key": insert.columns.get("key"), "visibility": insert.columns.get("visibility")}
        named.update(insert.metadata_columns)
        roster: dict[str, Any] = {"source": insert.source}
        named = {key: value for key, value in named.items() if value is not None}
        if named:
            roster["fields"] = Inline(named)
        block["views"] = roster

    @staticmethod
    def attribute_values(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        block["field"] = insert.columns["value"]
        if insert.columns["id"] != CANONICAL_ENTITY_ID:
            block["entity_id_field"] = insert.columns["id"]
        if insert.columns.get("view"):
            _fields(block, {"view": insert.columns["view"]})

    @staticmethod
    def layer_key(block: dict, insert: Any) -> None:
        """A key column: `[layer.members]` over the points, the column as `key` (§4.6)."""
        block["members"] = {
            "source": insert.source,
            "fields": Inline(
                {"key": insert.columns["key"], "entity": insert.columns["id"]}
            ),
        }

    @staticmethod
    def layer_artifacts(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        _fields(block, _renamed(insert, ARTIFACT_FIELDS))

    @staticmethod
    def layer_members(block: dict, insert: Any) -> None:
        members: dict[str, Any] = {"source": insert.source}
        named = _renamed(insert, MEMBER_FIELDS)
        if insert.columns["id"] != CANONICAL_MEMBER_ENTITY:
            named["entity"] = insert.columns["id"]
        if named:
            members["fields"] = Inline(named)
        block["members"] = members
        # The view a scoped layer's rows belong to is the layer's own field, which covers its
        # artifacts table and its members table alike (configuration.md §1).
        if insert.columns.get("view"):
            _fields(block, {"view": insert.columns["view"]})

    labels_text = layer_artifacts
    labels_members = layer_members

    @staticmethod
    def vocabulary_values(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        _fields(block, _renamed(insert, ("key", "title", "code")))


#: The artifact table's own column names, against which an insert's names are written only where
#: they differ: a column already carrying its canonical name needs no `fields` entry.
ARTIFACT_FIELDS = (
    "key", "level", "parent", "contents", "attached_layer", "attached_level", "attached_key",
    "members", "excluding", "space", "bbox", "circle", "ellipse", "wkt", "view", "visibility",
)
MEMBER_FIELDS = ("key", "level", "rank")


def _renamed(insert: Any, canonical: Sequence[str]) -> dict:
    return {
        name: insert.columns[name]
        for name in canonical
        if insert.columns.get(name) and insert.columns[name] != name
    }


def _attribute_of_a_view(document: dict, name: str, column: str, insert: Any) -> None:
    """An attribute filled from the allocation view's own frame, by name or by `columns=` (§3)."""
    for block in document.get("attribute", []):
        if block.get("name") != name:
            continue
        block["source"] = insert.source
        if column != name:
            block["field"] = column
        identity = insert.columns.get("id")
        if identity is not None and identity != CANONICAL_ENTITY_ID:
            block["entity_id_field"] = identity


def _settle_value_set(block: dict, inserts: Sequence[Any]) -> None:
    """A layer whose artifacts arrive in a table is closed; one with a key column is open (§4.6)."""
    if block.pop(CHOSEN_VALUE_SET, False) or block.get("artifacts"):
        return
    for insert in inserts:
        if insert.target == block.get("name") and insert.role == "artifacts":
            block["value_set"] = "closed"
            return
    if any(insert.target == block.get("name") for insert in inserts):
        block["value_set"] = "open"
