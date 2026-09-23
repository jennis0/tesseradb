from __future__ import annotations

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib
import subprocess
from collections import Counter
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import requests
from pyarrow import ipc

from .. import serve_battery
from .holdout import wire_columns
from .split import read_view_rows

#: A view's rows read to draw a probe's values from.
PROBE_SAMPLE_ROWS = 200_000

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
    probes: Sequence[dict] = (),
) -> dict:
    """Masked counts per principal and per layer: zoom 0 over the whole extent, then each box.

    Every request carries `layers: "all"`, so each box compares the artifacts and the parent links
    at the levels that box's zoom serves — a tiered layer answers only the levels whose declared
    zoom range covers the request's zoom, and zoom 0 reaches its coarsest level alone.

    `probes` are asked under the broadest and the narrowest principal: a filter's matched count
    over the whole extent, or a category column's value list in this view.
    """
    full = serve_battery.full_box(quant)
    targets = [rung["target"] for rung in ladder]
    probed = {max(targets), min(targets)} if targets else set()
    out: dict = {}
    for rung in ladder:
        token, _ = serve_battery.authorise(session_base, cred, rung["terms"])
        incomplete: list[str] = []
        whole = layered(viewer, token, view, 0, full, "zoom 0 over the whole extent", incomplete)
        row = {
            "terms": rung["terms"],
            "zoom0_visible": (whole["counts"] or {}).get("visible"),
            "boxes": [],
            "layers": artifact_frame_census(whole["body"]),
            "parents": artifact_frame_parents(whole["body"]),
            "incomplete": incomplete,
        }
        for i, (zoom, box) in enumerate(boxes):
            s = layered(viewer, token, view, zoom, box, f"box {i} zoom {zoom}", incomplete)
            row["boxes"].append(
                {
                    "zoom": zoom,
                    "box": box,
                    "visible": (s["counts"] or {}).get("visible"),
                    "layers": artifact_frame_census(s["body"]),
                    "parents": artifact_frame_parents(s["body"]),
                }
            )
        if rung["target"] in probed:
            row["filters"], row["categories"] = ask_probes(viewer, token, view, full, probes, incomplete)
        out[f"{rung['target']:.4f}"] = row
    return out


def ask_probes(
    viewer: str, token: str, view: str, full: list[float], probes: Sequence[dict], incomplete: list[str]
) -> tuple[dict, dict]:
    """Each probe's answer: a filter's matched count at zoom 0, or a category column's value keys
    as `/v1/categories` lists them for this view."""
    filters: dict = {}
    categories: dict = {}
    for probe in probes:
        where = f"probe {probe['name']}"
        if "filters" in probe:
            try:
                s = serve_battery.viewport(
                    viewer, token, view, 0, full, k=0, filters=probe["filters"], layers=None
                )
            except requests.exceptions.RequestException as e:
                incomplete.append(f"the census request at {where} was not answered: {e}")
                continue
            if s["shed"] or s["counts"] is None:
                incomplete.append(
                    f"the census request at {where} did not arrive whole: "
                    f"{s['shed_error'] or 'no counts frame or no trailer'}"
                )
                continue
            filters[probe["name"]] = s["counts"].get("matched")
        else:
            r = requests.get(
                f"{viewer}/v1/categories/{probe['categories']}",
                headers={"Authorization": f"Bearer {token}"},
                params={"view": view},
                timeout=60,
            )
            if r.status_code != 200:
                incomplete.append(f"the census request at {where} answered {r.status_code}: {r.text[:200]}")
                continue
            categories[probe["categories"]] = sorted(value["key"] for value in r.json()["values"])
    return filters, categories


def filter_probes(
    rung: Path, views: Sequence[dict], view: dict, meta: dict, binary: Path
) -> tuple[list[dict], int]:
    """What a view's census asks beyond counts and layers, and how many filter operands `/v1/meta`
    offers on the view. For each operand, a filter or two whose values are drawn from the rows the
    view's batches take the column from, and a category column's value list. A group-scoped column
    is read from a view holding the same key and an entity-scoped one from any view carrying it."""
    scoped = {family["name"]: set(family["views"]) for family in meta.get("scoped_scalars") or []}
    declared = tomllib.loads((rung / "corpus.toml").read_text())
    analysers = {a["name"]: a.get("analyser") for a in declared.get("attribute", [])}
    probes: list[dict] = []
    offered = 0
    for operand in meta.get("filter_operands") or []:
        column = operand["column"]
        if operand.get("scope"):
            if view["id"] not in scoped.get(column, ()):
                continue
            candidates = [v for v in views if (v["owner"], v["key"]) == (view["owner"], view["key"])]
        else:
            candidates = list(views)
        offered += 1
        candidates.sort(key=lambda v: v["id"] != view["id"])
        values = column_sample(rung, candidates, column)
        if values is not None:
            probes += family_probes(
                column, operand["family"], operand["operands"], values, binary, analysers.get(column)
            )
    return probes, offered


def column_sample(rung: Path, candidates: Sequence[dict], column: str) -> pa.Array | None:
    """Up to `PROBE_SAMPLE_ROWS` of the view's own rows' non-null values of `column`, from the
    first of `candidates` whose batches carry it: its points file, or the file the column is
    joined from."""
    for view in candidates:
        _, attributes, joined = wire_columns(rung, view)
        if column not in attributes:
            continue
        source = next(
            ({"points": j["file"], "select": j["select"]} for j in joined if column in j["columns"]),
            view,
        )
        rows = read_view_rows(source, [column], limit=PROBE_SAMPLE_ROWS)
        return rows.column(column).combine_chunks().drop_null()
    return None


def family_probes(
    column: str,
    family: str,
    operators: Sequence[str],
    values: pa.Array,
    binary: Path,
    analyser: str | None,
) -> list[dict]:
    """A numeric column's presence and upper half, a category or keyword column's three commonest
    values and a category's value list, and a text column's two commonest words."""
    if len(values) == 0:
        return []
    if family == "numeric" and "range" in operators:
        if pa.types.is_timestamp(values.type):
            values = values.cast(pa.timestamp("us")).cast(pa.int64())
        numbers = np.sort(values.to_numpy(zero_copy_only=False))
        return [
            {"name": f"{column} >= {bound}", "filters": {column: {"range": {"gte": bound.item()}}}}
            for bound in (numbers[0], numbers[len(numbers) // 2])
        ]
    if family in ("category", "keyword") and "eq" in operators:
        counted = sorted(
            pc.value_counts(values).to_pylist(), key=lambda entry: (-entry["counts"], str(entry["values"]))
        )
        probes = [
            {"name": f"{column} = {entry['values']}", "filters": {column: {"eq": entry["values"]}}}
            for entry in counted[:3]
        ]
        if family == "category":
            probes.append({"name": f"{column} values", "categories": column})
        return probes
    if family == "text" and "match" in operators:
        words = Counter(
            token
            for token in tokens(binary, analyser, values.slice(0, 2000).to_pylist())
            if len(token) >= 4 and token.isalpha()
        )
        return [
            {"name": f"{column} matches {word}", "filters": {column: {"match": word}}}
            for word, _ in sorted(words.items(), key=lambda item: (-item[1], item[0]))[:2]
        ]
    return []


def tokens(binary: Path, analyser: str | None, texts: Sequence) -> list[str]:
    """Every token of `texts` as the column's own analyser produces it, through `tessera
    tokenise`, so a probe's word is a term the index holds. Words of four letters or more are
    kept by the caller, which leaves out the short and numeric tokens every text has."""
    lines = "".join(" ".join(str(text).split()) + "\n" for text in texts)
    proc = subprocess.run(
        [str(binary), "tokenise", *(["--analyser", analyser] if analyser else [])],
        input=lines,
        capture_output=True,
        text=True,
        check=True,
    )
    return [token for line in proc.stdout.splitlines() for token in line.split("\t") if token]


def layered(
    viewer: str,
    token: str,
    view: str,
    zoom: int,
    box: Sequence[float],
    where: str,
    incomplete: list[str],
) -> dict:
    """One `layers: "all"` viewport whose frames the census reads. A response the server shed, cut
    or refused leaves a sentence in `incomplete` and is read as far as it arrived."""
    try:
        s = serve_battery.viewport(
            viewer, token, view, zoom, box, k=1, layers="all", keep_body=True
        )
    except requests.exceptions.RequestException as e:
        incomplete.append(f"the census request at {where} was not answered: {type(e).__name__}: {e}")
        return {"counts": None, "body": b""}
    if s["shed"]:
        incomplete.append(
            f"the census request at {where} did not arrive whole: "
            f"{s['shed_error'] or 'no trailer'}"
        )
    return s


def census_zooms(ranges: Sequence[Sequence[int]]) -> list[int]:
    """The zooms a census asks its boxes at: 3, 6 and 9, and the lower bound of every declared
    level range that none of them falls in, so each level of a tiered layer is served at one of
    them. A rung whose layers declare no range keeps 3, 6, 9 alone."""
    chosen = {3, 6, 9}
    for lo, hi in sorted((int(pair[0]), int(pair[1])) for pair in ranges):
        if not any(lo <= zoom <= hi for zoom in chosen):
            chosen.add(lo)
    return sorted(chosen)


def artifact_frame_rows(content: bytes):
    """Every kind-5 artifact frame of a viewport response, as Arrow tables: kind 5 is one row
    per served artifact.
    """
    offset = 0
    while offset + 5 <= len(content):
        kind = content[offset]
        length = int.from_bytes(content[offset + 1 : offset + 5], "little")
        payload = content[offset + 5 : offset + 5 + length]
        offset += 5 + length
        if kind == 5 and payload:
            yield ipc.open_stream(payload).read_all()


def artifact_frame_census(content: bytes) -> dict:
    """The served artifacts per layer: how many, and their summed masked count. Two numbers, not
    one, since an artifact count alone passes a defect that serves the right artifacts with the
    wrong memberships. `levels` is the same two numbers by the `rung` each row was served at — the
    declared level on a levelled layer, the response-local depth on a treed one.
    """
    out: dict = {}
    for table in artifact_frame_rows(content):
        layers = table.column("layer").to_pylist()
        counts = table.column("masked_count").to_pylist()
        rungs = table.column("rung").to_pylist()
        for layer, count, rung in zip(layers, counts, rungs):
            entry = out.setdefault(layer, {"artifacts": 0, "masked_count": 0, "levels": {}})
            level = entry["levels"].setdefault(str(rung), {"artifacts": 0, "masked_count": 0})
            for at in (entry, level):
                at["artifacts"] += 1
                at["masked_count"] += int(count or 0)
    return out


def artifact_frame_parents(content: bytes) -> dict:
    """The served parent links per layer, as sorted `[child, parent]` pairs of artifact keys,
    read back through the frame's `tessera_id -> key` map since a `tessera_id` is specific to
    this deployment. A parent is named only where it is a row of the same frame, so the map
    resolves every link, and a level served without the level above it carries none.
    """
    out: dict = {}
    for table in artifact_frame_rows(content):
        names = table.schema.names
        keys = table.column("key").to_pylist() if "key" in names else [None] * table.num_rows
        ids = table.column("tessera_id").to_pylist()
        parents = table.column("parent_ids").to_pylist() if "parent_ids" in names else None
        layers = table.column("layer").to_pylist()
        named = {i: key if key is not None else str(i) for i, key in zip(ids, keys)}
        for layer, own, parent_ids in zip(layers, ids, parents or [[]] * table.num_rows):
            entry = out.setdefault(layer, [])
            for parent in parent_ids or []:
                entry.append([named.get(own, str(own)), named.get(parent, str(parent))])
    return {layer: sorted(pairs) for layer, pairs in out.items()}


def layer_differences(folded: dict, all_in: dict) -> list[dict]:
    """One entry per layer whose served artifacts differ, by count, by summed masked count or by
    the breakdown across levels. One per layer, not one per artifact."""
    out = []
    for layer in sorted(set(folded) | set(all_in)):
        if folded.get(layer) != all_in.get(layer):
            out.append(
                {
                    "where": "layers",
                    "layer": layer,
                    "folded": folded.get(layer),
                    "all_in": all_in.get(layer),
                }
            )
    return out


def parent_differences(folded: dict, all_in: dict) -> list[dict]:
    """One entry per layer whose served parent links differ, naming the pairs only one side has."""
    out = []
    for layer in sorted(set(folded) | set(all_in)):
        mine = {tuple(pair) for pair in folded.get(layer) or []}
        theirs = {tuple(pair) for pair in all_in.get(layer) or []}
        if mine != theirs:
            out.append(
                {
                    "where": "parents",
                    "layer": layer,
                    "folded_edges": len(mine),
                    "all_in_edges": len(theirs),
                    # Capped, so a large layer does not write its whole hierarchy twice.
                    "only_folded": sorted(mine - theirs)[:20],
                    "only_all_in": sorted(theirs - mine)[:20],
                }
            )
    return out


def compare_census(folded: dict, all_in: dict) -> dict:
    """The equivalence check's answer: exact zero difference, or a listed one, over `zoom0`,
    `boxes`, `layers` and `parents`, reported separately since they are not equally comparable
    (`boxes` is frame-dependent, so `extent = "auto"` can disagree at the margins). A layer or
    parent difference inside a box carries the box it was found in and is counted under `layers`
    or `parents` all the same.
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
            at = f"box {i} zoom {x['zoom']}"
            if x["visible"] != y["visible"]:
                differences.append(
                    {
                        "principal": key,
                        "where": at,
                        "folded": x["visible"],
                        "all_in": y["visible"],
                    }
                )
            for d in layer_differences(
                x.get("layers") or {}, y.get("layers") or {}
            ) + parent_differences(x.get("parents") or {}, y.get("parents") or {}):
                differences.append({"principal": key, "box": at, **d})
        for d in layer_differences(a["layers"], b["layers"]) + parent_differences(
            a.get("parents") or {}, b.get("parents") or {}
        ):
            differences.append({"principal": key, **d})
        for kind in ("filters", "categories"):
            mine, theirs = a.get(kind) or {}, b.get(kind) or {}
            for name in sorted(set(mine) | set(theirs)):
                if mine.get(name) != theirs.get(name):
                    differences.append(
                        {
                            "principal": key,
                            "where": "filters",
                            "probe": name,
                            "folded": mine.get(name),
                            "all_in": theirs.get(name),
                        }
                    )
    by_surface: dict[str, int] = {}
    for d in differences:
        where = d.get("where", "missing")
        surface = where if where in ("zoom0", "layers", "parents", "filters") else "boxes"
        by_surface[surface] = by_surface.get(surface, 0) + 1
    return {
        "equal": not differences,
        "zoom0_equal": by_surface.get("zoom0", 0) == 0,
        "boxes_equal": by_surface.get("boxes", 0) == 0,
        "layers_equal": by_surface.get("layers", 0) == 0,
        "parents_equal": by_surface.get("parents", 0) == 0,
        "filters_equal": by_surface.get("filters", 0) == 0,
        "differences_by_surface": by_surface,
        "differences": differences,
    }


def census_coverage(one_view: dict, declared: dict, offered: int = 0, all_in: dict | None = None) -> dict:
    """What a census actually compared, read from the principal that sees the most: artifacts and
    parent edges per census zoom, per layer the levels an artifact was served at against the
    levels the layer declares, and the filter probes against the `offered` operands. `declared` is
    each layer's declared levels, empty for a layer that declares none, which sits wholly at level
    0. `all_in` is the same view's census on the all-in build, whose probes must match something.
    """
    if not one_view:
        return {}
    principal = max(one_view, key=float)
    row = one_view[principal]
    asked = [(0, row["layers"], row.get("parents") or {})] + [
        (box["zoom"], box.get("layers") or {}, box.get("parents") or {})
        for box in row["boxes"]
    ]
    layers = {
        layer: {"declared_levels": [int(level) for level in levels or [0]], "levels": {}, "parent_edges": 0}
        for layer, levels in declared.items()
    }

    def held(layer: str) -> dict:
        return layers.setdefault(
            layer, {"declared_levels": [0], "levels": {}, "parent_edges": 0}
        )

    zooms: dict[str, dict] = {}
    for zoom, artifacts, parents in asked:
        at = zooms.setdefault(str(zoom), {"artifacts": 0, "parent_edges": 0})
        for layer, entry in artifacts.items():
            at["artifacts"] += entry["artifacts"]
            for level, numbers in (entry.get("levels") or {}).items():
                seen = held(layer)["levels"].setdefault(level, {"artifacts": 0, "zooms": []})
                seen["artifacts"] += numbers["artifacts"]
                if zoom not in seen["zooms"]:
                    seen["zooms"].append(zoom)
        for layer, pairs in parents.items():
            at["parent_edges"] += len(pairs)
            held(layer)["parent_edges"] += len(pairs)
    for entry in layers.values():
        compared = [
            level
            for level in entry["declared_levels"]
            if (entry["levels"].get(str(level)) or {}).get("artifacts")
        ]
        entry["levels_declared"] = len(entry["declared_levels"])
        entry["levels_compared"] = len(compared)
        entry["levels_missing"] = [
            level for level in entry["declared_levels"] if level not in compared
        ]
    filters = row.get("filters") or {}
    reference = (all_in or {}).get(principal) or {}
    return {
        "principal": principal,
        "zooms": zooms,
        "layers": layers,
        "visible": row["zoom0_visible"],
        "filters": {
            "offered": offered,
            "compared": len(filters),
            "matching": sum(1 for matched in filters.values() if matched),
            "category_columns": len(row.get("categories") or {}),
            "unmatched_on_all_in": sorted(
                name for name, matched in (reference.get("filters") or {}).items() if not matched
            ),
        },
    }


def coverage_failures(coverage: dict) -> list[str]:
    """A sentence per layer the census proved nothing about: a declared level that no census zoom
    compared with an artifact in it."""
    return [
        f"{layer} was compared at no census zoom with an artifact at level "
        f"{', '.join(str(level) for level in entry['levels_missing'])}"
        for layer, entry in sorted((coverage.get("layers") or {}).items())
        if entry.get("levels_missing")
    ]


def probe_failures(coverage: dict) -> list[str]:
    """A sentence where the filter comparison proved nothing: operands offered and no probe
    compared, or a probe that matched nothing on the all-in build, where its values came from."""
    filters = coverage.get("filters") or {}
    out = []
    if filters.get("offered") and not filters.get("compared") and not filters.get("category_columns"):
        out.append(f"compared no probe of the {filters['offered']} filter operand(s) /v1/meta offers")
    out += [
        f"asked the probe {name}, which matched nothing on the all-in build"
        for name in filters.get("unmatched_on_all_in") or []
    ]
    return out


def incomplete_sentences(views: dict) -> list[str]:
    """Every census request that did not arrive whole, across a census's views and principals."""
    return sorted(
        {
            sentence
            for view in views.values()
            for row in view.values()
            for sentence in row.get("incomplete") or []
        }
    )
