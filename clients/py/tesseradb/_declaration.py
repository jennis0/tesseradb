"""The declaration: blocks, the typed verbs' compilation into them, and the TOML.

Each `declare_*` verb builds one block of the declaration and appends it here.
The SDK writes the TOML and `tessera check` reads that file, so the mapping from verb to block is
checked by the binary rather than mirrored in Python.

What the TOML always says: every source name on every block, the allocation view, the value
set on every layer, and the disclosure controls on every layer and vocabulary, whether the user
said them or a default did. A reader of `schema.toml` sees the whole declaration without knowing
the SDK's defaults.
"""

from __future__ import annotations

import datetime
from typing import Any, Iterable, Sequence

from ._inserts import ARTIFACT_FIELDS, MEMBER_FIELDS, VOCABULARY_FIELDS
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
#: file, so the block names no source.
FILLED = "__filled__"

#: `value_set` as the caller chose it, kept beside the block and never written: where the caller
#: chose none, the layer's inserts decide it when the document is written.
CHOSEN_VALUE_SET = "__value_set__"

#: What a layer's engine derives per viewer when the caller names nothing, and what a spatial layer
#: derives instead: an artifact has one drawn geometry, so a hull beside a membership shape is
#: refused at the declaration.
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
            # A view of a group is addressed `<group>:<key>` and a plain view by its own name, so
            # one name held by both would make a request mean two things.
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
        """The first declared view unless another says `anchor=True`.

        Written into the TOML in either case, so a rebuild that reorders the blocks cannot re-key
        the corpus. A declaration carrying groups alone names none the SDK could write, a
        group's views being the distinct values of its roster, and the build chooses.
        """
        anchored = [b["name"] for b in self.blocks["view"] if b.get(ANCHOR)]
        if anchored:
            return anchored[0]
        names = self.view_names()
        return names[0] if names else None

    def document(self, sources: dict[str, str]) -> dict[str, Any]:
        """The declaration as TOML's own shape.

        **`[defaults].source` is never written.** Every block names the source its own inserts
        gave it, which is what lets a reader of `schema.toml` see the whole declaration without
        knowing the SDK's defaults; a block with no insert names none and is declared and empty,
        which the declaration allows.
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
        # its own, which draws the layer on every view of it, present and future.
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
            # route rather than read from a file.
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
    """One `[[view]]`: a frame, a projection and a gate, and no data.

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
    """One `[[view_group]]`: a set of views sharing every setting, differing by a key.

    The group's views and their metadata come from `insert(group, roster=table, key=, …)`, and
    its rows from `insert(group, table, id=, x=, y=, access=, view=)` with `view=` naming the
    column that says which view each row belongs to. `members` names another group whose views
    this group shares, and such a group declares no roster and no metadata of its own.
    """
    projection = projection or "none"
    declared = dict(metadata) if metadata else {}
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


def metadata_names(block: dict) -> tuple[str, ...]:
    """The metadata names a group declared, which its roster insert names one column each of."""
    return tuple(dict(block.get("metadata") or {}))


def roster_body(record: dict, names: Sequence[str]) -> dict:
    """One roster row as `PUT /control/views/{group}/{key}` takes it.

    Every metadata name the group declared is here and typed against it. A `timestamp_us`
    travels as microseconds since the epoch, JSON carrying no date type.
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
    # side. A frame is index configuration and does not change for the life of the
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
    """One `[[vocabulary]]`, with no data of its own.

    A closed set gives `values` inline or takes an `insert(name, table, key=, title=, code=)`; an
    open one minted from the data needs neither and may take an insert for titles.
    """
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
    """One `[[attribute]]`: a type and its two flags, and nothing else.

    It is filled by an `insert(name, table, id=, value=)`, or by name from a frame inserted into
    the allocation view.
    """
    group = _scope(scope)
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
    """One `[[layer]]`, with no data of its own.

    Its artifacts and its memberships come from its inserts: a key column
    (`insert(layer, table, id=, key=)`), or an artifacts table and a members table. `artifacts=`
    is the one exception and is a declaration rather than data: an authored roster written in
    the declaration itself, as its inline `artifacts` array.
    """
    membership_value, how = _membership(name, membership)
    group = _scope(scope)
    shape_block = _shape(name, shape)
    if how == "spatial":
        computed = SPATIAL_DERIVED if computed is DERIVED else computed
    rows = _artifact_rows(name, artifacts)

    block: dict[str, Any] = {"name": name}
    if title is not None:
        block["title"] = title
    if group is not None:
        block["scope"] = Inline({"group": group})
    if views is not None:
        block["views"] = list(views)
    else:
        # A scoped layer's views may name only its own group and the groups sharing its views, so
        # "every view" is that group and nothing else.
        block["views"] = [group] if group is not None else VIEWS_ALL
    block["membership"] = membership_value
    if how == "spatial" or default_space != "view":
        block["default_space"] = default_space
    block["hierarchy"] = Inline({"kind": kind, "prune_children": prune_children})
    if withdraw_on_member_deletion:
        block["withdraw_on_member_deletion"] = True
    # Written whenever the SDK chose it, since under `open` a mistyped key is a permanent artifact.
    # A layer whose artifacts arrive in a table is closed, and one with a key column alone is open;
    # which it is, is settled when the document is written, the inserts being made after.
    block["value_set"] = value_set or _value_set(how, rows)
    if layout is not None:
        block["layout"] = layout
    block["visibility"] = visibility
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    if depends_on:
        block["depends_on"] = list(depends_on)
    # A predicate layer's artifacts are the column's distinct values and carry no content, so the
    # derived set the caller did not ask for is not written onto one.
    if how != "attribute" or supplied or computed is not DERIVED:
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


def _scope(scope: Any) -> str | None:
    """`entity`, or the view group whose views a scoped block keeps one value or artifact set per.

    An attribute and a layer spell it the same way, `{"group": name}` or the name itself.
    """
    if scope == "entity" or scope is None:
        return None
    return scope.get("group") if isinstance(scope, dict) else scope


def _shape(name: str, shape: Any) -> dict | None:
    """The `[layer.shape]` block: `shape="polygon"`, or `{"kind": "polygon"}` spelled out."""
    if shape is None:
        return None
    if isinstance(shape, dict) and set(shape) != {"kind"}:
        raise Refusal(
            f"layer {name!r}: a shape declares its kind and nothing else. It names "
            f"{sorted(shape)!r}"
        )
    return {"kind": shape["kind"] if isinstance(shape, dict) else shape}


def _value_set(how: str, rows: Any) -> str:
    """`closed` where the layer's artifacts are a roster, `open` where they are minted."""
    # A predicate layer's artifacts are the column's values, which the vocabulary already bounds.
    if how == "attribute":
        return "closed"
    return "closed" if rows is not None else "open"


#: The artifact table's own columns, which an inline row spells canonically.
ARTIFACT_KEYS = (
    "key", "level", "members", "excluding", "bbox", "circle", "ellipse", "wkt", "space",
    "contents", "parent", "attached_layer", "attached_level", "attached_key",
)


def _artifact_rows(layer: str, artifacts: Any) -> list[dict] | None:
    """Inline `artifacts=`: a list of dicts, or a frame carrying the artifact table's columns."""
    if artifacts is None:
        return None
    if not isinstance(artifacts, (list, tuple)):
        artifacts = rows_of(artifacts)
    rows = [_artifact_row(layer, row) for row in artifacts]
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
    return [_stated(
        {name: values[i] for name, values in columns.items() if values[i] is not None},
        "access" in columns,
    ) for i in range(table.num_rows)]


def _stated(row: dict, named_access: bool) -> dict:
    """`row`, with a null `access` read as an empty list: a null states that the artifact has no
    label of its own, where a missing `access` states nothing."""
    if named_access and row.get("access") is None:
        row["access"] = []
    return row


ATTACHMENT_KEYS = ("layer", "key", "level")


def _artifact_row(layer: str, row: Any) -> dict:
    row = dict(row)
    row = _stated(
        {key: value for key, value in row.items() if value is not None}, "access" in row
    )
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
    # The declaration's own key order, with anything else after it, so a key the artifact table
    # does not carry reaches the check rather than being dropped here.
    ordered = {name: row[name] for name in ARTIFACT_KEYS if name in row}
    ordered.update({name: value for name, value in row.items() if name not in ordered})
    return ordered


def labels_block(
    name: str,
    content_requires: str = "inherited",
    type: str = "text",
    require_member_visibility: Any = "none",
    artifact_visibility: Any = "inherited",
    title: str | None = None,
) -> dict:
    """The `[layer.labels]` block: a label set over the clustering it hangs from.

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
    if isinstance(artifact_visibility, dict) and "field" in artifact_visibility:
        raise Refusal(
            f"labels {name!r}: a label set's text carries no access labels of its own, so it "
            f"has no column to read them from. Give artifact_visibility the default label alone"
        )
    block["artifact_visibility"] = _artifact_visibility(artifact_visibility)
    block["require_member_visibility"] = _requirement(require_member_visibility)
    block["content"] = {"require_member_visibility": content_requires}
    return block


def _artifact_visibility(value: Any) -> Any:
    """A label, which is the default, or `{"field": column, "default": label}`, where the field
    names the column each artifact's own labels are read from."""
    if isinstance(value, dict):
        return Inline(value)
    return Inline({"default": value})


def carry_labels(layer: str, block: dict, column: str) -> None:
    """Write `column` as the field of the layer's `artifact_visibility`, refusing a different
    column where one is already written.

    `block` is anything holding an `artifact_visibility`: the TOML block the build reads, the
    body a new layer is sent with, the SDK's own block once a commit sending the column is
    accepted, or a copy made only to check an insert.
    """
    visibility = dict(block.get("artifact_visibility") or {})
    held = visibility.get("field")
    if held is not None and held != column:
        raise Refusal(
            f"insert into layer {layer!r}: access={column!r}, and this layer reads its labels "
            f"from column {held!r}. Name that column with access={held!r}"
        )
    visibility["field"] = column
    block["artifact_visibility"] = Inline(visibility)


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


#: What the declaration's own defaults call the identity column in each place one is read: a
#: view's `fields.entity_id` and an attribute's `entity_id_field`, and a member row's
#: `fields.entity`.
CANONICAL_ENTITY_ID = "entity_id"
CANONICAL_MEMBER_ENTITY = "entity"


def bind(document: dict, inserts: Sequence[Any]) -> None:
    """Write onto every block the source and the column names its inserts gave it.

    The SDK rewrites no file, so a column keeps whatever name it has in the user's own table and
    the declaration is what says where each one is.
    """
    for block in document.get("view_group", []):
        _bind_group(
            block,
            [one for one in inserts if one.kind == "view_group" and one.target == block["name"]],
        )
    for insert in inserts:
        if insert.kind == "view_group":
            continue
        block = _block_for(document, insert)
        if block is None:
            continue
        getattr(_Bind, f"{insert.kind}_{insert.role}")(block, insert)
        for name, column in insert.named_attributes.items():
            _attribute_of_a_view(document, name, column, insert)


def bind_value_sets(document: dict, inserts: Sequence[Any]) -> None:
    """A layer's value set, where the caller chose none: its inserts decide it."""
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
    """Add to a block's `fields`, which the SDK writes as an inline table.

    One entry per column the call renamed. A column carrying the name the build reads it under is
    not here: the map locates a block's columns in the file its source names, and a group whose
    views each have their own file names no source for one to be read out of.
    """
    named = {key: value for key, value in named.items() if value is not None and value != key}
    if not named:
        return
    block["fields"] = Inline({**dict(block.get("fields") or {}), **named})


def _bind_group(block: dict, inserts: Sequence[Any]) -> None:
    """What a group's inserts write onto its block: one of the two roster forms.

    A group whose views each have their own file inserts one table per view, naming the view with
    `view_key=`, and the group's roster is those records: one `[[view_group.view]]` each, with
    the file it was given and the metadata the roster insert carries for that key. A group with
    one file for every view names the column that says which with `view=`, and its roster is the
    table itself, `[view_group.views]` beside the group's own source.
    """
    rows = [one for one in inserts if one.role == "rows"]
    roster = next((one for one in inserts if one.role == "roster"), None)
    keyed = [one for one in rows if one.view_key is not None]
    if keyed:
        metadata = _roster_rows(roster)
        block["view"] = [
            {
                "key": insert.view_key,
                "source": insert.source,
                **metadata.get(insert.view_key, {}),
            }
            for insert in keyed
        ]
        for insert in keyed:
            _Bind._points(block, insert)
        return
    for insert in rows:
        _Bind.view_group_rows(block, insert)
    if roster is not None:
        _Bind.view_group_roster(block, roster)


def _roster_rows(roster: Any) -> dict:
    """A roster table's metadata values by key, for the records an inline roster writes."""
    if roster is None:
        return {}
    table = roster.table()
    keys = table[roster.columns["key"]].to_pylist()
    values: dict[str, dict] = {}
    for at, key in enumerate(keys):
        record = {}
        if roster.columns.get("visibility"):
            record["visibility"] = table[roster.columns["visibility"]][at].as_py()
        for name, column in roster.metadata_columns.items():
            record[name] = table[column][at].as_py()
        values[str(key)] = record
    return values


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
        if insert.columns["value"] != block["name"]:
            block["field"] = insert.columns["value"]
        if insert.columns["id"] != CANONICAL_ENTITY_ID:
            block["entity_id_field"] = insert.columns["id"]
        if insert.columns.get("view"):
            _fields(block, {"view": insert.columns["view"]})

    @staticmethod
    def layer_key(block: dict, insert: Any) -> None:
        """A key column: `[layer.members]` over the points, the column as `key`."""
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
        if insert.columns.get("access"):
            carry_labels(insert.target, block, insert.columns["access"])

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
        # artifacts table and its members table alike.
        if insert.columns.get("view"):
            _fields(block, {"view": insert.columns["view"]})

    labels_text = layer_artifacts
    labels_members = layer_members

    @staticmethod
    def vocabulary_values(block: dict, insert: Any) -> None:
        block["source"] = insert.source
        _fields(block, _renamed(insert, VOCABULARY_FIELDS))


def _renamed(insert: Any, canonical: Sequence[str]) -> dict:
    """The `fields` entries an insert's names need: one per column it renamed.

    The names are `_inserts`' own tables, which are the build's field set, so a column the build
    reads under its own name alone never reaches a `fields` map.
    """
    return {
        name: insert.columns[name]
        for name in canonical
        if insert.columns.get(name) and insert.columns[name] != name
    }


def _attribute_of_a_view(document: dict, name: str, column: str, insert: Any) -> None:
    """An attribute filled from the allocation view's own frame, by name or by `columns=`."""
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
    """A layer whose artifacts arrive in a table is closed; one with a key column is open."""
    if block.pop(CHOSEN_VALUE_SET, False) or block.get("artifacts"):
        return
    for insert in inserts:
        if insert.target == block.get("name") and insert.role == "artifacts":
            block["value_set"] = "closed"
            return
    if any(insert.target == block.get("name") for insert in inserts):
        block["value_set"] = "open"
