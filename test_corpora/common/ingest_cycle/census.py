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
