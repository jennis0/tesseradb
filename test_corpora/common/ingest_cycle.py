"""The ingest cycle — decision 0091's test, run as a measurement rather than as an assertion.

A rung is built from *all* of its rows. This driver holds a seeded, uniform fraction *f* of the
entities back, builds the complement, serves it, and puts the hold-out through `/control/ingest`
— then flushes, folds, and asks whether the two deployments give the same masked counts. That is
[decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)'s claim
stated as a number instead of a principle: *build is ingest into an empty database*, so a
deployment assembled either way must answer identically.

Which route a layer's membership takes
--------------------------------------

**The base bundle carries the points, the declarations, and the member table of any layer whose
membership the build mints from a column.** The rung's `corpus.toml` is copied with every
`[[layer]]`'s `source` removed, so each layer is declared — kind, levels, visibility rules, content
kinds — and the route its membership takes follows from what it declares:

* **A layer with supplied content** (rung 3's MeSH descriptors, every `clusters/kmeans`) is
  declared and empty at the base: `[layer.members]` is removed with the roster. Its artifacts,
  their memberships and their supplied content are published through
  `PUT /control/layers/{name}/artifacts` **after every point they depend on has been ingested**
  (owner ruling, 2026-09-03). A key naming no artifact yet is minted, and
  `LayerRegistry::resolve_or_mint` refuses to mint on a layer that declares supplied content — an
  artifact served without content its layer declared cannot be told apart from one whose content
  was withheld — so a membership column at either entry point would always arrive first and
  always be refused. Membership arrives with the artifact that holds it, and the driver drops such
  a layer's column from the base points file for the same reason.
* **A layer with no supplied content and no roster** (rung 5's `taxonomy/tree`: an open value set
  with computed content, minted from a list column) keeps `[layer.members]` at the base build,
  over the base's rows only, and its hold-out rows carry the same list on the wire as the ingest
  batch's column named for the layer — one entry per declared level, an unknown key minting the
  artifact that carries its name and the computed content its points give it
  ([decision 0128](../../docs/decisions/0128-a-layer-with-no-supplied-content-travels-as-a-column-at-ingest.md);
  contracts §3.4). Both entry points read that column by one rule (decision 0091), and this is
  where the ingest cycle exercises the mint-from-column path at scale. The member table is read in
  lockstep with the points file, both ascending by entity and a row group at a time, so it is
  never held whole.
* **An attribute-membership layer** (`publishers/source`) carries nothing: its membership is
  evaluated against the indexed column every batch already sends.

An artifact cannot depend on a point that does not exist yet, and that ordering is the only
constraint: it holds at every fraction, so at *f* = 10% the base is 90% of the points and none of
the published artifacts.

⊘ **What this drops, deliberately.** An earlier driver built the base *with* the rung's artifact
roster. That put the layers on the build side of the split and made the *f* = 100% cell impossible
for a layer whose content requires every member visible: an artifact with no members names an empty
generating set, which is satisfied by everyone and is refused at both entry points. A published
artifact names its generating set, so the case does not arise.

What it measures, in order
--------------------------

1. **The split and the base build** — `tessera build --stage-timings-json` over the complement's
   points and the declaration-only `corpus.toml`, so the base's per-stage record is on the same
   schema as the whole-corpus build's.
2. **Online ingest** — Arrow IPC batches of 10,000 rows at *C* concurrent callers, `items/s`
   acked, ack p50/p99, and every refusal counted by status (429 backpressure, 409 duplicate or
   batch-id conflict, 422 bounds or contract). The batches carry the points, their labels as a
   list, and the member list of any layer on the column route — see above.
3. **Publication** — every layer's whole roster, in batches under a byte cap, with each artifact's
   whole member set (base and hold-out alike, by external addressing), its ranked content with its
   generating set, and its `parent` list. Its own figure: artifacts/s and members/s.
4. **Flush** — the wall of `POST /control/flush`, and *time to visibility*: when a zoom-0 viewport
   under the 100% principal reaches the expected count. Those are two different numbers and the
   second is the one a viewer experiences.
5. **The fold** — `POST /control/compact`, its wall and its RSS, both read from
   `/control/status`'s own `compaction` block rather than timed from outside: the route answers
   202 immediately, so an outside timer would measure the request and not the fold.
6. **Equivalence** — the ladder's masked counts on the folded deployment against the all-in build,
   at zoom 0, on a set of boxes, and **per layer**: artifact count and summed masked count off the
   kind-5 artifact frame, per principal. Exact zero difference, or a listed one.
7. **The write cycle** — deletes, suppressions, re-ingests, another fold and the census again.

⊘ **A hold-out is not a random sample of the map.** Entity ids are assigned in signature-sorted
order at a build and above the high-water at ingest (0091's own stated internal difference), so
the ingested rows land in a different place in entity space than the build would have put them.
That is *expected* and is not what the equivalence test checks: it checks the masked counts a
principal is served, which is what a client can observe.
"""

from __future__ import annotations

import argparse
try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
import base64
import concurrent.futures
import json
import shutil
import subprocess
import sys
import time
import urllib.parse
import uuid
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
import requests
from pyarrow import ipc

from . import serve_battery
from .deployment import Deployment

#: Write-path §2's per-batch row cap. The driver sends exactly this, so a run also exercises the
#: cap's own boundary rather than sitting comfortably under it.
BATCH_ROWS = 10_000


# ---------------------------------------------------------------------------------------------
# The split
# ---------------------------------------------------------------------------------------------


def split_entities(points: Path, fraction: float, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """`(base entity ids, hold-out entity ids)` — a seeded uniform hold-out of `fraction`.

    Uniform over **entities**, not over rows of the file, which for a one-row-per-entity points
    file is the same thing and is stated because it stops being so the moment a rung has views.
    """
    ids = pq.read_table(points, columns=["entity_id"]).column("entity_id").to_numpy()
    rng = np.random.default_rng(seed)
    order = rng.permutation(len(ids))
    cut = int(round(fraction * len(ids)))
    held = np.sort(ids[order[:cut]])
    base = np.sort(ids[order[cut:]])
    return base, held


def in_sorted(values: np.ndarray, sorted_ids: np.ndarray) -> np.ndarray:
    """Membership of `values` in `sorted_ids`, by binary search.

    `np.isin` builds an intermediate the size of both inputs; at rung 3 one of them is 1.66×10⁹
    rows and the other 3.6×10⁷, so the pass is done a row group at a time against a sorted array
    instead.
    """
    if len(sorted_ids) == 0:
        # The f = 1.0 cell: nothing is kept for the base at all. An empty `keep` is a real case
        # and the whole point of that cell — a build over a zero-row points file.
        return np.zeros(len(values), dtype=bool)
    idx = np.searchsorted(sorted_ids, values)
    idx[idx >= len(sorted_ids)] = 0
    return sorted_ids[idx] == values


def filter_parquet(
    source: Path, out: Path, column: str, keep: np.ndarray, drop: Sequence[str] = ()
) -> int:
    """Copy `source` to `out`, keeping rows whose `column` is in `keep`. **A row group at a time.**

    Streaming rather than `read_table().filter()` because rung 3's points file is 4 GB of parquet:
    read whole it is tens of gigabytes of Arrow, and the machine this runs on has 47. Through
    `read_row_group` rather than `iter_batches`, for the reason `HoldOut.batches` gives: the batch
    reader retains part of every row group it has yielded, and over rung 4's file that is tens of
    gigabytes by the time the split ends.

    **Written under the source's own compression**, not `ParquetWriter`'s default. Rung 4's points
    file is 52 GB of ZSTD carrying abstracts; rewritten as Snappy the 90% base copy passes 130 GB
    and fills the disk before the build starts. The codec is read off the first row group, so a
    rung that changes its own is followed rather than assumed.

    `drop` names columns to leave behind — see [`write_base_inputs`], which drops the membership
    columns a rung's points file may carry.
    """
    reader = pq.ParquetFile(source)
    codec = reader.metadata.row_group(0).column(0).compression.lower()
    if codec == "uncompressed":
        codec = "none"
    writer = None
    kept = 0
    try:
        wanted = [name for name in reader.schema_arrow.names if name not in set(drop)]
        for index in range(reader.metadata.num_row_groups):
            table = reader.read_row_group(index, columns=wanted)
            mask = pa.array(in_sorted(table.column(column).to_numpy(), keep))
            table = table.filter(mask)
            if writer is None:
                writer = pq.ParquetWriter(out, table.schema, compression=codec)
            if table.num_rows:
                writer.write_table(table)
                kept += table.num_rows
    finally:
        if writer is not None:
            writer.close()
    if writer is None:  # an empty source still needs a file with the right schema
        schema = pa.schema([f for f in reader.schema_arrow if f.name not in set(drop)])
        pq.write_table(schema.empty_table(), out, compression=codec)
    return kept


def ranks_file(rung: Path) -> Path:
    """The rung's principal ladder — `[{"term": …, "pairs": …}, …]`, richest term first.

    **Named after the rung's own axis, not after MedCPT's.** Rung 3 and MedCPT rank by branch and
    write `branch-ranks.json`; rung 4 ranks by licence and writes `licence-ranks.json`. A driver
    that opened the first by name refused to run against the second at all, after building its
    base.
    """
    named = rung / "branch-ranks.json"
    if named.exists():
        return named
    candidates = sorted(rung.glob("*-ranks.json"))
    if not candidates:
        raise FileNotFoundError(f"no <axis>-ranks.json in {rung}")
    return candidates[0]


def state_extent(corpus_toml: Path, bundle: Path, view: str | None = None) -> dict | None:
    """Rewrite the declaration's `extent = "auto"` as the all-in bundle's own frame.

    Two things follow from `auto`, and both are properties of the *frame* rather than of ingest:

    * **A zero-row build is refused.** `auto` fits a box around the data, and there is no box
      around no rows — the refusal says so and names this remedy. So the *f* = 100% cell cannot
      run at all under `auto`, and a deployment starting empty must state its frame.
    * **A complement build quantises onto a different grid.** The frame is fitted to the rows the
      build saw, so a base built from 90% of the corpus has a slightly smaller box, and the two
      deployments' cells do not line up. Every box-level count then differs at the margins for a
      reason that has nothing to do with the write path.

    Stating the all-in frame removes both. It is a change to the *declaration the measurement
    builds from*, never to the rung's committed one, and the run records that it was made.
    """
    manifest = json.loads((bundle / "CURRENT").read_text()) if (bundle / "CURRENT").is_file() else None
    version = manifest if isinstance(manifest, str) else None
    candidates = sorted(bundle.glob("v*/MANIFEST.json"))
    if not candidates:
        return None
    meta = json.loads(candidates[-1].read_text())
    views = meta["views"]
    chosen = next((v for v in views if v["id"] == view), views[0])
    q = chosen["quantisation"]
    text = corpus_toml.read_text()
    stated = (
        f'extent           = {{ x = [{q["x_min"]!r}, {q["x_max"]!r}], '
        f'y = [{q["y_min"]!r}, {q["y_max"]!r}] }}'
    )
    if 'extent           = "auto"' in text:
        text = text.replace('extent           = "auto"', stated)
    elif 'extent = "auto"' in text:
        text = text.replace('extent = "auto"', stated.replace("extent           =", "extent ="))
    else:
        return None
    corpus_toml.write_text(text)
    return {"view": chosen["id"], "quantisation": q, "from_version": version}


def base_declaration(text: str, keep_members: Sequence[str] = ()) -> tuple[str, list[dict]]:
    """The rung's `corpus.toml` as the base's: every layer stated, and only a column-route layer's
    member table kept.

    A `[[layer]]` block says two kinds of thing. Its declaration — kind, levels, views, the three
    disclosure controls, the content kinds — is what a running deployment holds and what
    `PUT /control/layers` takes. Its `source` and `[layer.members]` are *acquisition*: where the
    rows come from, which is build-only and is the half decision 0091 excludes from the rule that
    the two entry points say the same things (`configuration.md` §2). Removing that half leaves a
    layer that exists, is empty, and can be published into.

    `keep_members` names the layers whose `[layer.members]` stays: the ones with no supplied
    content and no roster, whose membership the build mints from the member table and whose
    hold-out rows carry the same list on the wire (decision 0128; the module doc). Every layer's
    `source` is removed regardless, a roster being the publication route's input.

    **A layer declared with no source is legal and needed no change** (`Config::layer_sources`
    carries `None` for it): the build reads no artifact table, plans no artifacts, and writes an
    empty level. What the build *does* refuse for an empty layer is nothing at all — the refusals
    an earlier driver met at *f* = 100% were about an empty **corpus**, not an empty layer, and
    they are gone with the roster.

    Only `source` at a `[[layer]]`'s own depth is removed. `[defaults]` and `[[view]]` carry a
    `source` too and are untouched: the points still come from a file.

    Returns the rewritten text and one record per layer, saying what was removed from it.
    """
    out: list[str] = []
    removed: list[dict] = []
    section: str | None = None
    layer: dict | None = None
    skipping = False
    for line in text.splitlines(keepends=True):
        head = line.strip()
        if head.startswith("[") and not head.startswith("[["):
            section = head
        elif head.startswith("[["):
            section = head
        if head.startswith("[[layer]]"):
            layer = {"layer": None, "removed": []}
            removed.append(layer)
        if head.startswith("[") and not head.startswith("[layer.members]"):
            skipping = False
        if head.startswith("[layer.members]"):
            if layer is not None and layer["layer"] in keep_members:
                layer["kept"] = "[layer.members]"
                out.append(line)
                continue
            # The block runs to the next table header at any indent, or to the end of the file.
            skipping = True
            if layer is not None:
                layer["removed"].append("[layer.members]")
            continue
        if skipping:
            continue
        if layer is not None and head.startswith("name") and layer["layer"] is None:
            layer["layer"] = head.split("=", 1)[1].strip().strip('"')
        if section == "[[layer]]" and head.startswith("source") and "=" in head:
            if layer is not None:
                layer["removed"].append(head)
            continue
        out.append(line)
    return "".join(out), removed


def declared_layers(rung: Path) -> list[dict]:
    """Every `[[layer]]` of the rung's declaration, with the files its acquisition names.

    One record per layer, in declaration order: `name`; `roster`, the path the layer's own `source`
    names, or None where the layer declares none (an open value set mints its artifacts from the
    member rows); `members`, the path `[layer.members] source` names, or None; `attribute`, the
    column an attribute-membership layer is drawn from, or None; `supplied`, whether the layer
    declares supplied content; and `route`, how its membership reaches the folded deployment
    (the module doc): `attribute` for a predicate layer, `column` for a layer with no supplied
    content and no roster whose member table the base build reads and whose hold-out rows carry
    on the wire, `publication` for everything else. A `source` is a key into `[sources]` or a file
    name, as `Config::layer_sources` reads it.

    Read off the declaration rather than listed in this file: a table of two layer names ran every
    rung's cell with at most those two, so rung 4's `topics/openalex` was never published and no
    record said so.
    """
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})

    def path_of(key: str | None) -> Path | None:
        return None if key is None else rung / named.get(key, key)

    out = []
    for layer in declared.get("layer", []):
        membership = layer.get("membership")
        attribute = membership.get("attribute") if isinstance(membership, dict) else None
        roster = path_of(layer.get("source"))
        members = path_of((layer.get("members") or {}).get("source"))
        supplied = bool((layer.get("content") or {}).get("supplied"))
        if attribute is not None:
            route = "attribute"
        elif roster is None and not supplied and members is not None:
            route = "column"
        else:
            route = "publication"
        out.append(
            {
                "name": layer["name"],
                "roster": roster,
                "members": members,
                "attribute": attribute,
                "supplied": supplied,
                "route": route,
                "value_set": layer.get("value_set"),
                "hierarchy": (layer.get("hierarchy") or {}).get("kind"),
            }
        )
    return out


def member_table_columns(schema: pa.Schema) -> tuple[str, str]:
    """`(entity column, key column)` of a member table, as the build reads one: `entity` (or
    `entity_id`) and `key`."""
    names = set(schema.names)
    entity = next((name for name in ("entity", "entity_id") if name in names), None)
    if entity is None or "key" not in names:
        raise ValueError(
            f"a member table needs an `entity` (or `entity_id`) column and a `key` column; this one "
            f"has {schema.names}"
        )
    return entity, "key"


def write_base_inputs(rung: Path, out: Path, base_ids: np.ndarray) -> dict:
    """The complement's inputs: **the points, the declaration, and a column-route layer's member
    table over the base's rows.** Nothing else.

    No artifact roster is copied, and no member table of a layer on the publication route (the
    module doc). Every such artifact, its membership and its supplied content is published on the
    wire after the points it depends on have been ingested, so a roster beside the build would be
    the same layer supplied twice — once as a build input and once as a publication — and the
    level's keys would collide on the second.

    **A publication-route layer's membership column is dropped from the points file with it.** A
    rung may name a point's artifacts in a column of its own rows (decision 0125; `mesh.py` writes
    one), which is the other way a membership arrives at a build — and it would arrive *before* the
    artifacts exist, on a layer declaring supplied content, which `LayerRegistry::resolve_or_mint`
    refuses outright: an artifact minted from a key alone could not be served, so the key is
    unmintable and the build stops. Membership travels with the artifact that holds it there.

    **A column-route layer's member table is filtered to the base's rows** and written beside the
    points under the name the declaration gives it, so the base build mints exactly the artifacts
    the base's rows name; the hold-out's rows name theirs on the wire (decision 0128).

    `corpus.toml` is rewritten by [`base_declaration`], which removes each layer's roster and
    keeps a column-route layer's `[layer.members]`.
    """
    out.mkdir(parents=True, exist_ok=True)
    layers = declared_layers(rung)
    published = [layer["name"] for layer in layers if layer["route"] == "publication"]
    on_column = [layer for layer in layers if layer["route"] == "column"]
    kept = {
        "points": filter_parquet(
            rung / "points.parquet", out / "points.parquet", "entity_id", base_ids, drop=published
        ),
        "dropped_membership_columns": [
            name
            for name in pq.ParquetFile(rung / "points.parquet").schema_arrow.names
            if name in set(published)
        ],
        "member_tables": {},
    }
    for layer in on_column:
        members = layer["members"]
        entity, _ = member_table_columns(pq.ParquetFile(members).schema_arrow)
        kept["member_tables"][layer["name"]] = {
            "file": members.name,
            "rows": filter_parquet(members, out / members.name, entity, base_ids),
        }
    # **Every file the declaration still names**, read off the declaration rather than listed here.
    # A vocabulary is copied whole — it is a value set, not rows, and a base built from half the
    # corpus declares the same closed set. Any *other* view's points file is filtered by entity id
    # exactly as the anchor's is: a rung may carry several row spaces over one entity space
    # (rung 5's `bioclip` and `geo`), and a declaration naming a file the base directory does not
    # hold refuses the build with `No such file or directory`.
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})
    anchor = declared.get("defaults", {}).get("source", "points")

    def path_of(key: str) -> Path:
        return rung / named.get(key, key)

    for vocabulary in declared.get("vocabulary", []):
        source = vocabulary.get("source")
        if source is None:
            continue
        got = path_of(source)
        if got.exists():
            shutil.copy2(got, out / got.name)
            kept.setdefault("vocabularies", []).append(got.name)

    for view in declared.get("view", []):
        source = view.get("source", anchor)
        if source == anchor:
            continue
        got = path_of(source)
        if got.exists():
            kept.setdefault("views", {})[got.name] = filter_parquet(
                got, out / got.name, "entity_id", base_ids, drop=published
            )

    for name in ("branch.parquet", ".env"):
        source = rung / name
        if source.exists() and not (out / name).exists():
            shutil.copy2(source, out / name)
    declaration, removed = base_declaration(
        (rung / "corpus.toml").read_text(), keep_members=[layer["name"] for layer in on_column]
    )
    (out / "corpus.toml").write_text(declaration)
    kept["declaration_only"] = removed
    (out / "tessera.toml").write_text((rung / "tessera.toml").read_text())
    return kept


# ---------------------------------------------------------------------------------------------
# The hold-out, as ingest batches
# ---------------------------------------------------------------------------------------------


def wire_columns(rung: Path) -> tuple[str | None, list[str]]:
    """`(access column, attribute columns)` for the hold-out's batches, **read off the rung's own
    declaration** rather than listed here.

    The batch a rung's hold-out is sent as is not a property of this driver: hard-coding MedCPT's
    four made the driver refuse rung 4 after building its 92M-row base, on a `KeyError` for a
    column that rung does not have.

    The access column is the first `point_visibility.field` any view declares — a rung compartments
    on one column, and the wire takes one list of labels a row; the rung's column may be a list per
    row (rung 3 and MedCPT) or one string (rung 4's licence, rung 5's publisher), and
    [`encode_batch`] sends both as the list (decision 0129). The attribute
    columns are every `[[attribute]]` the declaration names that this rung's points file actually
    holds, which is what makes the ingested rows carry the same columns the built ones do; a rung
    whose points file does not hold one of them is a rung whose build would have refused too.

    ⊘ **Points of the anchor view alone.** A rung with several row spaces over one entity space
    (rung 5's `bioclip` and `geo`) ingests its hold-out into the anchor view; the other views'
    rows for those entities do not travel, so an equivalence census over a second view is not
    comparable and the run says so.
    """
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    access = None
    for view in declared.get("view", []):
        field = (view.get("point_visibility") or {}).get("field")
        if field:
            access = field
            break
    held = set(pq.ParquetFile(rung / "points.parquet").schema_arrow.names)
    attributes = [a["name"] for a in declared.get("attribute", []) if a["name"] in held]
    return access, attributes


def encode_batch(
    table: pa.Table, access: str | None, attributes: list[str], columns: Sequence[str] = ()
) -> bytes:
    """One Arrow IPC stream for a slice of the hold-out.

    `access` is the wire's **list of labels**, one element per label, each taken verbatim
    (contracts §3.4, decision 0129): a rung's list column travels as itself, a scalar compartment
    column as one-element lists, and a null — a null scalar or a null list — as the empty list,
    which the server would otherwise refuse for the whole batch. **The empty list is a row with no
    label, and the view's declaration decides it at both entry points** (decision 0133): where the
    view declares a `point_visibility.default` the server gives the row that label, as the build
    gives it to a null or empty value; where it declares none the server refuses the batch naming
    the count, as the build refuses the corpus. This driver applies no default of its own, so an
    ingest cycle takes the same declaration the build took. Nothing here joins or splits a label, so a compartment
    key containing a comma is one term on both sides of the split. `external_id` is the **source entity id, eight bytes little-endian**, the same form the
    build mints under `--mint-external-ids` (see [`external_ids`]). That is what makes an ingested
    row addressable on `/control/changes` afterwards, and what an artifact's `members` names it by
    on the same footing as a base row. Every declared attribute travels beside it, by the name the
    declaration gives it — see [`wire_columns`].

    `columns` names the column-route layers (the module doc): each is a column of `table` already
    named for the layer, carrying the row's member list as the member table spells it — one entry
    per declared level, null where the row is in no artifact at that level — and it travels as
    itself. A publication-route layer has no column here, because the column would name artifacts
    that do not exist yet and a layer declaring supplied content refuses to mint them.
    """
    entities = table.column("entity_id").to_pylist()
    arrays = [
        table.column("x").cast(pa.float64()).combine_chunks(),
        table.column("y").cast(pa.float64()).combine_chunks(),
    ]
    names = ["x", "y"]
    if access is not None:
        column = table.column(access).combine_chunks()
        if isinstance(column, pa.ChunkedArray):
            column = pa.concat_arrays(column.chunks) if column.num_chunks else pa.array([], column.type)
        if pa.types.is_list(column.type) or pa.types.is_large_list(column.type):
            column = column.cast(pa.list_(pa.string()))
            lengths = pc.fill_null(pc.list_value_length(column), 0).to_numpy(zero_copy_only=False)
            offsets = np.concatenate([[0], np.cumsum(lengths)]).astype(np.int32)
            column = pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), pc.list_flatten(column))
        else:
            values = column.cast(pa.string())
            lengths = pc.cast(pc.is_valid(values), pa.int32()).to_numpy(zero_copy_only=False)
            offsets = np.concatenate([[0], np.cumsum(lengths)]).astype(np.int32)
            column = pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), values.drop_null())
        arrays.append(column)
        names.append("access")
    arrays.append(pa.array([int(e).to_bytes(8, "little") for e in entities], pa.binary()))
    names.append("external_id")
    # **Every declared attribute, the access column included.** The scalar tail is read back by
    # position, so an omission misaligns it exactly as a spurious column does — and a rung whose
    # compartment is also a rendered attribute (rung 5's `publisher`) sends it twice on purpose:
    # once as `access`, the plugin's own descriptor list, and once as the column itself.
    for name in attributes:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    for name in columns:
        arrays.append(table.column(name).combine_chunks())
        names.append(name)
    batch = pa.RecordBatch.from_arrays(
        [pa.array(a) if not isinstance(a, pa.Array) else a for a in arrays], names=names
    )
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class MemberStream:
    """A column-route layer's member table, read in lockstep with the points file.

    Both files ascend by entity — the rung's preparation writes them so, and this checks it a row
    group at a time rather than trusting it — so the member rows a points batch needs are the ones
    up to its last entity. One row group is decoded at a time, filtered to the hold-out, and what is
    live is the rows past the last batch's entities; at rung 5's 2.3×10⁸ member rows nothing is held
    whole. A hold-out entity the table does not name is in no artifact and gets a null cell, which
    is what the build reads for a point the member table leaves out; the count is recorded.
    """

    def __init__(self, name: str, path: Path, held: np.ndarray):
        self.name = name
        self.path = path
        self.reader = pq.ParquetFile(path)
        self.entity_column, self.key_column = member_table_columns(self.reader.schema_arrow)
        self.key_type = self.reader.schema_arrow.field(self.key_column).type
        self.held = held
        self.group = 0
        self.pending: list[pa.Table] = []
        self.last_entity = -1
        self.stats = {"file": path.name, "rows_with_keys": 0, "rows_without": 0}

    def _pull(self) -> bool:
        """Decode the next row group into `pending`, filtered to the hold-out; False when done."""
        if self.group >= self.reader.metadata.num_row_groups:
            return False
        table = self.reader.read_row_group(self.group, columns=[self.entity_column, self.key_column])
        self.group += 1
        entities = table.column(self.entity_column).to_numpy()
        if len(entities):
            if int(entities[0]) <= self.last_entity or np.any(np.diff(entities.astype(np.int64)) <= 0):
                raise ValueError(
                    f"{self.path.name}: `{self.entity_column}` is not strictly ascending across row "
                    f"group {self.group - 1}; the driver reads a member table in lockstep with the "
                    f"points file and needs both in entity order"
                )
            self.last_entity = int(entities[-1])
        table = table.filter(pa.array(in_sorted(entities, self.held)))
        if table.num_rows:
            self.pending.append(table)
        return True

    def keys_for(self, entities: np.ndarray) -> pa.Array:
        """The member list of each of `entities` (ascending), null where the table names none."""
        if len(entities) == 0:
            return pa.array([], self.key_type)
        top = int(entities[-1])
        while self.last_entity < top and self._pull():
            pass
        have = pa.concat_tables(self.pending) if self.pending else None
        if have is None or have.num_rows == 0:
            self.stats["rows_without"] += len(entities)
            return pa.nulls(len(entities), self.key_type)
        wanted = pa.array(entities).cast(have.column(self.entity_column).type)
        index = pc.index_in(wanted, value_set=have.column(self.entity_column).combine_chunks())
        keys = pc.take(have.column(self.key_column).combine_chunks(), index)
        missing = index.null_count
        self.stats["rows_without"] += missing
        self.stats["rows_with_keys"] += len(entities) - missing
        rest = have.filter(pc.greater(have.column(self.entity_column), top))
        self.pending = [rest] if rest.num_rows else []
        return keys


class HoldOut:
    """The held-back rows, streamed out of the rung's own parquet as ingest batches.

    **Streamed, never materialised.** At *f* = 100% of rung 3 the hold-out is the whole corpus —
    4 GB of parquet, tens of gigabytes of Arrow — and holding it beside a running server on a
    47 GB box is the run failing for a reason that has nothing to do with what it measures. Only
    `head_rows` rows are kept, for the write cycle, which needs the same bytes twice.

    A column-route layer's member list rides each batch as the column named for the layer, joined
    from a [`MemberStream`] read in lockstep with the points; `member_stats` records, per layer, how
    many hold-out rows the table named and how many it did not.
    """

    def __init__(self, rung: Path, held: np.ndarray, head_rows: int = 0):
        self.rung = rung
        self.access, self.attributes = wire_columns(rung)
        self.held = np.sort(held)
        self.head_rows = head_rows
        self.head: pa.Table | None = None
        self.members = [
            MemberStream(layer["name"], layer["members"], self.held)
            for layer in declared_layers(rung)
            if layer["route"] == "column"
        ]
        self.columns = [stream.name for stream in self.members]
        self.member_stats = {stream.name: stream.stats for stream in self.members}
        self.last_entity = -1

    def batches(self, rows: int = BATCH_ROWS):
        """Yield `(first row index, body bytes, row count)` for the whole hold-out, in file order.

        **One row group at a time through `read_row_group`, not `iter_batches`.** Measured on
        rung 4's 52 GB points file (`probes/2026-09-05-holdout-memory/`): pyarrow 25's
        `iter_batches` reader keeps about 150 MB of every row group it has yielded alive in
        Arrow's pool for the life of the iterator, whatever the caller drops and whichever
        allocator backs the pool (`mimalloc` and `system` measured), so the driver reached 38 GB by
        900 batches and the cell stalled. `read_row_group` holds one decoded row group at a time
        and the same file streams whole with the driver under 3 GB.
        """
        reader = pq.ParquetFile(self.rung / "points.parquet")
        pending: list[pa.Table] = []
        pending_rows = 0
        emitted = 0
        head: list[pa.Table] = []
        head_rows = 0
        for index in range(reader.metadata.num_row_groups):
            group = reader.read_row_group(index)
            table = group.filter(pa.array(in_sorted(group.column("entity_id").to_numpy(), self.held)))
            del group
            if table.num_rows == 0:
                continue
            if self.members:
                entities = table.column("entity_id").to_numpy()
                if int(entities[0]) <= self.last_entity or np.any(np.diff(entities.astype(np.int64)) <= 0):
                    raise ValueError(
                        f"points.parquet: `entity_id` is not strictly ascending across row group "
                        f"{index}; a member table is read in lockstep with it and needs both in "
                        f"entity order"
                    )
                self.last_entity = int(entities[-1])
                for stream in self.members:
                    table = table.append_column(stream.name, stream.keys_for(entities))
            if head_rows < self.head_rows:
                take = min(self.head_rows - head_rows, table.num_rows)
                head.append(table.slice(0, take))
                head_rows += take
            pending.append(table)
            pending_rows += table.num_rows
            while pending_rows >= rows:
                whole = pa.concat_tables(pending)
                yield emitted, self.encode(whole.slice(0, rows)), rows
                emitted += rows
                rest = whole.slice(rows)
                pending = [rest] if rest.num_rows else []
                pending_rows = rest.num_rows
        if pending_rows:
            whole = pa.concat_tables(pending)
            yield emitted, self.encode(whole), pending_rows
            emitted += pending_rows
        self.total = emitted
        if head:
            self.head = pa.concat_tables(head)

    def encode(self, table: pa.Table) -> bytes:
        """[`encode_batch`] over a slice of this hold-out, member columns included."""
        return encode_batch(table, self.access, self.attributes, self.columns)


# ---------------------------------------------------------------------------------------------
# Publication — the artifacts, after their points
# ---------------------------------------------------------------------------------------------

#: The publication route's own body cap, `PUBLISH_MAX_BODY_BYTES` in `tessera-server/src/control.rs`,
#: and `PATCH /control/layers/{name}/artifacts`' too. `--publish-max-bytes` is clamped to it. An
#: artifact whose whole membership does not fit under the working cap is published with as many
#: members as fit and then **grown** by PATCH in slices under the cap (decision 0127); only an
#: artifact whose key, content and parents alone do not fit is declined, and recorded.
ROUTE_MAX_BODY_BYTES = 64 * 1024 * 1024

#: The base64 alphabet, indexed by sextet.
_B64 = np.frombuffer(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/", np.uint8)

#: A member table's row, as the partitioning pass writes it: the artifact's ordinal in publication
#: order, the row's rank (−1 is the membership, `k` is `contents[k]`'s generating set) and the
#: entity. 14 bytes; 1.66×10⁹ rows of rung 3's DAG membership are 23 GB of transient files.
MEMBER_RECORD = np.dtype([("idx", "<i4"), ("rank", "<i2"), ("entity", "<u8")])

EMPTY_ENTITIES = np.zeros(0, np.uint64)


def external_ids_b64(entities: np.ndarray) -> np.ndarray:
    """`(n, 12)` uint8: each entity id as eight little-endian bytes, base64.

    **The external id is the source entity id, little-endian, because that is the one address both
    halves of the split share.** A publication names its members by external id, and its members
    are base rows and ingested rows alike: the ingested half carries whatever `external_id` the
    batch supplied, and the built half carries whatever the build minted, which is
    `source_id.to_le_bytes()` under `--mint-external-ids` (`tessera-build`'s `ExternalIdRow`) and
    nothing at all without it. So the driver builds the base with that flag and sends the same
    eight bytes on ingest, and one member list then addresses both.

    Computed from the ids in NumPy rather than looked up in a table indexed by entity id: rung 5's
    table was 16 bytes for each of 2.3×10⁸ entities, 3.7 GB, filled by a Python loop over every
    id. Eight bytes are two full base64 groups and one group of two bytes, so the twelfth
    character is always `=`.

    ⊘ **This is why the PMID is no longer the external id.** An earlier driver sent the PMID, which
    is the natural caller identifier for that corpus and is still the `pmid` attribute, but the
    build cannot mint an external id from a column, so a published membership over base rows was
    unaddressable and the whole batch was refused, naming member 0 of artifact 0.
    """
    raw = np.ascontiguousarray(entities, dtype="<u8").view(np.uint8).reshape(-1, 8)
    out = np.empty((len(raw), 12), np.uint8)
    for group in range(2):
        b0, b1, b2 = raw[:, 3 * group], raw[:, 3 * group + 1], raw[:, 3 * group + 2]
        out[:, 4 * group] = _B64[b0 >> 2]
        out[:, 4 * group + 1] = _B64[((b0 & 3) << 4) | (b1 >> 4)]
        out[:, 4 * group + 2] = _B64[((b1 & 15) << 2) | (b2 >> 6)]
        out[:, 4 * group + 3] = _B64[b2 & 63]
    b6, b7 = raw[:, 6], raw[:, 7]
    out[:, 8] = _B64[b6 >> 2]
    out[:, 9] = _B64[((b6 & 3) << 4) | (b7 >> 4)]
    out[:, 10] = _B64[(b7 & 15) << 2]
    out[:, 11] = ord("=")
    return out


def json_list_bytes(count: int) -> int:
    """The length [`json_list`] produces for `count` ids: `["` + 12 chars + `"` and a comma each."""
    return 2 if count == 0 else 15 * count + 1


def json_list(entities: np.ndarray) -> bytes:
    """A JSON array of base64 external ids, as bytes, assembled in NumPy.

    One `(n, 15)` byte array — quote, twelve characters, quote, comma — then one copy. No Python
    string is made per member: `json.dumps` over 7.6×10⁵ of them was the largest single cost in
    a publication, and a list of 4×10⁶ Python `bytes` is a gigabyte of interpreter objects.
    """
    if not len(entities):
        return b"[]"
    cell = np.empty((len(entities), 15), np.uint8)
    cell[:, 0] = ord('"')
    cell[:, 1:13] = external_ids_b64(entities)
    cell[:, 13] = ord('"')
    cell[:, 14] = ord(",")
    return b"[" + cell.tobytes()[:-1] + b"]"


def in_parent_order(table: pa.Table) -> pa.Table:
    """A roster's rows reordered so that no artifact precedes a parent of its own.

    A publication resolves a parent key against the level as it stands **plus the artifacts earlier
    in the same batch** (`LayerRegistry::prepare_publish`), so a child published before its parent
    names nothing and refuses the batch. A roster is written in key order, which for a DAG of MeSH
    descriptors is alphabetical and unrelated to depth.

    By longest path to a root, which is the layer's own depth and is what a level-by-level
    publication would have used. A parent the roster does not hold is ignored here, as it is where
    the row is written: it is not an artifact of this layer.
    """
    if "parent" not in table.schema.names:
        return table
    keys = table.column("key").to_pylist()
    parents = table.column("parent").to_pylist()
    at = {key: i for i, key in enumerate(keys)}
    depth = [-1] * len(keys)

    for start in range(len(keys)):
        # Iterative rather than recursive: 3.0×10⁴ descriptors is shallow, but a rung's DAG is the
        # caller's data and a recursion limit is not the refusal anyone wants to read. `on_stack`
        # is the cycle guard — the service refuses a cycle at publication, and a driver that spun
        # for ever instead of saying so would look like a hung run.
        stack = [start]
        on_stack = set()
        while stack:
            i = stack[-1]
            if depth[i] >= 0:
                on_stack.discard(i)
                stack.pop()
                continue
            pending = [at[k] for k in (parents[i] or []) if k in at and depth[at[k]] < 0]
            if any(j in on_stack for j in pending):
                raise ValueError(f"the roster's parent edges cycle at {keys[i]!r}")
            if pending:
                on_stack.add(i)
                stack.extend(pending)
                continue
            depth[i] = 1 + max(
                (depth[at[k]] for k in (parents[i] or []) if k in at), default=-1
            )
            on_stack.discard(i)
            stack.pop()
    order = sorted(range(len(keys)), key=lambda i: depth[i])
    return table.take(pa.array(order, pa.int64()))


def key_order_is_parent_order(keys: Sequence[str], parents: Sequence[list | None]) -> bool:
    """Whether publishing the roster in key order would put every parent before its children.

    True for a layer with no edges. Where it holds, a member file sorted by key can be streamed
    and published in file order; where it does not, the file is partitioned into publication
    order first.
    """
    held = set(keys)
    return all(
        parent < key
        for key, own in zip(keys, parents)
        for parent in (own or [])
        if parent in held
    )


def key_ranges(path: Path) -> list[tuple[str, str]] | None:
    """`(min key, max key)` per row group from the file's own statistics; None if any lacks them."""
    reader = pq.ParquetFile(path)
    column = reader.schema_arrow.names.index("key")
    out = []
    for i in range(reader.metadata.num_row_groups):
        statistics = reader.metadata.row_group(i).column(column).statistics
        if statistics is None or not statistics.has_min_max:
            return None
        out.append((statistics.min, statistics.max))
    return out


def grouped_by_key(ranges: Sequence[tuple[str, str]]) -> bool:
    """Whether consecutive row groups' key ranges never overlap, so every key's rows are contiguous.

    Two adjacent row groups may share one key at their boundary; a row group whose maximum exceeds
    the next one's minimum means a key's rows can be anywhere in the file. Parquet orders string
    statistics bytewise, as Python compares `str`.
    """
    return all(a_max <= b_min for (_, a_max), (b_min, _) in zip(ranges, ranges[1:]))


def member_columns(table: pa.Table, value_set: pa.Array, path: Path) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """`(ordinal int32, rank int16, entity uint64)` for one read of a member table.

    The ordinal is the key's position in `value_set`, the roster in publication order. A `key`
    read as a dictionary is mapped through its dictionary, so a 10⁶-row row group costs one
    `index_in` over its distinct keys and one NumPy take; a row whose key the roster does not hold
    is a refusal naming it. `rank` null becomes −1.
    """
    keys = table.column("key")
    if keys.null_count:
        raise ValueError(f"{path.name}: {keys.null_count} member rows have a null key")
    parts = []
    for chunk in keys.chunks:
        if pa.types.is_dictionary(chunk.type):
            at = pc.fill_null(pc.index_in(chunk.dictionary, value_set=value_set), -1)
            part = at.to_numpy().astype(np.int32)[chunk.indices.to_numpy()]
            if (part < 0).any():
                unknown = chunk.dictionary.to_pylist()
                missing = [k for k, a in zip(unknown, at.to_pylist()) if a < 0][:3]
                raise ValueError(f"{path.name}: member rows name keys the roster does not: {missing}")
        else:
            at = pc.fill_null(pc.index_in(chunk, value_set=value_set), -1)
            part = at.to_numpy().astype(np.int32)
            if (part < 0).any():
                missing = pc.filter(chunk, pc.equal(at, -1)).to_pylist()[:3]
                raise ValueError(f"{path.name}: member rows name keys the roster does not: {missing}")
        parts.append(part)
    idx = parts[0] if len(parts) == 1 else np.concatenate(parts)
    ranks = table.column("rank")
    rank = pc.fill_null(ranks, 0).to_numpy().astype(np.int64)
    if rank.max(initial=0) >= np.iinfo(np.int16).max:
        raise ValueError(f"{path.name}: a content rank of {int(rank.max())} does not fit this pass")
    rank = rank.astype(np.int16)
    rank[pc.is_null(ranks).to_numpy(zero_copy_only=False)] = -1
    entity = table.column("entity").to_numpy().astype(np.uint64)
    return idx, rank, entity


def rank_groups(idx: np.ndarray, rank: np.ndarray, entity: np.ndarray):
    """Yield `(ordinal, {rank: entities})` for every ordinal present, in ordinal order.

    One `lexsort` over the rows, then the group boundaries are one `diff`.
    """
    if not len(idx):
        return
    order = np.lexsort((rank, idx))
    idx, rank, entity = idx[order], rank[order], entity[order]
    del order
    edges = np.flatnonzero((np.diff(idx) != 0) | (np.diff(rank) != 0)) + 1
    starts, ends = np.r_[0, edges], np.r_[edges, len(idx)]
    current, groups = int(idx[0]), {}
    for lo, hi in zip(starts, ends):
        ordinal = int(idx[lo])
        if ordinal != current:
            yield current, groups
            current, groups = ordinal, {}
        groups[int(rank[lo])] = entity[lo:hi]
    yield current, groups


class Publication:
    """One declared layer's roster, published in batches under a byte cap, as they are assembled.

    **Every artifact carries its whole member set**, base rows and ingested rows alike, addressed
    by external id, which is the address both halves share. The batch is the commit unit at the
    route, so the cap splits *between* artifacts. An artifact whose whole body would exceed
    `--publish-max-bytes` is published with its key, content, parents and as many members as fit
    under the cap, then **grown** through `PATCH /control/layers/{name}/artifacts` in slices of at
    most the cap, in the same parent-before-child order and after the batch that published it
    (decision 0127; contracts §3.4). `grown_artifacts`, `grow_requests` and `grown_members` record
    that path. Only an artifact whose key, content and parents alone do not fit is declined, and
    recorded with its key, member count and body bytes before its member list is ever built.

    **The member table is read in artifact order once, and never held whole.** A layer's member
    table can be 1.66×10⁹ rows (rung 3's DAG membership), and a driver that inverted it in memory
    to address it per artifact held ~25 GB of bodies for it and declined the layer instead. Two
    readers, chosen from the file's own row-group statistics:

    * **Streamed**, where consecutive row groups' key ranges do not overlap (a k-means member file,
      written one cluster after another) and key order is a parent-before-child order for this
      layer. Row groups are read one at a time; a key closes when the next row group's minimum is
      past it, so what is live is the row groups spanning one key boundary. A key that reappears
      after closing is a refusal, so the statistics are checked rather than trusted.
    * **Partitioned**, otherwise. One pass over `key` and `rank` counts each artifact's rows, and
      buckets are planned as ranges of the publication order under `--publish-bucket-rows` (an
      artifact over the budget has a bucket to itself). One pass writes every row as a 14-byte
      record into its bucket under `--work`; then one bucket at a time is read back, sorted, and
      published. Peak memory is one bucket plus one artifact's body, whatever the table's size.
      Buckets are ranges of the publication order, so a parent is never in a later bucket than
      its child. An artifact whose membership alone already exceeds the route's cap is declined
      at planning and its rows are not written.

    **The roster's `parent` list travels as the artifact's `parent`**, which is where a `dag`
    layer's edges are spelled and the only place they are (decision 0125); `edges_published`
    counts what landed, against `edges_declared`. Parents are published before their children
    ([`in_parent_order`]): a parent must already exist or sit earlier in the same batch, an
    ordering an edge has always carried (`annotation-representation.md` §5.0.4).

    **A declined artifact is not held, and a child's edge to it is dropped before sending.** The
    route refuses an artifact whose parent the layer does not hold, so an edge to a declined parent
    would refuse every descendant's batch and the census would list the whole tree as missing
    rather than the artifacts that were declined. The child is kept, its edge to the declined
    parent is dropped, and `edges_dropped_to_declined` counts them beside `edges_published`. A
    parent whose batch the route **refused** is a different case: its children's batches are
    refused too, each refusal is counted and logged, and the census then lists the subtree.
    """

    def __init__(self, roster: Path, members: Path | None, work: Path, max_bytes: int, bucket_rows: int):
        self.table = in_parent_order(pq.read_table(roster))
        self.rows = self.table.to_pylist()
        self.keys = [row["key"] for row in self.rows]
        self.held = set(self.keys)
        self.members_path = members
        self.work = work
        self.max_bytes = min(max_bytes, ROUTE_MAX_BODY_BYTES)
        self.bucket_rows = bucket_rows
        self.stats = {
            "artifacts": len(self.rows),
            "members": 0,
            "generating_set_entries": 0,
            "edges_declared": 0,
            "edges_in_roster_and_layer": 0,
            "artifacts_with_several_parents": 0,
            "read_path": None,
            "row_groups": None,
            "buckets": None,
            "count_s": None,
            "partition_s": None,
            "declined_artifacts": [],
            "edges_dropped_to_declined": 0,
            "grown_artifacts": 0,
            "grow_slices": 0,
            "grown_members_sent": 0,
        }
        self.declined_keys: set[str] = set()
        for row in self.rows:
            own = row.get("parent") or []
            in_layer = [key for key in own if key in self.held]
            self.stats["edges_declared"] += len(own)
            self.stats["edges_in_roster_and_layer"] += len(in_layer)
            self.stats["artifacts_with_several_parents"] += 1 if len(in_layer) > 1 else 0

    # -- the member table, one artifact at a time -----------------------------------------

    def members(self):
        """Yield `(roster index, {rank: entities})` for every artifact, parents before children."""
        if self.members_path is None:
            self.stats["read_path"] = "no member table"
            for i in range(len(self.rows)):
                yield i, {}
            return
        ranges = key_ranges(self.members_path)
        self.stats["row_groups"] = pq.ParquetFile(self.members_path).metadata.num_row_groups
        parents = [row.get("parent") for row in self.rows]
        if ranges is not None and grouped_by_key(ranges) and key_order_is_parent_order(self.keys, parents):
            self.stats["read_path"] = "streamed"
            yield from self._streamed(ranges)
        else:
            self.stats["read_path"] = "partitioned"
            yield from self._partitioned()

    def _streamed(self, ranges: Sequence[tuple[str, str]]):
        reader = pq.ParquetFile(self.members_path, read_dictionary=["key"])
        value_set = pa.array(self.keys, pa.string())
        by_key = sorted(range(len(self.keys)), key=self.keys.__getitem__)
        closed = np.zeros(len(self.keys), bool)
        window: list[tuple[np.ndarray, np.ndarray, np.ndarray]] = []
        next_to_close = 0
        for i in range(len(ranges)):
            group = reader.read_row_group(i, columns=["key", "rank", "entity"])
            idx, rank, entity = member_columns(group, value_set, self.members_path)
            del group
            if closed[idx].any():
                reopened = self.keys[int(idx[closed[idx]][0])]
                raise ValueError(
                    f"{self.members_path.name}: {reopened!r} has rows after the row group its "
                    f"statistics said it ended in; the file is not grouped by key"
                )
            window.append((idx, rank, entity))
            bound = ranges[i + 1][0] if i + 1 < len(ranges) else None
            closing = []
            while next_to_close < len(by_key) and (bound is None or self.keys[by_key[next_to_close]] < bound):
                closing.append(by_key[next_to_close])
                next_to_close += 1
            if not closing:
                continue
            closed[closing] = True
            idx, rank, entity = (np.concatenate(parts) for parts in zip(*window))
            done = closed[idx]
            groups = dict(rank_groups(idx[done], rank[done], entity[done]))
            keep = ~done
            window = [(idx[keep], rank[keep], entity[keep])] if keep.any() else []
            del idx, rank, entity, done, keep
            for ordinal in closing:
                yield ordinal, groups.pop(ordinal, {})

    def _partitioned(self):
        reader = pq.ParquetFile(self.members_path, read_dictionary=["key"])
        value_set = pa.array(self.keys, pa.string())
        count = len(self.keys)

        # One pass over key: each artifact's rows, for the bucket budget.
        t0 = time.perf_counter()
        rows_of = np.zeros(count, np.int64)
        for i in range(reader.metadata.num_row_groups):
            group = reader.read_row_group(i, columns=["key", "rank"])
            idx, _, _ = member_columns(
                group.append_column("entity", pa.nulls(group.num_rows, pa.uint64()).fill_null(0)),
                value_set, self.members_path,
            )
            del group
            rows_of += np.bincount(idx, minlength=count)
        self.stats["count_s"] = round(time.perf_counter() - t0, 2)

        # Buckets: ranges of the publication order under the row budget. An artifact over the
        # budget has a bucket to itself; its membership is published in slices, never declined.
        bucket_of = np.full(count, -1, np.int32)
        bucket_range: list[tuple[int, int]] = []
        start, filled = 0, 0
        for ordinal in range(count):
            if filled and filled + rows_of[ordinal] > self.bucket_rows:
                bucket_range.append((start, ordinal))
                start, filled = ordinal, 0
            bucket_of[ordinal] = len(bucket_range)
            filled += int(rows_of[ordinal])
        bucket_range.append((start, count))
        self.stats["buckets"] = len(bucket_range)
        del rows_of

        # One pass writing every row to its bucket, as 14-byte records.
        t0 = time.perf_counter()
        self.work.mkdir(parents=True, exist_ok=True)
        handles = [open(self.work / f"bucket-{b:05d}.bin", "wb") for b in range(len(bucket_range))]
        try:
            for i in range(reader.metadata.num_row_groups):
                group = reader.read_row_group(i, columns=["key", "rank", "entity"])
                idx, rank, entity = member_columns(group, value_set, self.members_path)
                del group
                bucket = bucket_of[idx]
                order = np.argsort(bucket, kind="stable")
                records = np.empty(len(idx), MEMBER_RECORD)
                records["idx"], records["rank"], records["entity"] = idx[order], rank[order], entity[order]
                bounds = np.searchsorted(bucket[order], np.arange(len(bucket_range) + 1))
                del idx, rank, entity, bucket, order
                for b, handle in enumerate(handles):
                    if bounds[b + 1] > bounds[b]:
                        handle.write(records[bounds[b]:bounds[b + 1]].tobytes())
                del records
        finally:
            for handle in handles:
                handle.close()
        self.stats["partition_s"] = round(time.perf_counter() - t0, 2)

        # One bucket at a time, sorted, every ordinal of its range yielded whether or not it has rows.
        for b, (lo, hi) in enumerate(bucket_range):
            path = self.work / f"bucket-{b:05d}.bin"
            records = np.fromfile(path, MEMBER_RECORD)
            path.unlink()
            groups = dict(rank_groups(records["idx"], records["rank"], records["entity"]))
            del records
            for ordinal in range(lo, hi):
                yield ordinal, groups.pop(ordinal, {})

    # -- bodies ----------------------------------------------------------------------------

    def _head(self, i: int) -> bytes:
        return b'{"key":' + json.dumps(self.rows[i]["key"]).encode() + b',"members":'

    def bodies(self):
        """Yield the requests in order: `("put", level, body, artifacts, members, edges)` as each
        batch fills, and `("grow", level, key, body, members)` for each slice that grows an artifact
        the batch before it published.

        A batch closes when the next artifact would take it over `--publish-max-bytes` or sits on
        another level; the wrapper is one level per request. An artifact whose whole membership does
        not fit closes the batch it is in, so its slices follow the request that created it.
        """
        if self.members_path is not None:
            self.work.mkdir(parents=True, exist_ok=True)
        batch: list[bytes] = []
        size = 0
        level = None
        counts = [0, 0, 0]
        for i, groups in self.members():
            block, members_n, edges_n, remainder = self._block(i, groups)
            if block is None:
                continue
            row_level = int(self.rows[i].get("level") or 0)
            if batch and (row_level != level or size + len(block) + 1 > self.max_bytes):
                yield "put", level, self._body(level, batch), *counts
                batch, size, counts = [], 0, [0, 0, 0]
            batch.append(block)
            size += len(block) + 1
            level = row_level
            counts[0] += 1
            counts[1] += members_n
            counts[2] += edges_n
            if remainder is not None:
                yield "put", level, self._body(level, batch), *counts
                batch, size, counts = [], 0, [0, 0, 0]
                yield from self._grow_slices(row_level, self.rows[i]["key"], remainder)
        if batch:
            yield "put", level, self._body(level, batch), *counts

    def _grow_body(self, level: int, key: str, members: np.ndarray) -> bytes:
        return (
            b'{"level":' + str(level).encode() + b',"addressing":"external","artifacts":[{"key":'
            + json.dumps(key).encode() + b',"members":' + json_list(members) + b"}]}"
        )

    def _grow_slices(self, level: int, key: str, members: np.ndarray):
        """`("grow", level, key, body, members)` for `members`, in slices of at most the cap."""
        fixed = len(self._grow_body(level, key, EMPTY_ENTITIES)) - 2
        per_slice = max(1, (self.max_bytes - fixed - 1) // 15)
        self.stats["grown_artifacts"] += 1
        self.stats["grown_members_sent"] += len(members)
        for start in range(0, len(members), per_slice):
            piece = members[start : start + per_slice]
            self.stats["grow_slices"] += 1
            yield "grow", level, key, self._grow_body(level, key, piece), len(piece)

    def _block(self, i: int, groups: dict) -> tuple[bytes | None, int, int, np.ndarray | None]:
        """One artifact's JSON with as many members as fit under the cap, its member and edge
        counts, and the members left to grow it with (None when the whole membership fit); or
        `(None, 0, 0, None)` for an artifact declined because its key, content and parents alone do
        not fit."""
        row = self.rows[i]
        parents = [key for key in (row.get("parent") or []) if key in self.held]
        # Parents are published before their children, so a parent's decline is known here.
        kept_parents = [key for key in parents if key not in self.declined_keys]
        self.stats["edges_dropped_to_declined"] += len(parents) - len(kept_parents)
        parents = kept_parents
        members = groups.get(-1, EMPTY_ENTITIES)
        self.stats["members"] += len(members)
        head = self._head(i)
        contents = row.get("contents") or []
        content_heads: list[tuple[bytes, np.ndarray]] = []
        for rank, values in enumerate(contents):
            generated = groups.get(rank, EMPTY_ENTITIES)
            self.stats["generating_set_entries"] += len(generated)
            content_heads.append((b'{"values":' + json.dumps(list(values)).encode() + b',"generated_from":', generated))
        tail = b""
        if parents:
            tail += b',"parent":' + json.dumps(parents).encode()
        if row.get("attached_layer"):
            tail += b',"attached_to":' + json.dumps(
                {
                    "layer": row["attached_layer"],
                    "level": int(row.get("attached_level") or 0),
                    "key": row["attached_key"],
                }
            ).encode()
        tail += b"}"
        # Sized before anything large is built: a declined artifact's list is never assembled, and
        # a grown one's is built only as far as the cap allows.
        fixed = len(head) + len(tail) + len(self._body(0, [b""]))
        if content_heads:
            fixed += len(b',"content":[') + 1 + sum(
                len(h) + json_list_bytes(len(g)) + 2 for h, g in content_heads
            )
        budget = self.max_bytes - fixed
        if budget < json_list_bytes(0):
            size = fixed + json_list_bytes(len(members))
            self.declined_keys.add(row["key"])
            self.stats["declined_artifacts"].append(
                {"key": row["key"], "members": len(members), "body_bytes": size, "content_counted": True}
            )
            return None, 0, 0, None
        fit = max(0, (budget - 1) // 15)
        remainder = None
        if fit < len(members):
            members, remainder = members[:fit], members[fit:]
        parts = [head, json_list(members)]
        if content_heads:
            parts.append(b',"content":[')
            parts.append(b",".join(h + json_list(g) + b"}" for h, g in content_heads))
            parts.append(b"]")
        parts.append(tail)
        return b"".join(parts), len(members), len(parents), remainder

    def _body(self, level: int, blocks: list[bytes]) -> bytes:
        return (
            b'{"level":' + str(level).encode() + b',"addressing":"external","artifacts":['
            + b",".join(blocks)
            + b"]}"
        )

    def cleanup(self) -> None:
        """Remove the layer's transient buckets. Called whether or not the publication finished."""
        if self.work.exists():
            shutil.rmtree(self.work, ignore_errors=True)


# ---------------------------------------------------------------------------------------------
# The control plane
# ---------------------------------------------------------------------------------------------


class Control:
    def __init__(self, base: str, cred: str, view: str | None = None):
        self.base = base
        self.headers = {"Authorization": f"Bearer {cred}"}
        # **`x-tessera-view` where the bundle has more than one.** A batch carries one row space,
        # and which one it belongs to is not inferable from its columns, so a multi-view deployment
        # refuses an unlabelled batch outright (contracts §3.4). One view is the header's absence,
        # which is what every rung below rung 5 sends.
        self.view = view

    def status(self) -> dict:
        r = requests.get(f"{self.base}/control/status", headers=self.headers, timeout=60)
        r.raise_for_status()
        return r.json()

    def ingest(self, body: bytes, batch_id: str, session: requests.Session, timeout=600):
        t0 = time.perf_counter()
        r = session.post(
            f"{self.base}/control/ingest",
            headers=self.headers
            | {"x-tessera-batch-id": batch_id, "Content-Type": "application/octet-stream"}
            | ({"x-tessera-view": self.view} if self.view else {}),
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0

    def changes(self, items: list[dict], timeout=600):
        t0 = time.perf_counter()
        r = requests.post(
            f"{self.base}/control/changes", headers=self.headers, json=items, timeout=timeout
        )
        return r, time.perf_counter() - t0

    def flush(self):
        return requests.post(f"{self.base}/control/flush", headers=self.headers, timeout=60)

    def compact(self):
        return requests.post(f"{self.base}/control/compact", headers=self.headers, timeout=60)

    def register_layer(self, declaration: dict):
        return requests.put(
            f"{self.base}/control/layers", headers=self.headers, json=declaration, timeout=120
        )

    def grow(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PATCH /control/layers/{name}/artifacts` (decision 0127): more members for artifacts the
        level already holds, the body already serialised, as [`Control.publish`] sends its own."""
        t0 = time.perf_counter()
        r = session.patch(
            f"{self.base}/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts",
            headers=self.headers | {"Content-Type": "application/json"},
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0

    def publish(self, layer: str, body: bytes, session: requests.Session, timeout=1800):
        """`PUT /control/layers/{name}/artifacts`, with the body already serialised.

        Sent as bytes under an explicit content type rather than through `json=`: the body is
        assembled once as bytes by [`Publication`], and handing `requests` a dict would serialise
        a 10⁶-member artifact a second time.

        **The layer name is percent-encoded**, because the route matches one path segment and this
        rung's names are path-shaped: `clusters/kmeans` unencoded is a 404 at the router rather
        than a refusal from the handler, which reads as an empty publication rather than as an
        error.
        """
        t0 = time.perf_counter()
        r = session.put(
            f"{self.base}/control/layers/{urllib.parse.quote(layer, safe='')}/artifacts",
            headers=self.headers | {"Content-Type": "application/json"},
            data=body,
            timeout=timeout,
        )
        return r, time.perf_counter() - t0


def wait_for(predicate, timeout: float, interval: float = 0.5) -> tuple[bool, float]:
    """Poll `predicate` until true. Returns `(reached, seconds)`."""
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout:
        if predicate():
            return True, time.perf_counter() - t0
        time.sleep(interval)
    return False, time.perf_counter() - t0


# ---------------------------------------------------------------------------------------------
# The census, and what equivalence means
# ---------------------------------------------------------------------------------------------


def census(
    viewer: str,
    session_base: str,
    cred: str,
    view: str,
    quant: dict,
    ladder: Sequence[dict],
    boxes: Sequence[tuple[int, list[float]]],
) -> dict:
    """Masked counts per principal: zoom 0 over the whole extent, then each given box.

    **Per principal and per layer**, because a single total passes any defect that moves rows
    between principals while preserving the sum — the same argument `tessera corpus census` makes
    for per-tile counts (correctness-suite §9.2).
    """
    full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
    out: dict = {}
    for rung in ladder:
        token, _ = serve_battery.authorise(session_base, cred, rung["terms"])
        whole = serve_battery.viewport(viewer, token, view, 0, full, k=1)
        row = {
            "terms": rung["terms"],
            "zoom0_visible": whole["counts"]["visible"],
            "boxes": [],
            "layers": {},
        }
        for zoom, box in boxes:
            s = serve_battery.viewport(viewer, token, view, zoom, box, k=1, layers=None)
            row["boxes"].append(
                {"zoom": zoom, "box": box, "visible": s["counts"]["visible"]}
            )
        # The artifact frames ride the ordinary viewport response when `layers` is asked for, so
        # the per-layer census is the whole-extent request with `layers: "all"` and the frames
        # counted off it.
        r = requests.post(
            f"{viewer}/v1/viewport",
            headers={"Authorization": f"Bearer {token}"},
            json={"view": view, "zoom": 0, "bbox": full, "k": 1, "layers": "all"},
            timeout=300,
        )
        r.raise_for_status()
        row["layers"] = artifact_frame_census(r.content)
        out[f"{rung['target']:.4f}"] = row
    return out


def artifact_frame_census(content: bytes) -> dict:
    """The kind-5 artifact frames of a viewport response, **per layer**: artifacts and masked count.

    The wire is a sequence of `(u8 kind, u32 LE length, payload)` frames, and kind 5 is one row per
    served artifact (`tessera-wire/src/payload.rs`). Its `layer` column is dictionary-encoded and
    its `masked_count` is what the viewer is shown, so grouping the rows by layer gives exactly
    what decision 0091's per-layer test asks for: how many artifacts this principal is served on
    each layer, and how many documents those artifacts count for them.

    **Two numbers per layer, not one.** An artifact count alone passes a defect that serves the
    right artifacts with the wrong memberships; a summed masked count alone passes one that moves
    members between artifacts of the same layer.
    """
    out: dict = {}
    offset = 0
    while offset + 5 <= len(content):
        kind = content[offset]
        length = int.from_bytes(content[offset + 1 : offset + 5], "little")
        payload = content[offset + 5 : offset + 5 + length]
        offset += 5 + length
        if kind != 5 or not payload:
            continue
        table = ipc.open_stream(payload).read_all()
        layers = table.column("layer").to_pylist()
        counts = table.column("masked_count").to_pylist()
        for layer, count in zip(layers, counts):
            entry = out.setdefault(layer, {"artifacts": 0, "masked_count": 0})
            entry["artifacts"] += 1
            entry["masked_count"] += int(count or 0)
    return out


def compare_census(folded: dict, all_in: dict) -> dict:
    """The 0091 test's answer: exact zero difference, or a listed one.

    **Three surfaces, reported separately**, because they are not equally comparable:

    * `zoom0` — the whole extent under each principal. Frame-independent: every row of the corpus
      is in the box whatever the frame is, so this is the surface on which "the two deployments
      agree" is a claim about the *data* and nothing else.
    * `boxes` — a box at zoom 3, 6 and 9. Frame-**dependent**, and on a rung declaring
      `extent = "auto"` the base build's frame is computed from the rows it saw, so a base built
      from the complement quantises onto a slightly different grid than the all-in build does and
      the margins of a box disagree by a handful of rows. That is a property of `auto`, not of
      ingest, and the frames are recorded beside the counts so a reader can see it.
    * `layers` — the kind-5 artifact frame, per layer: how many artifacts this principal is
      served on it and what they count for them. One entry per layer that differs.
    """
    differences = []
    for key in sorted(set(folded) | set(all_in)):
        a, b = folded.get(key), all_in.get(key)
        if a is None or b is None:
            differences.append({"principal": key, "missing_from": "folded" if a is None else "all_in"})
            continue
        if a["zoom0_visible"] != b["zoom0_visible"]:
            differences.append(
                {
                    "principal": key,
                    "where": "zoom0",
                    "folded": a["zoom0_visible"],
                    "all_in": b["zoom0_visible"],
                }
            )
        for i, (x, y) in enumerate(zip(a["boxes"], b["boxes"])):
            if x["visible"] != y["visible"]:
                differences.append(
                    {
                        "principal": key,
                        "where": f"box {i} zoom {x['zoom']}",
                        "folded": x["visible"],
                        "all_in": y["visible"],
                    }
                )
        # **One difference per layer**, not one per principal: a layer the wire declined and a
        # layer whose masked counts moved are different findings, and a single blob comparison
        # reports them as one.
        for layer in sorted(set(a["layers"]) | set(b["layers"])):
            folded_layer = a["layers"].get(layer)
            all_in_layer = b["layers"].get(layer)
            if folded_layer != all_in_layer:
                differences.append(
                    {
                        "principal": key,
                        "where": "layers",
                        "layer": layer,
                        "folded": folded_layer,
                        "all_in": all_in_layer,
                    }
                )
    by_surface: dict[str, int] = {}
    for d in differences:
        where = d.get("where", "missing")
        surface = "zoom0" if where == "zoom0" else ("layers" if where == "layers" else "boxes")
        by_surface[surface] = by_surface.get(surface, 0) + 1
    return {
        "equal": not differences,
        "zoom0_equal": by_surface.get("zoom0", 0) == 0,
        "boxes_equal": by_surface.get("boxes", 0) == 0,
        "layers_equal": by_surface.get("layers", 0) == 0,
        "differences_by_surface": by_surface,
        "differences": differences,
    }


# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------


def executor_laps(before: dict, after: dict, rows: int) -> dict:
    """The `WriteStage` laps across one ingest phase, µs per accepted row.

    Differenced rather than read absolute, because the laps are process totals and the base's own
    open may have closed windows before the phase began. **All zero without `bench-timing`** — the
    `bench_timing` flag is carried through so a reader can tell an uninstrumented binary from an
    idle executor. `unattributed` is the coarse `apply_nanos_total` minus the three `apply` laps
    plus whatever `submit\u2192receipt` sees beyond the executor's own stages; it is reported, not
    hidden, because the close's stages are meant to partition its wall clock.
    """
    stages = after.get("stage_nanos") or {}
    prior = before.get("stage_nanos") or {}
    windows = after.get("wal_fsyncs", 0) - before.get("wal_fsyncs", 0)
    laps = {
        name: round((nanos - prior.get(name, 0)) / 1000.0 / rows, 3) if rows else None
        for name, nanos in stages.items()
    }
    executor = sum(
        laps.get(name) or 0.0
        for name in ("allocate", "wal_append", "wal_fsync", "buffer_clone", "apply_rows",
                     "swap", "admit", "record_batch")
    )
    return {
        "bench_timing": after.get("bench_timing", False),
        "rows": rows,
        "windows": windows,
        "rows_per_window": round(rows / windows, 1) if windows else None,
        "us_per_row": laps,
        "executor_sum_us_per_row": round(executor, 3),
        "queueing_us_per_row": round((laps.get("submit\u2192receipt") or 0.0) - executor, 3),
        "apply_nanos_total_us_per_row": round(
            (after.get("apply_nanos_total", 0) - before.get("apply_nanos_total", 0))
            / 1000.0 / rows, 3
        ) if rows else None,
        "apply_nanos_max_ms": round(after.get("apply_nanos_max", 0) / 1e6, 1),
        "work_service_nanos_ewma_ms": round(after.get("work_service_nanos_ewma", 0) / 1e6, 1),
    }


class Cycle:
    def __init__(self, args):
        self.args = args
        self.rung = Path(args.rung_dir)
        self.work = Path(args.work)
        self.binary = Path(args.binary)
        self.result: dict = {
            "fraction": args.fraction,
            "concurrency": args.concurrency,
            "seed": args.seed,
            "batch_rows": BATCH_ROWS,
        }

    def log(self, message: str) -> None:
        print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)

    # -- 1. split and build ---------------------------------------------------------------

    def build_base(self) -> Path:
        base_dir = self.work / f"base-{self.args.fraction:g}"
        bundle = base_dir / "bundle"
        base_ids, held = split_entities(self.rung / "points.parquet", self.args.fraction, self.args.seed)
        self.result["base_rows"] = int(len(base_ids))
        self.result["holdout_rows"] = int(len(held))
        self.held = held
        if self.args.reuse_base and (bundle / "CURRENT").exists():
            self.log(f"reusing {bundle}")
            self.result["build"] = {"reused": True}
            return base_dir
        # **The split survives a failed build.** Writing rung 4's base inputs is a quarter of an
        # hour and 50 GB, and a build that dies after it — out of memory, out of disc — would
        # otherwise pay for it again. `--reuse-base` reuses a *prepared* base as well as a built
        # one, on the same test the split itself would apply: the points file exists and holds the
        # rows this fraction and seed ask for.
        prepared = base_dir / "points.parquet"
        reuse_inputs = (
            self.args.reuse_base
            and prepared.exists()
            and pq.ParquetFile(prepared).metadata.num_rows == len(base_ids)
        )
        if reuse_inputs:
            self.log(f"reusing the prepared base inputs at {base_dir}")
        else:
            if base_dir.exists():
                shutil.rmtree(base_dir)
            self.log(f"splitting: base {len(base_ids):,} rows, hold-out {len(held):,} rows")
            write_base_inputs(self.rung, base_dir, base_ids)
            # Whatever the split's last row group left in the pool goes back before the build,
            # which runs beside this process and needs the memory more.
            pa.default_memory_pool().release_unused()
        if self.args.state_extent:
            self.result["stated_extent"] = state_extent(
                base_dir / "corpus.toml", self.rung / "bundle"
            )
            self.log(f"stated the all-in frame in the base declaration: {self.result['stated_extent']}")
        stages = base_dir / "stage-timings.json"
        t0 = time.perf_counter()
        # **A zero-row base is a real case and it is the point of the f = 1.0 cell**: nothing is
        # held for the build at all, so this is `tessera build` over an empty points file, and
        # whether a deployment can start from one is the first thing this driver finds out.
        proc = subprocess.run(
            [
                str(self.binary),
                "build",
                # **The base's rows must be addressable by external id**, because the artifacts
                # published after the ingest name their members that way and most of those members
                # are base rows. The flag mints one per item from its source entity id
                # (`ExternalIdRow`), which is the form [`encode_batch`] sends for the hold-out.
                "--mint-external-ids",
                "--deployment",
                str(base_dir / "tessera.toml"),
                "--stage-timings-json",
                str(stages),
                "--stage-timings",
            ],
            cwd=base_dir,
            capture_output=True,
            text=True,
        )
        wall = time.perf_counter() - t0
        self.result["build"] = {
            "wall_s": round(wall, 2),
            "returncode": proc.returncode,
            "stdout": proc.stdout[-4000:],
            "stderr_tail": proc.stderr[-4000:],
            "stages": json.loads(stages.read_text()) if stages.exists() else None,
            "bundle_bytes": sum(p.stat().st_size for p in bundle.rglob("*") if p.is_file())
            if bundle.exists()
            else 0,
        }
        if proc.returncode != 0:
            self.result["blocked"] = {
                "at": "base build",
                "fraction": self.args.fraction,
                "refusal": proc.stderr[-2000:],
            }
        return base_dir

    # -- 2. ingest ------------------------------------------------------------------------

    def run_ingest(self, control: Control, source, label: str) -> dict:
        """Put `source`'s batches through `/control/ingest` at *C* concurrent callers.

        `source` yields `(first row index, body, rows)`. It is a **generator**, so the hold-out is
        encoded as it is sent rather than built in full first — and the pool is fed through a
        bounded window so a fast producer cannot put the whole corpus's encoded batches in memory
        in front of a slower server.
        """
        run_id = uuid.uuid4().hex[:8]
        acks: list[float] = []
        statuses: dict[str, int] = {}
        totals = {"accepted": 0, "minted": 0, "offered": 0, "batches": 0}
        lock = __import__("threading").Lock()

        def one(item):
            start, body, rows = item
            batch_id = f"{label}-{run_id}-{start}"
            session = requests.Session()
            r = dt = None
            # **A 429 is backpressure, not a failure**: the contract says retry, so the driver
            # does, and counts every one. A run that reported the 429 as a refusal would report
            # the server's own flow control as an error rate.
            for _ in range(600):
                r, dt = control.ingest(body, batch_id, session)
                with lock:
                    statuses[str(r.status_code)] = statuses.get(str(r.status_code), 0) + 1
                if r.status_code == 429:
                    time.sleep(min(float(r.headers.get("retry-after", "1")), 5.0))
                    continue
                break
            return r, dt, rows

        t0 = time.perf_counter()
        window = max(2 * self.args.concurrency, 4)
        with concurrent.futures.ThreadPoolExecutor(max_workers=self.args.concurrency) as pool:
            pending: set = set()
            for item in source:
                pending.add(pool.submit(one, item))
                if len(pending) >= window:
                    done, pending = concurrent.futures.wait(
                        pending, return_when=concurrent.futures.FIRST_COMPLETED
                    )
                    self._collect(done, acks, totals)
            self._collect(pending, acks, totals)
        wall = time.perf_counter() - t0
        return {
            "batches": totals["batches"],
            "rows_offered": totals["offered"],
            "accepted": totals["accepted"],
            "minted": totals["minted"],
            "wall_s": round(wall, 2),
            "items_per_s": round(totals["accepted"] / wall, 1) if wall else None,
            "ack_ms": serve_battery.percentiles(acks),
            "statuses": statuses,
        }

    def _collect(self, futures, acks: list, totals: dict) -> None:
        for future in futures:
            r, dt, rows = future.result()
            acks.append(dt * 1000.0)
            totals["batches"] += 1
            totals["offered"] += rows
            if r.status_code == 200:
                body = r.json()
                totals["accepted"] += body["accepted"]
                totals["minted"] += body.get("minted", 0)
            elif "first_refusal" not in self.result:
                self.result["first_refusal"] = {"status": r.status_code, "body": r.text[:1500]}
            if totals["batches"] % 100 == 0:
                self.log(f"  {totals['batches']} batches, {totals['accepted']:,} accepted")

    # -- the whole thing ------------------------------------------------------------------

    def run(self) -> dict:
        args = self.args
        base_dir = self.build_base()
        if self.result.get("blocked"):
            self.log("BLOCKED at the base build; the refusal is in the result")
            return self.result
        self.result["driver_rss"] = {"after_base": driver_rss()}

        scratch = self.work / f"serve-{args.fraction:g}"
        # **A run that flushes writes into the bundle it serves.** Every publication adds a side
        # manifest, so a base is a *different* base after one cell has ingested into it — the next
        # `--reuse-base` run 409s on the hold-out its predecessor published. Serving a copy is what
        # makes the flag mean what it says; without it only the first cell over a given base is the
        # cell that was intended.
        bundle = base_dir / "bundle"
        if args.copy_base:
            bundle = scratch / "bundle"
            if bundle.exists():
                shutil.rmtree(bundle)
            scratch.mkdir(parents=True, exist_ok=True)
            t0 = time.perf_counter()
            shutil.copytree(base_dir / "bundle", bundle)
            self.result["base_copy_s"] = round(time.perf_counter() - t0, 2)
            self.log(f"copied the base bundle in {self.result['base_copy_s']} s")
        served = Deployment(
            base_dir,
            bundle,
            scratch,
            (args.port0, args.port0 + 1, args.port0 + 2),
            self.binary,
            ingest=json.loads(args.ingest_config) if args.ingest_config else None,
        )
        self.result["ingest_config"] = served.ingest
        served.clear_scratch()
        t0 = time.perf_counter()
        try:
            served.start()
        except Exception as e:  # a zero-row bundle that cannot be opened is the finding
            self.result["blocked"] = {"at": "serve", "refusal": str(e)[:2000]}
            self.log(f"BLOCKED at serve: {e}")
            return self.result
        self.result["open_s"] = round(time.perf_counter() - t0, 2)
        self.log(f"served pid={served.pid} open={self.result['open_s']} s")

        try:
            # The anchor view: the hold-out's rows are that row space's, and a bundle carrying
            # more than one refuses an unlabelled batch. Named from the declaration rather than
            # from `/v1/meta`'s order, which is creation order and not the anchor.
            declared = tomllib.loads((self.rung / "corpus.toml").read_text())
            views = [v["name"] for v in declared.get("view", [])]
            anchor = declared.get("allocation_view") or (views[0] if views else None)
            control = Control(
                served.control, served.credential("operator"),
                view=anchor if len(views) > 1 else None,
            )
            self.result["ingested_view"] = control.view
            session_cred = served.credential("session")
            ranks = json.loads(ranks_file(self.rung).read_text())
            all_terms = sorted(r["term"] for r in ranks)
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            m = serve_battery.meta(served.viewer, token)
            view = m["views"][0]["id"]
            quant = m["views"][0]["quantisation"]
            self.result["base_visible"] = serve_battery.viewport(
                served.viewer, token, view, 0,
                [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]], k=1
            )["counts"]["visible"]

            head = 3 * args.write_cycle_n if args.write_cycle else 0
            hold = HoldOut(self.rung, self.held, head_rows=head)
            self.log(f"ingesting {len(self.held):,} rows at C={args.concurrency}")
            before = control.status()["write_executor"]
            self.result["ingest"] = self.run_ingest(control, hold.batches(), "cycle")
            self.result["ingest"]["membership_columns"] = hold.member_stats
            self.result["executor_laps"] = executor_laps(
                before,
                control.status()["write_executor"],
                self.result["ingest"]["accepted"],
            )
            self.log(f"  {self.result['ingest']['items_per_s']} items/s")
            self.result["driver_rss"]["after_ingest"] = driver_rss()
            if args.stop_after_ingest:
                # **The attribution cell, not the cycle.** Everything after this measures
                # publication, flush and the fold; a run that only wants the executor's laps
                # pays ~an hour for figures it is not reading. The equivalence census is
                # therefore *absent* from such a run's result, not passed — see `stop_after`.
                self.result["stop_after"] = "ingest"
                return self.result

            # **Every artifact, after every point it depends on.** Before the flush, deliberately:
            # an ingested row is resolvable by its external id from the moment it is acked
            # (`Session::resolve_external_ids` consults the live map first), so the ordering the
            # ruling states — points before the artifacts that name them — is the only one there is.
            self.result["publish"] = self.publish_layers(control)
            self.result["driver_rss"]["after_publish"] = driver_rss()

            # **Each phase's failure is recorded and the run continues.** A cell that died at the
            # flush used to lose its ingest figures too, which are the expensive half; and a
            # failure here is as often a *result* — a shed stream, a refusal — as it is a bug in
            # the driver.
            def phase(name, fn):
                try:
                    self.result[name] = fn()
                except Exception as e:  # noqa: BLE001 — a driver failure is a recorded outcome
                    self.result[name] = {"failed": f"{type(e).__name__}: {e}"[:2000]}
                    self.log(f"  {name} FAILED: {type(e).__name__}: {e}")

            phase("flush", lambda: self.do_flush(control, served, session_cred, view, quant, all_terms))
            phase("layers_after_ingest", lambda: self.probe_layers_after_ingest(
                served, session_cred, view, quant, all_terms
            ))
            phase("fold", lambda: self.do_fold(control))
            phase("equivalence", lambda: self.do_equivalence(served, session_cred, view, quant, ranks))
            if args.write_cycle:
                phase("write_cycle", lambda: self.do_write_cycle(
                    control, served, session_cred, view, quant, all_terms, hold
                ))
        finally:
            self.result["status_at_end"] = safe(lambda: Control(served.control, served.credential("operator")).status())
            self.result["driver_rss"]["end"] = driver_rss()
            served.stop()
        return self.result

    # -- the artifacts, on the wire --------------------------------------------------------

    def publish_layers(self, control: Control) -> dict:
        """Publish every declared layer, or record why it was not. Its own figure.

        **The whole roster, and the whole of each artifact's membership**, base rows and ingested
        rows alike. An artifact exists because the layer declares it, so publishing only the ones
        whose members survived the split would make the two deployments differ in their *roster* as
        well as in their membership, which is a second variable in a test that has one.

        **Every declared layer is accounted for.** Each has an entry under `layers`, under
        `on_column`, or under `declined` with its reason: a column-route layer's membership rode
        the ingest batches (the module doc) and is recorded under `on_column` with what the hold-out
        carried; an attribute-membership layer has nothing to publish (an ingested row joins it
        through the column its batch carries); a layer with supplied content and no roster cannot
        be published; a roster whose publication failed carries the failure. Within a published
        layer, an artifact whose body alone would exceed the route's cap is declined per artifact
        and listed in the layer's `declined_artifacts`, so a census difference on the layer is
        attributable to named artifacts.

        Bodies are sent as they are assembled. `wall_s` is the sum of the requests' round trips,
        the service's cost, and `prepared_s` the time spent inside the body generator, the
        driver's; `phase_s` is the two together with whatever else the loop spent.
        """
        out: dict = {"layers": {}, "on_column": {}, "declined": {}}
        totals = {"artifacts": 0, "members": 0, "wall_s": 0.0, "requests": 0}
        work = self.work / f"publish-{self.args.fraction:g}"
        for layer in declared_layers(self.rung):
            name = layer["name"]
            if layer["route"] == "column":
                out["on_column"][name] = {
                    "reason": "no supplied content and no roster: the base build read the member "
                    "table over the base's rows, and every hold-out row carried its member list as "
                    "the ingest batch's column named for the layer (decision 0128)",
                    "member_rows": pq.ParquetFile(layer["members"]).metadata.num_rows,
                    "holdout": (self.result.get("ingest") or {}).get("membership_columns", {}).get(name),
                }
                self.log(f"  {name}: nothing to publish, membership travelled on the ingest column")
                continue
            if layer["attribute"] is not None:
                out["declined"][name] = {
                    "reason": f"membership is the `{layer['attribute']}` attribute column: the layer "
                    f"has no roster and no member table, and an ingested row joins it through the "
                    f"attribute its batch carries",
                }
                self.log(f"  {name}: nothing to publish, membership is the `{layer['attribute']}` attribute")
                continue
            if layer["roster"] is None:
                members = layer["members"]
                if layer["supplied"]:
                    reason = (
                        "supplied content and no artifact roster: a layer declaring supplied content "
                        "refuses to mint from a column, and the publication route takes a roster"
                    )
                else:
                    reason = (
                        "no artifact roster and no member table: nothing names this layer's "
                        "artifacts, so there is nothing to mint at the build or to publish"
                    )
                out["declined"][name] = {
                    "reason": reason + "; not published",
                    "member_rows": pq.ParquetFile(members).metadata.num_rows
                    if members is not None and members.exists()
                    else None,
                }
                self.log(f"  {name}: NOT PUBLISHED, {reason.split(':')[0]}")
                continue
            if not layer["roster"].exists():
                out["declined"][name] = {"reason": f"roster {layer['roster'].name} is not in the rung directory"}
                self.log(f"  {name}: NOT PUBLISHED, {layer['roster'].name} absent")
                continue
            try:
                entry = self.publish_layer(control, name, layer, work / name.replace("/", "__"))
            except Exception as e:  # noqa: BLE001 — the failure is the layer's record
                out["declined"][name] = {"reason": f"{type(e).__name__}: {e}"[:1500]}
                self.log(f"  {name}: FAILED, {type(e).__name__}: {str(e)[:200]}")
                continue
            out["layers"][name] = entry
            totals["artifacts"] += entry["published_artifacts"]
            totals["members"] += entry["published_members"]
            totals["wall_s"] += entry["wall_s"]
            totals["requests"] += entry["requests"]
        totals["wall_s"] = round(totals["wall_s"], 2)
        totals["artifacts_per_s"] = (
            round(totals["artifacts"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        totals["members_per_s"] = (
            round(totals["members"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        out["totals"] = totals
        out["edges_declared"] = sum(e["edges_declared"] for e in out["layers"].values())
        out["edges_published"] = sum(e["edges_published"] for e in out["layers"].values())
        return out

    def publish_layer(self, control: Control, name: str, layer: dict, work: Path) -> dict:
        """One layer: its bodies assembled and sent in turn, one caller, serial."""
        publication = Publication(
            layer["roster"], layer["members"], work, self.args.publish_max_bytes, self.args.publish_bucket_rows
        )
        statuses: dict[str, int] = {}
        refusal = None
        refusals = 0
        first_by_status: dict[str, dict] = {}
        published = {"artifacts": 0, "members": 0, "edges": 0}
        grown = {"requests": 0, "members": 0, "joined": 0}
        prepared_s = 0.0
        wall_s = 0.0
        requests_n = 0
        session = requests.Session()
        t_phase = time.perf_counter()
        bodies = publication.bodies()
        try:
            while True:
                t0 = time.perf_counter()
                item = next(bodies, None)
                prepared_s += time.perf_counter() - t0
                if item is None:
                    break
                if item[0] == "grow":
                    _, level, key, body, members_n = item
                    artifacts, edges = 0, 0
                    r, dt = control.grow(name, body, session)
                    grown["requests"] += 1
                else:
                    _, level, body, artifacts, members_n, edges = item
                    key = None
                    r, dt = control.publish(name, body, session)
                del body
                wall_s += dt
                requests_n += 1
                statuses[str(r.status_code)] = statuses.get(str(r.status_code), 0) + 1
                if key is not None and r.status_code == 200:
                    grown["members"] += members_n
                    published["members"] += members_n
                    try:
                        grown["joined"] += sum(int(a.get("joined") or 0) for a in r.json()["artifacts"])
                    except (ValueError, KeyError, TypeError):
                        pass
                elif key is None and r.status_code == 201:
                    published["artifacts"] += artifacts
                    published["members"] += members_n
                    published["edges"] += edges
                else:
                    # **Never quiet.** Each status is logged with its detail the first time it
                    # appears, and every refusal is counted into the record and the summary line.
                    refusals += 1
                    if refusal is None:
                        refusal = {"level": level, "status": r.status_code, "body": r.text[:1500]}
                    if str(r.status_code) not in first_by_status:
                        first_by_status[str(r.status_code)] = {"level": level, "body": r.text[:1500]}
                        what = f"grow of {key!r}" if key is not None else f"{artifacts} artifact(s) in the batch"
                        self.log(
                            f"  {name}: REFUSED {r.status_code} at level {level} ({what}): {r.text[:300]}"
                        )
        finally:
            publication.cleanup()
        phase_s = time.perf_counter() - t_phase
        stats = publication.stats
        declined = stats.pop("declined_artifacts")
        entry = dict(stats)
        entry.update(
            {
                "requests": requests_n,
                "prepared_s": round(prepared_s, 2),
                "wall_s": round(wall_s, 2),
                "phase_s": round(phase_s, 2),
                "published_artifacts": published["artifacts"],
                "published_members": published["members"],
                "edges_published": published["edges"],
                "artifacts_per_s": round(published["artifacts"] / wall_s, 1) if wall_s else None,
                "members_per_s": round(published["members"] / wall_s, 1) if wall_s else None,
                "statuses": statuses,
                "refusals": refusals,
                "first_refusal": refusal,
                "first_refusal_by_status": first_by_status,
                "grow_requests": grown["requests"],
                "grown_members": grown["members"],
                "grown_members_joined": grown["joined"],
                "grown_members_unjoined": grown["members"] - grown["joined"],
                "declined_artifacts": declined,
                "declined_members": sum(d["members"] for d in declined),
            }
        )
        if grown["members"] != grown["joined"]:
            self.log(
                f"  {name}: {grown['members'] - grown['joined']:,} of {grown['members']:,} grown "
                f"members did not join — the route already held them, or refused them"
            )
        self.log(
            f"  {name}: {published['artifacts']:,} of {stats['artifacts']:,} artifacts, "
            f"{published['members']:,} members in {wall_s:.1f} s ({entry['artifacts_per_s']} "
            f"artifacts/s, {entry['members_per_s']} members/s), {requests_n} requests, "
            f"{refusals} refused; {stats['grown_artifacts']:,} artifact(s) grown by "
            f"{grown['requests']:,} PATCH(es) carrying {grown['members']:,} members; "
            f"{published['edges']:,}/{stats['edges_declared']:,} parent edges, "
            f"{stats['edges_dropped_to_declined']:,} dropped to declined parents, "
            f"{len(declined)} artifact(s) declined over the cap; {stats['read_path']}, "
            f"driver {prepared_s:.1f} s"
        )
        return entry

    def probe_layers_after_ingest(self, served, session_cred, view, quant, all_terms) -> dict:
        """One zoom-0 viewport **with `layers: "all"`** after the flush, timed and allowed to fail.

        Its own measurement because it is the request that broke the first 3.6×10⁷ cell. The
        trigger is not the flush: it is the record change under a level — a publication here, a
        one-row growth before — which moves the level's version, after which the engine refuses the
        fold-written column and rebuilds the level's row form on the next layered request —
        94–113 s here, shed against `serve.stream_deadline_ms`. Reproduced with no flush in
        `probes/2026-09-03-growth-trigger/`; the fix is ruled in
        `docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`. Recorded rather than
        routed around.
        """
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]
        token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
        t0 = time.perf_counter()
        try:
            s = serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers="all", timeout=600
            )
            return {
                "served": True,
                "wall_s": round(time.perf_counter() - t0, 2),
                "server_ms": s["server_ms"],
                "visible": s["counts"]["visible"],
            }
        except Exception as e:  # noqa: BLE001 — the shed is the finding
            return {
                "served": False,
                "wall_s": round(time.perf_counter() - t0, 2),
                "error": f"{type(e).__name__}: {e}"[:600],
            }

    # -- flush, fold, equivalence, write cycle ---------------------------------------------

    def do_flush(self, control, served, session_cred, view, quant, all_terms) -> dict:
        before = control.status()["write_executor"]["flush"]["flushes"]
        expected = (self.result["base_visible"] or 0) + self.result["ingest"]["accepted"]
        t0 = time.perf_counter()
        code = control.flush().status_code
        request_s = time.perf_counter() - t0
        # **Or nothing left to flush.** Under the row trigger (write-path §4.1) a fast loader's
        # rows are published as they arrive, so the buffer can be empty when this request lands —
        # and a tick against an empty buffer publishes nothing and moves no counter. Waiting on
        # the counter alone then burns the whole `--flush-timeout` on a deployment that is already
        # fully visible, which is what this cell would otherwise report as a 900 s flush.
        def flush_landed() -> bool:
            flush = control.status()["write_executor"]["flush"]
            return flush["flushes"] > before or flush["buffered_items"] == 0

        published, publish_s = wait_for(flush_landed, timeout=self.args.flush_timeout)
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]

        # **`layers=None`, and that is not a detail.** A zoom-0 whole-extent viewport asking for
        # `layers: "all"` on a freshly-ingested 3.6×10⁷-row deployment was **shed mid-body** at
        # 113 s against a 60 s stream deadline: the layer probe's growth moved the mesh level's
        # version, so its row form is rebuilt from scratch on the first layered request after it.
        # That is a result about the read path after a record change (in `layers_after_ingest`),
        # not something a visibility poll should be measuring — what this needs is the masked
        # count, which the tiles frame carries on its own.
        def visible_now() -> int:
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            return serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers=None
            )["counts"]["visible"]

        reached, visibility_s = wait_for(
            lambda: visible_now() >= expected, timeout=self.args.flush_timeout, interval=0.25
        )
        return {
            "status": code,
            "request_s": round(request_s, 4),
            "published": published,
            "publish_s": round(publish_s, 3),
            "expected_visible": expected,
            "visible": visible_now(),
            "visibility_reached": reached,
            "visibility_s": round(visibility_s, 3),
        }

    def do_fold(self, control) -> dict:
        before = control.status()["compaction"]["folds"]
        code = control.compact().status_code
        done, wall = wait_for(
            lambda: control.status()["compaction"]["folds"] > before,
            timeout=self.args.fold_timeout,
            interval=1.0,
        )
        compaction = control.status()["compaction"]
        return {
            "status": code,
            "completed": done,
            "observed_s": round(wall, 2),
            "fold_s": compaction.get("last_secs"),
            "fold_peak_rss_bytes": compaction.get("last_rss_bytes"),
            "folds": compaction.get("folds"),
            "fold_failures": compaction.get("fold_failures"),
            "live_rows": compaction.get("live_rows"),
        }

    def do_equivalence(self, served, session_cred, view, quant, ranks) -> dict:
        """The folded deployment's census against the all-in build's, on the same boxes."""
        targets = [float(t) for t in self.args.targets.split(",")]
        token, _ = serve_battery.authorise(
            served.session, session_cred, sorted(r["term"] for r in ranks)
        )
        total = serve_battery.viewport(
            served.viewer, token, view, 0,
            [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]], k=1
        )["counts"]["visible"]
        ladder = serve_battery.compose_ladder(ranks, total or 1, targets)
        import random as _random

        rng = _random.Random(self.args.seed)
        boxes = []
        for zoom in (3, 6, 9):
            for box in serve_battery.candidate_boxes(quant, zoom, self.args.equivalence_boxes, rng):
                boxes.append((zoom, box))
        folded = census(served.viewer, served.session, session_cred, view, quant, ladder, boxes)
        folded_frame = quant

        # The all-in deployment, served beside it on its own ports and scratch: the three ports
        # after the folded deployment's, so a cycle takes six consecutive ports from `--port0`.
        scratch = self.work / "serve-allin"
        allin = Deployment(
            self.rung,
            self.rung / "bundle",
            scratch,
            (self.args.port0 + 3, self.args.port0 + 4, self.args.port0 + 5),
            self.binary,
        )
        allin.clear_scratch()
        allin.start()
        try:
            reference_token, _ = serve_battery.authorise(
                allin.session, allin.credential("session"), sorted(r["term"] for r in ranks)
            )
            all_in_meta = serve_battery.meta(allin.viewer, reference_token)
            all_in_frame = next(
                v for v in all_in_meta["views"] if v["id"] == view
            )["quantisation"]
            reference = census(
                allin.viewer, allin.session, allin.credential("session"), view, quant, ladder, boxes
            )
        finally:
            allin.stop()
        out = compare_census(folded, reference)
        # **The frames, side by side.** Under `extent = "auto"` they differ, and that difference
        # is what a box-level disagreement of a handful of rows is; recording them is what stops
        # the next reader attributing it to the write path.
        out["frames"] = {"folded": folded_frame, "all_in": all_in_frame}
        out["frames_equal"] = folded_frame == all_in_frame
        out["ladder"] = [{"target": r["target"], "terms": r["terms"]} for r in ladder]
        out["folded"] = folded
        out["all_in"] = reference
        return out

    def do_write_cycle(self, control, served, session_cred, view, quant, all_terms, hold) -> dict:
        """1,000 deletes, 1,000 suppressions, 1,000 re-ingests, a fold, and the census again.

        Addressed by `external_id` — the source entity id, as [`external_ids`] spells it — because
        that is the address an ingested row has that survives a delete: `tessera_id`s are per
        entity and a deleted holder does not block a re-ingest of the same external id (decision
        0047, edit is delete + re-ingest).
        """
        if hold.head is None or hold.head.num_rows < 2:
            return {"skipped": "hold-out too small for a write cycle"}
        ids = [
            base64.b64encode(int(e).to_bytes(8, "little")).decode()
            for e in hold.head.column("entity_id").to_pylist()
        ]
        n = min(self.args.write_cycle_n, len(ids) // 3)
        if n == 0:
            return {"skipped": "hold-out too small for a write cycle"}
        deletes = ids[:n]
        suppressions = ids[n : 2 * n]
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]

        def visible() -> int:
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            return serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers=None
            )["counts"]["visible"]

        start_visible = visible()
        out: dict = {"n": n, "visible_before": start_visible}

        for op, batch in (("delete", deletes), ("suppress", suppressions)):
            items = [{"external_id": external, "op": op} for external in batch]
            r, wall = control.changes(items)
            expected = start_visible - len(batch) if op == "delete" else None
            reached, visibility_s = wait_for(
                lambda: visible() <= (expected if expected is not None else start_visible - len(batch)),
                timeout=120,
                interval=0.25,
            )
            out[op] = {
                "status": r.status_code,
                "body": r.text[:600] if r.status_code != 200 else None,
                "wall_s": round(wall, 3),
                "visibility_s": round(visibility_s, 3),
                "visibility_reached": reached,
                "visible_after": visible(),
            }
            start_visible = out[op]["visible_after"]

        # Re-ingest: the deleted rows, under fresh batch ids. A deleted holder never blocks a
        # re-ingest (decision 0047), so these must be accepted rather than 409'd.
        # `deletes` is `ids[:n]`, so the rows to re-ingest are the head's first `n` — the same
        # bytes, under fresh batch ids, which is what makes this a re-ingest rather than a replay.
        def head_slice():
            for start in range(0, n, BATCH_ROWS):
                chunk = hold.head.slice(start, min(BATCH_ROWS, n - start))
                yield start, hold.encode(chunk), chunk.num_rows

        out["reingest"] = self.run_ingest(control, head_slice(), "recycle")
        control.flush()
        before = control.status()["compaction"]["folds"]
        control.compact()
        done, wall = wait_for(
            lambda: control.status()["compaction"]["folds"] > before, timeout=self.args.fold_timeout, interval=1.0
        )
        compaction = control.status()["compaction"]
        out["fold"] = {
            "completed": done,
            "observed_s": round(wall, 2),
            "fold_s": compaction.get("last_secs"),
            "fold_peak_rss_bytes": compaction.get("last_rss_bytes"),
        }
        out["visible_after_cycle"] = visible()
        out["overlay"] = control.status()["overlay"]
        return out


def driver_rss() -> dict:
    """This process's `VmRSS` and `VmHWM`, in bytes, from `/proc/self/status`.

    The driver's resident set is a figure of every cell. It runs beside the server it loads, on
    the same box, and a driver that grows with the corpus stops the cell before the write path is
    measured. `peak` is the process's high-water mark and only rises.
    """
    out: dict = {}
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:")):
            out[line.split(":")[0]] = int(line.split()[1]) * 1024
    return {"rss": out.get("VmRSS"), "peak": out.get("VmHWM")}


def safe(fn):
    try:
        return fn()
    except Exception as e:
        return {"error": str(e)[:400]}


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--rung-dir", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--binary", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--fraction", type=float, required=True)
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--port0", type=int, default=8161)
    ap.add_argument("--targets", default="0.01,0.05,0.10,0.25,0.50,1.0")
    ap.add_argument("--equivalence-boxes", type=int, default=8)
    ap.add_argument("--write-cycle", action="store_true")
    ap.add_argument("--write-cycle-n", type=int, default=1000)
    ap.add_argument("--reuse-base", action="store_true")
    ap.add_argument(
        "--copy-base",
        action="store_true",
        help="serve a copy of the base bundle under the scratch instead of the base itself, so a "
        "cell that publishes does not change the base the next `--reuse-base` cell starts from",
    )
    ap.add_argument(
        "--ingest-config",
        default=None,
        help="a JSON object of `[ingest]` keys written into the served deployment's copy, for a "
        'cell that sweeps a write-path knob: `{"flush_max_age_secs": 5}`. Absent means the '
        "server's own defaults",
    )
    ap.add_argument(
        "--stop-after-ingest",
        action="store_true",
        help="return after the ingest phase and its executor laps, skipping publication, flush, "
        "the fold and the equivalence census. An attribution run, not a cycle: the result carries "
        '`stop_after: "ingest"` and no census at all',
    )
    ap.add_argument(
        "--publish-max-bytes",
        type=int,
        default=32 * 1024 * 1024,
        help="the publication byte cap, clamped to the route's own 64 MiB: a batch is split between "
        "artifacts to stay under it, and an artifact whose whole membership does not fit is published "
        "with as many members as fit and grown by PATCH in slices under it (decision 0127)",
    )
    ap.add_argument(
        "--state-extent",
        action="store_true",
        help="rewrite the base declaration's `extent = \"auto\"` as the all-in bundle's own "
        "frame — required for f = 1.0, which auto refuses, and what makes a box-level "
        "equivalence census compare like with like",
    )
    ap.add_argument(
        "--publish-bucket-rows",
        type=int,
        default=16_000_000,
        help="the partitioning publication reader's bucket budget, in member rows: a bucket is a "
        "range of the publication order holding at most this many rows, or one artifact where that "
        "artifact alone is larger. 16e6 rows is ~220 MB on disk and under 1 GB read back and sorted",
    )
    ap.add_argument("--flush-timeout", type=float, default=900.0)
    ap.add_argument("--fold-timeout", type=float, default=7200.0)
    args = ap.parse_args(argv)
    started = time.time()
    result = Cycle(args).run()
    result["ran_s"] = round(time.time() - started, 1)
    # `VmHWM` of the driver itself, whichever phase set it: the split, the hold-out's batches,
    # the publication's buckets or the census. Beside the server's own fold peak in the cell.
    result["driver_peak_rss"] = driver_rss()["peak"]
    Path(args.out).write_text(json.dumps(result, indent=2, default=str))
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
