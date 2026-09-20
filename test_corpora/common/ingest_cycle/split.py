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
import pyarrow.parquet as pq

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


def bundle_manifest(bundle: Path) -> tuple[str, dict] | None:
    """`(version prefix, manifest)` of the bundle's **current** version, or None for no bundle.

    The version is the one `CURRENT` names. Read by name, the lexicographically last `v*` is a
    version the bundle has written and may have abandoned, and its frame is then stated against a
    manifest nothing serves.
    """
    current = bundle / "CURRENT"
    if not current.is_file():
        return None
    named = json.loads(current.read_text())
    prefix = named["prefix"] if isinstance(named, dict) else str(named)
    return prefix, json.loads((bundle / prefix / "MANIFEST.json").read_text())


def state_extent(corpus_toml: Path, bundle: Path) -> dict | None:
    """Rewrite each `[[view]]`'s `extent = "auto"` as **that view's own** frame in the all-in bundle.

    Two things follow from `auto`, and both are properties of the *frame* rather than of ingest:

    * **A zero-row build is refused.** `auto` fits a box around the data, and there is no box
      around no rows — the refusal says so and names this remedy. So the *f* = 100% cell cannot
      run at all under `auto`, and a deployment starting empty must state its frame.
    * **A complement build quantises onto a different grid.** The frame is fitted to the rows the
      build saw, so a base built from 90% of the corpus has a slightly smaller box, and the two
      deployments' cells do not line up. Every box-level count then differs at the margins for a
      reason that has nothing to do with the write path.

    Stating the all-in frame removes both. Each view's frame is fitted to its own layout, so a
    declaration carrying several views takes several frames: one frame written over both puts one
    route's points in a corner of the other's grid, which quantisation clamps rather than refuses.

    It is a change to the *declaration the measurement builds from*, never to the rung's committed
    one, and the run records what was stated.
    """
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
    """The rung's `corpus.toml` as the base's: every layer stated, and only a column-route layer's
    member table kept.

    A `[[layer]]` block says two kinds of thing. Its declaration — kind, levels, views, the three
    disclosure controls, the content kinds — is what a running deployment holds and what
    `PUT /control/layers` takes. Its `source` and `[layer.members]` are *acquisition*: where the
    rows come from, which is build-only and is the half decision 0091 excludes from the rule that
    the two entry points say the same things (`configuration.md` §2). Removing that half leaves a
    layer that exists, is empty, and can be published into.

    `keep_members` names the layers whose `[layer.members]` stays: the ones whose member table is
    per-point, which the build reads over the base's rows and whose hold-out rows carry the same
    list on the wire (the module doc). `keep_sources` names the layers whose roster stays, which
    are those of them that declare supplied content: an artifact of such a layer cannot be minted
    from a key alone, at either entry point, so the base build takes the roster the all-in build
    took. Every other layer's `source` is removed, a roster being the publication route's input.

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
        if section == "[[layer]]" and head.startswith("source") and "=" in head:
            if layer is None or layer["layer"] not in keep_sources:
                if layer is not None:
                    layer["removed"].append(head)
                continue
            layer["kept"] = f"{layer.get('kept', '')} {head}".strip()
        out.append(line)
    return "".join(out), removed


def declared_layers(rung: Path) -> list[dict]:
    """Every `[[layer]]` of the rung's declaration, with the files its acquisition names.

    One record per layer, in declaration order: `name`; `roster`, the path the layer's own `source`
    names, or None where the layer declares none (an open value set mints its artifacts from the
    member rows); `members`, the path `[layer.members] source` names, or None; `attribute`, the
    column an attribute-membership layer is drawn from, or None; `supplied`, whether the layer
    declares supplied content; and `route`, how its membership reaches the folded deployment
    (the module doc): `attribute` for a predicate layer, `column` for a layer whose member table is
    **per point** — the build reads it over the base's rows and the hold-out's rows carry the same
    lists on the wire — and `publication` for one whose member table is per (artifact, entity),
    which is read artifact by artifact and sent with the artifact. A `source` is a key into
    `[sources]` or a file name, as `Config::layer_sources` reads it.

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
            }
        )
    return out


def per_point_member_table(path: Path) -> bool:
    """Whether a member table names **a point's artifacts in a list column** — one row per point,
    one entry per declared level — rather than one row per (artifact, entity).

    The two shapes are two routes. A list column is what the build reads beside the points file and
    what an ingest batch carries as the column named for the layer, so such a membership travels
    with the rows that hold it. A `key` and `rank` per row addresses artifacts, and its rows are
    read artifact by artifact and sent with the artifact they belong to.
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
    """`tessera build` into `out`, and what it cost.

    `peak_rss_kib` is the largest stage's own high-water, which the build reports per stage. The
    driver's `getrusage(RUSAGE_CHILDREN)` is a high-water over every child it has reaped — a
    `cargo build` among them — and cannot be attributed to this build.
    """
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
    points under the name the declaration gives it, so the base build reads exactly the membership
    the base's rows name; the hold-out's rows name theirs on the wire. Such a layer's roster, where
    it has one, is copied whole: an artifact of a layer declaring supplied content is never minted
    from a key, so the keys must come from the roster, and a key no base row names is an artifact
    with no members at the base and its membership arrives with the hold-out.

    `corpus.toml` is rewritten by [`base_declaration`], which removes the publication route's
    rosters and keeps a column-route layer's `[layer.members]` and roster.
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
        # **A roster beside a per-point member table stays**, whole: the layer declares supplied
        # content, so its artifacts cannot be minted from the member column at either entry point,
        # and the base build takes the roster the all-in build took.
        if layer["roster"] is not None and layer["roster"].exists():
            shutil.copy2(layer["roster"], out / layer["roster"].name)
            kept.setdefault("rosters", []).append(layer["roster"].name)
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
        (rung / "corpus.toml").read_text(),
        keep_members=[layer["name"] for layer in on_column],
        keep_sources=[layer["name"] for layer in on_column if layer["roster"] is not None],
    )
    (out / "corpus.toml").write_text(declaration)
    kept["declaration_only"] = removed
    (out / "tessera.toml").write_text((rung / "tessera.toml").read_text())
    return kept
