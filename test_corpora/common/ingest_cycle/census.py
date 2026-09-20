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
        row["parents"] = artifact_frame_parents(r.content)
        out[f"{rung['target']:.4f}"] = row
    return out


def artifact_frame_rows(content: bytes):
    """Every kind-5 artifact frame of a viewport response, as Arrow tables.

    The wire is a sequence of `(u8 kind, u32 LE length, payload)` frames, and kind 5 is one row per
    served artifact (`tessera-wire/src/payload.rs`).
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
    """The served artifacts **per layer**: how many, and their summed masked count.

    The `layer` column is dictionary-encoded and `masked_count` is what the viewer is shown, so
    grouping the rows by layer gives how many artifacts this principal is served on each layer and
    how many documents those artifacts count for them.

    **Two numbers per layer, not one.** An artifact count alone passes a defect that serves the
    right artifacts with the wrong memberships; a summed masked count alone passes one that moves
    members between artifacts of the same layer.
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
    """The served parent links **per layer**, as sorted `[child, parent]` pairs of artifact keys.

    `parent_ids` carries the `tessera_id` of each parent that is a row of this same frame, and a
    `tessera_id` is a blinding permutation of an entity — a number of *this* deployment, which the
    same artifact in another deployment has no reason to share. The frame's own `key` column is the
    caller's name for the artifact and is the same on both sides of the split, so every id is read
    back through the frame's `tessera_id → key` map and the pairs are compared by key.

    An artifact whose key the wire did not carry falls back to its `tessera_id` as a string, so a
    layer whose keys are withheld is listed as differing rather than passed quietly.

    A count and a masked count say nothing about the shape of a hierarchy: a deployment that served
    every artifact of a `dag` layer with no edges at all agrees with one that served them all.
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
    * `parents` — the same frame's parent links, by key, per layer: which artifact names which.
      Frame-independent and membership-independent, and the only surface on which a hierarchy that
      arrived without its edges differs from one that arrived with them.
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
                        # The pairs themselves, capped: a layer with 10⁵ edges would otherwise
                        # write the whole hierarchy into the result twice.
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
