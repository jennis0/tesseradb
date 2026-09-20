from __future__ import annotations

from typing import Sequence

import requests
from pyarrow import ipc

from .. import serve_battery

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
    """Masked counts per principal and per layer: zoom 0 over the whole extent, then each box.

    Every request carries `layers: "all"`, so each box compares the artifacts and the parent links
    at the levels that box's zoom serves — a tiered layer answers only the levels whose declared
    zoom range covers the request's zoom, and zoom 0 reaches its coarsest level alone.
    """
    full = serve_battery.full_box(quant)
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
        out[f"{rung['target']:.4f}"] = row
    return out


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
    by_surface: dict[str, int] = {}
    for d in differences:
        where = d.get("where", "missing")
        surface = where if where in ("zoom0", "layers", "parents") else "boxes"
        by_surface[surface] = by_surface.get(surface, 0) + 1
    return {
        "equal": not differences,
        "zoom0_equal": by_surface.get("zoom0", 0) == 0,
        "boxes_equal": by_surface.get("boxes", 0) == 0,
        "layers_equal": by_surface.get("layers", 0) == 0,
        "parents_equal": by_surface.get("parents", 0) == 0,
        "differences_by_surface": by_surface,
        "differences": differences,
    }


def census_coverage(one_view: dict, declared: dict) -> dict:
    """What a census actually compared, read from the principal that sees the most: artifacts and
    parent edges per census zoom, and per layer the levels an artifact was served at against the
    levels the layer declares. `declared` is each layer's declared levels, empty for a layer that
    declares none, which sits wholly at level 0.
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
    return {"principal": principal, "zooms": zooms, "layers": layers}


def coverage_failures(coverage: dict) -> list[str]:
    """A sentence per layer the census proved nothing about: a declared level that no census zoom
    compared with an artifact in it."""
    return [
        f"{layer} was compared at no census zoom with an artifact at level "
        f"{', '.join(str(level) for level in entry['levels_missing'])}"
        for layer, entry in sorted((coverage.get("layers") or {}).items())
        if entry.get("levels_missing")
    ]


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
