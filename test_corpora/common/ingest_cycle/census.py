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
    """Masked counts per principal and per layer: zoom 0 over the whole extent, then each box."""
    full = serve_battery.full_box(quant)
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
        # The per-layer census is the whole-extent request with `layers: "all"`.
        r = requests.post(
            f"{viewer}/v1/viewport",
            headers={"Authorization": f"Bearer {token}"},
            json={"view": view, "zoom": 0, "bbox": full, "k": 1, "layers": "all"},
            timeout=300,
        )
        r.raise_for_status()
        row["layers"] = artifact_frame_census(r.content)
        row["parents"] = artifact_frame_parents(r.content)
        out[f"{rung['target']:.4f}"] = row
    return out


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
    wrong memberships.
    """
    out: dict = {}
    for table in artifact_frame_rows(content):
        layers = table.column("layer").to_pylist()
        counts = table.column("masked_count").to_pylist()
        for layer, count in zip(layers, counts):
            entry = out.setdefault(layer, {"artifacts": 0, "masked_count": 0})
            entry["artifacts"] += 1
            entry["masked_count"] += int(count or 0)
    return out


def artifact_frame_parents(content: bytes) -> dict:
    """The served parent links per layer, as sorted `[child, parent]` pairs of artifact keys,
    read back through the frame's `tessera_id -> key` map since a `tessera_id` is specific to
    this deployment.
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


def compare_census(folded: dict, all_in: dict) -> dict:
    """The equivalence check's answer: exact zero difference, or a listed one, over `zoom0`,
    `boxes`, `layers` and `parents`, reported separately since they are not equally comparable
    (`boxes` is frame-dependent, so `extent = "auto"` can disagree at the margins).
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
        # One difference per layer, not one per principal.
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
        folded_parents, all_in_parents = a.get("parents") or {}, b.get("parents") or {}
        for layer in sorted(set(folded_parents) | set(all_in_parents)):
            mine = [tuple(pair) for pair in folded_parents.get(layer) or []]
            theirs = [tuple(pair) for pair in all_in_parents.get(layer) or []]
            if set(mine) != set(theirs):
                differences.append(
                    {
                        "principal": key,
                        "where": "parents",
                        "layer": layer,
                        "folded_edges": len(mine),
                        "all_in_edges": len(theirs),
                        # Capped, so a large layer does not write its whole hierarchy twice.
                        "only_folded": sorted(set(mine) - set(theirs))[:20],
                        "only_all_in": sorted(set(theirs) - set(mine))[:20],
                    }
                )
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
