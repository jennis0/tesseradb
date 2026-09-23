from __future__ import annotations

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
import json
import shutil
import subprocess
import time
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

# ---------------------------------------------------------------------------------------------
# The split
# ---------------------------------------------------------------------------------------------


def declared_entities(rung: Path) -> np.ndarray:
    """Every entity id the rung's views hold: the anchor view's in file order, then each other
    view's ids the ones before it did not hold."""
    parts: list[np.ndarray] = []
    seen: np.ndarray | None = None
    files: dict[Path, str] = {}
    for view in declared_views(rung):
        files.setdefault(view["points"], view["fields"]["entity_id"])
    for path, column in files.items():
        ids = pq.read_table(path, columns=[column]).column(column).to_numpy()
        # Every file's ids in the first file's integer type: mixing int64 and uint64 in numpy
        # promotes to float64, which cannot hold an id above 2**53.
        if seen is None:
            seen = np.zeros(0, ids.dtype)
        ids = ids.astype(seen.dtype, copy=False)
        fresh = ids[~in_sorted(ids, seen)]
        fresh = fresh[np.sort(np.unique(fresh, return_index=True)[1])]
        parts.append(fresh)
        seen = np.sort(np.concatenate([seen, fresh]))
    return np.concatenate(parts) if parts else np.zeros(0, np.uint64)


def split_entities(ids: np.ndarray, fraction: float, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """`(base entity ids, hold-out entity ids)` — a seeded uniform hold-out of `fraction`, over
    entities rather than rows, which differ once a rung has several views.
    """
    rng = np.random.default_rng(seed)
    order = rng.permutation(len(ids))
    cut = int(round(fraction * len(ids)))
    held = np.sort(ids[order[:cut]])
    base = np.sort(ids[order[cut:]])
    return base, held


def in_sorted(values: np.ndarray, sorted_ids: np.ndarray) -> np.ndarray:
    """Membership of `values` in `sorted_ids`, by binary search against a sorted array rather
    than `np.isin`, which builds an intermediate the size of both inputs.
    """
    if len(sorted_ids) == 0:
        # At f = 1.0 nothing is kept for the base: an empty `keep` is a real case, a build over
        # a zero-row points file.
        return np.zeros(len(values), dtype=bool)
    idx = np.searchsorted(sorted_ids, values)
    idx[idx >= len(sorted_ids)] = 0
    return sorted_ids[idx] == values


def filter_parquet(
    source: Path, out: Path, column: str, keep: np.ndarray, drop: Sequence[str] = ()
) -> int:
    """Copy `source` to `out`, keeping rows whose `column` is in `keep`, a row group at a time
    through `read_row_group`, for the reason `HoldOut.batches` gives, and under the source's own
    compression rather than `ParquetWriter`'s default. `drop` names columns to leave behind.
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
    """The rung's principal ladder — `[{"term": ..., "pairs": ...}, ...]`, richest term first,
    read from whichever `<axis>-ranks.json` is present.
    """
    named = rung / "branch-ranks.json"
    if named.exists():
        return named
    candidates = sorted(rung.glob("*-ranks.json"))
    if not candidates:
        raise FileNotFoundError(f"no <axis>-ranks.json in {rung}")
    return candidates[0]


def ranks_for(rung: Path, work: Path) -> tuple[Path, dict]:
    """The rung's ranks file, or one derived into `work` where the rung has none, and a record
    of which it was."""
    try:
        found = ranks_file(rung)
    except FileNotFoundError:
        work.mkdir(parents=True, exist_ok=True)
        out = work / "derived-ranks.json"
        return out, {"file": str(out), "derived": True, **derive_ranks(rung, out)}
    return found, {"file": str(found), "derived": False}


def derive_ranks(rung: Path, out: Path) -> dict:
    """Write a ranks file counted from the data: each view's `point_visibility` field, one pair
    per (entity, label) across every view's points, a missing label counted under the view's
    declared default."""
    pairs = []
    fields = set()
    for view in declared_views(rung):
        field = view["point_visibility"].get("field")
        default = view["point_visibility"].get("default")
        if field is None:
            continue
        fields.add(field)
        table = read_view_rows(view, ["entity_id", field])
        labels = table.column(field).combine_chunks()
        entities = table.column("entity_id").combine_chunks()
        if pa.types.is_list(labels.type) or pa.types.is_large_list(labels.type):
            unlabelled = entities.filter(pc.equal(pc.fill_null(pc.list_value_length(labels), 0), 0))
            entities = pc.take(entities, pc.list_parent_indices(labels))
            labels = pc.list_flatten(labels).cast(pa.string())
        else:
            labels = labels.cast(pa.string())
            unlabelled = entities.filter(pc.is_null(labels))
        pairs.append(pa.table({"entity": entities, "term": labels}).filter(pc.is_valid(labels)))
        if isinstance(default, str) and len(unlabelled):
            filled = pa.array([default] * len(unlabelled), pa.string())
            pairs.append(pa.table({"entity": unlabelled, "term": filled}))
    if not pairs:
        raise ValueError(f"{rung}: no view declares a point_visibility field to count terms from")
    distinct = pa.concat_tables(pairs).group_by(["entity", "term"]).aggregate([])
    counted = distinct.group_by("term").aggregate([("entity", "count")])
    terms = counted.column("term").to_pylist()
    counts = counted.column("entity_count").to_pylist()
    ranks = sorted(
        ({"term": term, "pairs": int(n)} for term, n in zip(terms, counts)),
        key=lambda rank: (-rank["pairs"], rank["term"]),
    )
    out.write_text(json.dumps(ranks))
    return {"fields": sorted(fields), "terms": len(ranks)}


def declared_views(rung: Path) -> list[dict]:
    """Every view the declaration names, the allocation view first: a plain view by its name and
    a group's view as `group:key`, the id the server gives it. `points` is the file its rows are
    read from, `select` the `(column, key)` picking them out of a file a group's views share, and
    `fields` each canonical column name (`entity_id`, the coordinate pair, `view`) as that file
    spells it. `record` is a group view's roster record under canonical names, on the group that
    owns the keys, and `metadata` the names that group declares."""
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})
    defaults = declared.get("defaults", {})
    entity = defaults.get("entity_id_field", "entity_id")
    views = [
        {
            "id": view["name"],
            "group": None,
            "owner": None,
            "key": None,
            "points": source_path(rung, named, view.get("source", defaults.get("source"))),
            "select": None,
            "projection": view.get("projection", "none"),
            "fields": view_fields(view, entity),
            "point_visibility": view.get("point_visibility") or {},
            "record": None,
            "metadata": [],
        }
        for view in declared.get("view", [])
    ]
    groups = {group["name"]: group for group in declared.get("view_group", [])}
    for group in groups.values():
        owner = groups.get(group.get("members"), group)
        fields = view_fields(group, entity)
        # A group with its own `source` holds every view's rows in that file, picked out by its
        # discriminator; otherwise each view's rows are the file its roster entry names.
        shared = source_path(rung, named, group.get("source"))
        for record, own in roster(rung, named, owner, entity):
            views.append(
                {
                    "id": f"{group['name']}:{record['key']}",
                    "group": group["name"],
                    "owner": owner["name"],
                    "key": record["key"],
                    "points": shared or source_path(rung, named, own),
                    "select": (fields["view"], record["key"]) if shared else None,
                    "projection": group.get("projection", "none"),
                    "fields": fields,
                    "point_visibility": group.get("point_visibility") or {},
                    "record": record,
                    "metadata": list(owner.get("metadata") or {}),
                }
            )
    anchor = defaults.get("allocation_view") or declared.get("allocation_view")
    anchor = anchor or (views[0]["id"] if views else None)
    views.sort(key=lambda view: view["id"] != anchor)
    return views


def view_fields(block: dict, entity: str) -> dict[str, str]:
    """A view's or a group's canonical column names mapped to its file's: `entity_id`, `x`/`y`
    for a view with no projection or `lon`/`lat` for a projected one, and the discriminator
    `view`, each its own name unless `fields` renames it."""
    projected = block.get("projection", "none") != "none"
    renamed = block.get("fields") or {}
    canonical = ["entity_id", *(("lon", "lat") if projected else ("x", "y")), "view"]
    return {name: renamed.get(name, entity if name == "entity_id" else name) for name in canonical}


def roster(rung: Path, named: dict, owner: dict, entity: str) -> list[tuple[dict, str | None]]:
    """A group's views as `(roster record, the view's own source)`, in the build's three forms:
    `[[view_group.view]]` blocks, a `[view_group.views]` table, or keys minted from the distinct
    values of the group's discriminator, in key order."""
    if owner.get("view"):
        return [
            ({key: value for key, value in entry.items() if key != "source"}, entry.get("source"))
            for entry in owner["view"]
        ]
    table = owner.get("views")
    if table is not None:
        renamed = table.get("fields") or {}
        names = ["key", "visibility", *(owner.get("metadata") or {})]
        read = pq.read_table(source_path(rung, named, table["source"]))
        columns = {name: renamed.get(name, name) for name in names}
        rows = read.select([c for c in columns.values() if c in read.column_names]).to_pylist()
        return [
            (
                {
                    name: row[column]
                    for name, column in columns.items()
                    if row.get(column) is not None
                },
                None,
            )
            for row in rows
        ]
    source = source_path(rung, named, owner.get("source"))
    if source is None:
        return []
    column = view_fields(owner, entity)["view"]
    keys = pq.read_table(source, columns=[column]).column(column).unique().drop_null()
    return [({"key": key}, None) for key in sorted(keys.to_pylist())]


def read_view_rows(
    view: dict, columns: Sequence[str], keep: np.ndarray | None = None, limit: int | None = None
) -> pa.Table:
    """`columns` of one view's rows under their canonical names, picked out of a shared file by
    its discriminator, kept to the sorted entity ids `keep` where given, and stopping once `limit`
    rows are held."""
    fields = view.get("fields") or {}
    spelt = {name: fields.get(name, name) for name in ["entity_id", *columns]}
    select = view["select"]
    wanted = list(dict.fromkeys([*spelt.values(), *([select[0]] if select else [])]))
    reader = pq.ParquetFile(view["points"])
    parts: list[pa.Table] = []
    held = 0
    for index in range(reader.metadata.num_row_groups):
        table = reader.read_row_group(index, columns=wanted)
        if select is not None:
            table = table.filter(pc.equal(table.column(select[0]), select[1]))
        if keep is not None:
            table = table.filter(pa.array(in_sorted(table.column(spelt["entity_id"]).to_numpy(), keep)))
        parts.append(table)
        held += table.num_rows
        if limit is not None and held >= limit:
            break
    table = pa.concat_tables(parts) if parts else reader.schema_arrow.empty_table().select(wanted)
    if limit is not None:
        table = table.slice(0, limit)
    return pa.table({name: table.column(spelt[name]) for name in columns})


def bundle_manifest(bundle: Path) -> tuple[str, dict] | None:
    """`(version prefix, manifest)` of the bundle's current version, or None for no bundle: the
    version `CURRENT` names, not the lexicographically last `v*`, which can be abandoned.
    """
    current = bundle / "CURRENT"
    if not current.is_file():
        return None
    named = json.loads(current.read_text())
    prefix = named["prefix"] if isinstance(named, dict) else str(named)
    return prefix, json.loads((bundle / prefix / "MANIFEST.json").read_text())


def state_extent(corpus_toml: Path, bundle: Path) -> dict | None:
    """Rewrite each `[[view]]`'s `extent = "auto"` as that view's own frame in the all-in bundle:
    `auto` refuses a zero-row build and quantises a complement build onto a slightly different
    grid. Changes the declaration the measurement builds from, never the rung's committed one."""
    read = bundle_manifest(bundle)
    if read is None:
        return None
    prefix, meta = read
    frames = {view["id"]: view["quantisation"] for view in meta["views"]}
    lines = corpus_toml.read_text().splitlines(keepends=True)
    heads = [i for i, line in enumerate(lines) if line.lstrip().startswith("[")]
    stated: dict[str, dict] = {}
    for n, start in enumerate(heads):
        if lines[start].strip() != "[[view]]":
            continue
        end = heads[n + 1] if n + 1 < len(heads) else len(lines)
        name = None
        at = None
        for i in range(start, end):
            head = lines[i].strip()
            if name is None and head.startswith("name") and "=" in head:
                name = head.split("=", 1)[1].strip().strip('"')
            if head.startswith("extent") and "=" in head:
                at = i
        if name is None or at is None or lines[at].split("=", 1)[1].strip() != '"auto"':
            continue
        if name not in frames:
            raise ValueError(
                f"the all-in bundle at {bundle} declares no view named {name!r}; state "
                f"--all-in-bundle as the bundle this rung's declaration was built into"
            )
        q = frames[name]
        pad = lines[at].split("=", 1)[0]
        lines[at] = (
            f'{pad}= {{ x = [{q["x_min"]!r}, {q["x_max"]!r}], '
            f'y = [{q["y_min"]!r}, {q["y_max"]!r}] }}\n'
        )
        stated[name] = q
    if not stated:
        return None
    corpus_toml.write_text("".join(lines))
    return {"views": stated, "from_version": prefix}


def base_declaration(
    text: str, keep_members: Sequence[str] = (), keep_sources: Sequence[str] = ()
) -> tuple[str, list[dict]]:
    """The rung's `corpus.toml` as the base's: every layer stated, and only a column-route
    layer's member table kept. A `[[layer]]`'s `source`, its `fields` and `[layer.members]` are
    build-only acquisition, removed except where `keep_members` or `keep_sources` names the layer. Returns
    the rewritten text and one record per layer, saying what was removed."""
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
                layer["kept"] = f"{layer.get('kept', '')} [layer.members]".strip()
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
        # A layer's `fields` map names columns of the file its `source` names, so it goes with it.
        if section == "[[layer]]" and head.startswith(("source", "fields")) and "=" in head:
            if layer is None or layer["layer"] not in keep_sources:
                if layer is not None:
                    layer["removed"].append(head)
                continue
            layer["kept"] = f"{layer.get('kept', '')} {head}".strip()
        out.append(line)
    return "".join(out), removed


def source_path(rung: Path, named: dict, key: str | None) -> Path | None:
    """The file a `source` names: a key into `[sources]` or a file name, as `Config::layer_sources`
    reads it. None for no source at all."""
    return None if key is None else rung / named.get(key, key)


def declared_layers(rung: Path) -> list[dict]:
    """Every `[[layer]]` of the rung's declaration, with the files its acquisition names and its
    `route`: `attribute` for a predicate layer, `column` for a per-point member table,
    `publication` for a per (artifact, entity) one.
    """
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})
    out = []
    for layer in declared.get("layer", []):
        membership = layer.get("membership")
        attribute = membership.get("attribute") if isinstance(membership, dict) else None
        roster = source_path(rung, named, layer.get("source"))
        members = source_path(rung, named, (layer.get("members") or {}).get("source"))
        supplied = bool((layer.get("content") or {}).get("supplied"))
        scope = layer.get("scope")
        if attribute is not None:
            route = "attribute"
        elif members is not None and members.exists() and per_point_member_table(members):
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
                "views": layer.get("views") or [],
                # The group a group-scoped layer's artifact sets vary by, and the roster column
                # naming each artifact's view.
                "scope_group": scope.get("group") if isinstance(scope, dict) else None,
                "view_column": (layer.get("fields") or {}).get("view", "view")
                if isinstance(scope, dict)
                else None,
                "inline": bool(layer.get("artifacts")),
            }
        )
    return out


def per_point_member_table(path: Path) -> bool:
    """Whether a member table names a point's artifacts in a list column — one row per point, one
    entry per declared level — rather than one row per (artifact, entity), which is read artifact
    by artifact and sent with the artifact it belongs to.
    """
    key = pq.ParquetFile(path).schema_arrow.field("key")
    return pa.types.is_list(key.type) or pa.types.is_large_list(key.type)


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


def build_bundle(
    binary: Path,
    cwd: Path,
    out: Path,
    stages_json: Path,
    extra: Sequence[str] = (),
    env: dict[str, str] | None = None,
) -> dict:
    """`tessera build` into `out`, and what it cost. `peak_rss_kib` is the largest stage's own
    high-water, as the build reports it, rather than the driver's `getrusage`, a high-water over
    every child it has reaped."""
    t0 = time.perf_counter()
    proc = subprocess.run(
        [
            str(binary),
            "build",
            *extra,
            "--out",
            str(out),
            "--stage-timings-json",
            str(stages_json),
            "--stage-timings",
        ],
        cwd=cwd,
        capture_output=True,
        text=True,
        env=env,
    )
    wall = time.perf_counter() - t0
    stages = json.loads(stages_json.read_text()) if stages_json.exists() else None
    return {
        "wall_s": round(wall, 2),
        "returncode": proc.returncode,
        "stdout": proc.stdout[-4000:],
        "stderr_tail": proc.stderr[-4000:],
        "stages": stages,
        "peak_rss_kib": max((s["peak_rss_kib"] for s in stages or []), default=None),
        "bundle_bytes": sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
        if out.exists()
        else 0,
    }


def write_base_inputs(rung: Path, out: Path, base_ids: np.ndarray) -> dict:
    """The complement's inputs: every file a view or an attribute reads, over the base's
    entities, the declaration, and a column-route layer's member table over the base's rows.
    Nothing else: a publication-route layer's artifacts are published on the wire after the
    points they depend on have been ingested. `corpus.toml` is rewritten by
    [`base_declaration`]."""
    out.mkdir(parents=True, exist_ok=True)
    layers = declared_layers(rung)
    published = [layer["name"] for layer in layers if layer["route"] == "publication"]
    on_column = [layer for layer in layers if layer["route"] == "column"]
    kept: dict = {"entities": len(base_ids)}
    kept["member_tables"], rosters = write_base_members(out, on_column, base_ids)
    if rosters:
        kept["rosters"] = rosters
    kept.update(copy_declared_inputs(rung, out, base_ids, published))
    declaration, removed = base_declaration(
        (rung / "corpus.toml").read_text(),
        keep_members=[layer["name"] for layer in on_column],
        keep_sources=[layer["name"] for layer in on_column if layer["roster"] is not None],
    )
    (out / "corpus.toml").write_text(declaration)
    kept["declaration_only"] = removed
    (out / "tessera.toml").write_text((rung / "tessera.toml").read_text())
    (out / "base-inputs.json").write_text(json.dumps(kept))
    return kept


def write_base_members(
    out: Path, on_column: Sequence[dict], base_ids: np.ndarray
) -> tuple[dict, list[str]]:
    """Each column-route layer's member table, filtered to the base's rows and written under the
    name the declaration gives it, and its roster, where the layer declares supplied content,
    copied beside it whole.
    """
    tables: dict = {}
    rosters: list[str] = []
    for layer in on_column:
        members = layer["members"]
        entity, _ = member_table_columns(pq.ParquetFile(members).schema_arrow)
        tables[layer["name"]] = {
            "file": members.name,
            "rows": filter_parquet(members, out / members.name, entity, base_ids),
        }
        if layer["roster"] is not None and layer["roster"].exists():
            shutil.copy2(layer["roster"], out / layer["roster"].name)
            rosters.append(layer["roster"].name)
    return tables, rosters


def copy_declared_inputs(
    rung: Path, out: Path, base_ids: np.ndarray, published: Sequence[str]
) -> dict:
    """Every other file the declaration still names, read off the declaration rather than listed
    here: a vocabulary is copied whole, and every file a view or an attribute reads is filtered
    to the base's entities, leaving out a publication-route layer's column.
    """
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    named = declared.get("sources", {})
    kept: dict = {}
    for vocabulary in declared.get("vocabulary", []):
        got = source_path(rung, named, vocabulary.get("source"))
        if got is not None and got.exists():
            shutil.copy2(got, out / got.name)
            kept.setdefault("vocabularies", []).append(got.name)
    entity = declared.get("defaults", {}).get("entity_id_field", "entity_id")
    entity_files: dict[Path, str] = {}
    for view in declared_views(rung):
        entity_files.setdefault(view["points"], view["fields"]["entity_id"])
    for attribute in declared.get("attribute", []):
        got = source_path(rung, named, attribute.get("source"))
        if got is not None:
            entity_files.setdefault(got, (attribute.get("fields") or {}).get("entity_id", entity))
    for got, column in entity_files.items():
        dropped = [name for name in pq.ParquetFile(got).schema_arrow.names if name in set(published)]
        kept.setdefault("entity_files", {})[got.name] = {
            "rows": filter_parquet(got, out / got.name, column, base_ids, drop=published),
            "dropped_membership_columns": dropped,
        }
    for name in ("branch.parquet", ".env"):
        source = rung / name
        if source.exists() and not (out / name).exists():
            shutil.copy2(source, out / name)
    return kept
