"""The ingest cycle — decision 0091's test, run as a measurement rather than as an assertion.

A rung is built from *all* of its rows. This driver holds a seeded, uniform fraction *f* of the
entities back, builds the complement, serves it, and puts the hold-out through `/control/ingest`
— then flushes, folds, and asks whether the two deployments give the same masked counts. That is
[decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)'s claim
stated as a number instead of a principle: *build is ingest into an empty database*, so a
deployment assembled either way must answer identically.

What it measures, in order
--------------------------

1. **The split and the base build** — `tessera build --stage-timings-json`, so the base's per-stage
   record is on the same schema as the whole-corpus build's.
2. **Online ingest** — Arrow IPC batches of 10,000 rows at *C* concurrent callers, `items/s`
   acked, ack p50/p99, and every refusal counted by status (429 backpressure, 409 duplicate or
   batch-id conflict, 422 bounds or contract).
3. **Flush** — the wall of `POST /control/flush`, and *time to visibility*: when a zoom-0 viewport
   under the 100% principal reaches the expected count. Those are two different numbers and the
   second is the one a viewer experiences.
4. **The fold** — `POST /control/compact`, its wall and its RSS, both read from
   `/control/status`'s own `compaction` block rather than timed from outside: the route answers
   202 immediately, so an outside timer would measure the request and not the fold.
5. **Equivalence** — the ladder's masked counts on the folded deployment against the all-in build,
   at zoom 0 and on a set of boxes, per layer and per principal. Exact zero difference, or a
   listed one.
6. **The write cycle** — deletes, suppressions, re-ingests, another fold and the census again.

⊘ **A hold-out is not a random sample of the map.** Entity ids are assigned in signature-sorted
order at a build and above the high-water at ingest (0091's own stated internal difference), so
the ingested rows land in a different place in entity space than the build would have put them.
That is *expected* and is not what the equivalence test checks: it checks the masked counts a
principal is served, which is what a client can observe.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Sequence

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import requests
from pyarrow import ipc

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib

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


def filter_parquet(source: Path, out: Path, column: str, keep: np.ndarray) -> int:
    """Copy `source` to `out`, keeping rows whose `column` is in `keep`. **A row group at a time.**

    Streaming rather than `read_table().filter()` because rung 3's member table is 1.66×10⁹ rows:
    read whole it is tens of gigabytes of Arrow, and the machine this runs on has 47.
    """
    reader = pq.ParquetFile(source)
    writer = None
    kept = 0
    try:
        for batch in reader.iter_batches(batch_size=1 << 20):
            table = pa.Table.from_batches([batch])
            mask = pa.array(in_sorted(table.column(column).to_numpy(), keep))
            table = table.filter(mask)
            if writer is None:
                writer = pq.ParquetWriter(out, reader.schema_arrow)
            if table.num_rows:
                writer.write_table(table)
                kept += table.num_rows
    finally:
        if writer is not None:
            writer.close()
    if writer is None:  # an empty source still needs a file with the right schema
        pq.write_table(reader.schema_arrow.empty_table(), out)
    return kept


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


def strip_all_members_content(out: Path) -> list[dict]:
    """Drop the supplied content kinds an **empty** base cannot carry, from the base declaration
    and from the rosters that supply them.

    A supplied kind declaring `require_member_visibility = "all"` is served only to a viewer who
    can see every document it was generated from, so an artifact carrying it must name that
    generating set — an empty one is satisfied by everyone, and the registry refuses it at both
    entry points alike (`tessera_lifecycle::registry`). A generating set is named by the member
    rows carrying a `rank`, so a base with **no rows at all** has no generating set for any
    artifact, and the kind cannot exist there.

    That is not a defect and it is not patched around: an empty deployment genuinely has no
    description that was generated from its corpus, because it has no corpus. The kind is removed
    from the base's declaration, the roster's `contents` column is nulled where nothing else is
    declared to fill it, and what was removed is recorded — so the layer census below reports the
    difference rather than hiding it. Only the *measurement's* copy is edited, never the rung's.

    Returns one record per kind removed, empty when the declaration has none.
    """
    corpus_toml = out / "corpus.toml"
    lines = corpus_toml.read_text().splitlines(keepends=True)
    sources: dict[str, str] = tomllib.loads(corpus_toml.read_text()).get("sources", {})

    removed: list[dict] = []
    keep: list[str] = []
    layer = {"name": None, "source": None}
    remaining_supplied: dict[str, int] = {}
    i = 0
    while i < len(lines):
        head = lines[i].strip()
        if head == "[[layer]]":
            layer = {"name": None, "source": None}
        if head.startswith("[[layer.content.supplied]]"):
            # The block runs to the next table header at any indent, or to the end of the file.
            j = i + 1
            while j < len(lines) and not lines[j].strip().startswith("["):
                j += 1
            block = "".join(lines[i:j])
            if '"all"' in block and "require_member_visibility" in block:
                removed.append(
                    {
                        "layer": layer["name"],
                        "source": layer["source"],
                        "kind": next(
                            (
                                line.split("=", 1)[1].strip().strip('"')
                                for line in block.splitlines()
                                if line.strip().startswith("name")
                            ),
                            None,
                        ),
                    }
                )
            else:
                remaining_supplied[layer["name"]] = remaining_supplied.get(layer["name"], 0) + 1
                keep.extend(lines[i:j])
            i = j
            continue
        if layer["name"] is None and head.startswith("name") and "=" in head:
            layer["name"] = head.split("=", 1)[1].strip().strip('"')
        if layer["source"] is None and head.startswith("source") and "=" in head:
            layer["source"] = head.split("=", 1)[1].strip().strip('"')
        keep.append(lines[i])
        i += 1

    if not removed:
        return []
    corpus_toml.write_text("".join(keep))
    for entry in removed:
        if remaining_supplied.get(entry["layer"]):
            # Something else still fills the column, so the roster keeps it; the values of the
            # removed kind stay where they sit and the build reads one fewer of them.
            continue
        roster = sources.get(entry["source"] or "")
        path = out / roster if roster else None
        if path is None or not path.exists():
            entry["roster"] = None
            continue
        table = pq.read_table(path)
        column = table.schema.field("contents")
        table = table.set_column(
            table.schema.get_field_index("contents"),
            column,
            pa.nulls(table.num_rows, column.type),
        )
        pq.write_table(table, path)
        entry["roster"] = roster
    return removed


def write_base_inputs(rung: Path, out: Path, base_ids: np.ndarray) -> dict:
    """The complement's inputs: the points file filtered, the member tables filtered, the rest copied.

    **The artifact rosters are copied whole**, not filtered: a cluster or a descriptor exists
    because the layer declares it, and dropping the ones whose members all fell into the hold-out
    would make the two deployments differ in their *roster* as well as in their membership, which
    is a second variable in a test that has one. It is also what makes the *f* = 100% cell
    possible at all on a rung whose layers are `value_set = "closed"`: an arriving point may only
    join an artifact that already exists, so the roster is what the empty bundle is for.

    The one thing an empty base cannot carry is a supplied content kind requiring every member
    visible — see [`strip_all_members_content`], which the caller applies there.
    """
    out.mkdir(parents=True, exist_ok=True)
    kept = {"points": filter_parquet(rung / "points.parquet", out / "points.parquet", "entity_id", base_ids)}
    for name in ("clusters-kmeans-members.parquet", "mesh-descriptors-members.parquet"):
        kept[name] = filter_parquet(rung / name, out / name, "entity", base_ids)
    for name in (
        "branch.parquet",
        "clusters-kmeans.parquet",
        "mesh-descriptors.parquet",
        "corpus.toml",
        ".env",
    ):
        source = rung / name
        if source.exists():
            shutil.copy2(source, out / name)
    (out / "tessera.toml").write_text((rung / "tessera.toml").read_text())
    return kept


# ---------------------------------------------------------------------------------------------
# The hold-out, as ingest batches
# ---------------------------------------------------------------------------------------------


def encode_batch(table: pa.Table, layers: Sequence[str], membership: dict) -> bytes:
    """One Arrow IPC stream for a slice of the hold-out.

    `access` is the passthrough plugin's wire form — a comma-separated descriptor list — and
    `external_id` is the article's PMID as bytes, which is what makes an ingested row addressable
    on `/control/changes` afterwards.
    """
    branches = table.column("branches").to_pylist()
    pmids = table.column("pmid").to_pylist()
    arrays = [
        table.column("x").cast(pa.float64()).combine_chunks(),
        table.column("y").cast(pa.float64()).combine_chunks(),
        pa.array([",".join(b) for b in branches], pa.string()),
        pa.array([p.encode() for p in pmids], pa.binary()),
        table.column("published").combine_chunks(),
        table.column("title").combine_chunks(),
        table.column("mesh_major").combine_chunks(),
        table.column("pmid").combine_chunks(),
    ]
    names = ["x", "y", "access", "external_id", "published", "title", "mesh_major", "pmid"]
    if layers:
        entities = table.column("entity_id").to_pylist()
        for layer in layers:
            per = membership[layer]
            arrays.append(pa.array([per.get(int(e), []) for e in entities], pa.list_(pa.string())))
            names.append(layer)
    batch = pa.RecordBatch.from_arrays([pa.array(a) if not isinstance(a, pa.Array) else a for a in arrays], names=names)
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class HoldOut:
    """The held-back rows, streamed out of the rung's own parquet as ingest batches.

    **Streamed, never materialised.** At *f* = 100% of rung 3 the hold-out is the whole corpus —
    4 GB of parquet, tens of gigabytes of Arrow — and holding it beside a running server on a
    47 GB box is the run failing for a reason that has nothing to do with what it measures.

    Membership for a *carried* layer is the exception and is held: the wire wants one cell per
    entity and the member table is one row per `(key, entity)` pair, so the inversion has to
    happen somewhere. `--max-member-rows` is what stops that being attempted for rung 3's
    1.66×10⁹-row DAG membership.
    """

    def __init__(self, rung: Path, held: np.ndarray, layers: Sequence[str], head_rows: int = 0):
        self.rung = rung
        self.held = np.sort(held)
        self.layers = list(layers)
        self.head_rows = head_rows
        self.head: pa.Table | None = None
        self.membership: dict[str, dict[int, list[str]]] = {}
        sources = {
            "clusters/kmeans": "clusters-kmeans-members.parquet",
            "mesh/descriptors": "mesh-descriptors-members.parquet",
        }
        for layer in self.layers:
            per: dict[int, list[str]] = {}
            reader = pq.ParquetFile(rung / sources[layer])
            for batch in reader.iter_batches(batch_size=1 << 20, columns=["key", "entity"]):
                entity = batch.column("entity").to_numpy()
                inside = np.nonzero(in_sorted(entity, self.held))[0]
                if not len(inside):
                    continue
                keys = batch.column("key").to_pylist()
                for i in inside:
                    per.setdefault(int(entity[i]), []).append(keys[i])
            self.membership[layer] = per

    def batches(self, rows: int = BATCH_ROWS):
        """Yield `(first row index, body bytes, row count)` for the whole hold-out, in file order."""
        reader = pq.ParquetFile(self.rung / "points.parquet")
        pending: list[pa.Table] = []
        pending_rows = 0
        emitted = 0
        head: list[pa.Table] = []
        head_rows = 0
        for batch in reader.iter_batches(batch_size=1 << 17):
            table = pa.Table.from_batches([batch])
            table = table.filter(pa.array(in_sorted(table.column("entity_id").to_numpy(), self.held)))
            if table.num_rows == 0:
                continue
            if head_rows < self.head_rows:
                take = min(self.head_rows - head_rows, table.num_rows)
                head.append(table.slice(0, take))
                head_rows += take
            pending.append(table)
            pending_rows += table.num_rows
            while pending_rows >= rows:
                whole = pa.concat_tables(pending)
                yield emitted, encode_batch(whole.slice(0, rows), self.layers, self.membership), rows
                emitted += rows
                rest = whole.slice(rows)
                pending = [rest] if rest.num_rows else []
                pending_rows = rest.num_rows
        if pending_rows:
            whole = pa.concat_tables(pending)
            yield emitted, encode_batch(whole, self.layers, self.membership), pending_rows
            emitted += pending_rows
        self.total = emitted
        if head:
            self.head = pa.concat_tables(head)


def probe_batch(points: pa.Table, external_id: str, layer: str, keys: Sequence[str]) -> bytes:
    """A one-row batch carrying one layer's membership column, for the end-to-end layer probe.

    Built from the rung's **roster** rather than its member table: the point of the probe is
    whether the wire accepts a membership cell for this layer at all, and reading a 1.66×10⁹-row
    member table to find out would cost more than the run it precedes.
    """
    row = points.slice(0, 1)
    batch = pa.RecordBatch.from_arrays(
        [
            pa.array([float(row.column("x").to_pylist()[0])], pa.float64()),
            pa.array([float(row.column("y").to_pylist()[0])], pa.float64()),
            pa.array([",".join(row.column("branches").to_pylist()[0])], pa.string()),
            pa.array([external_id.encode()], pa.binary()),
            pa.array(row.column("published").to_pylist(), row.column("published").type),
            pa.array(row.column("title").to_pylist(), pa.string()),
            pa.array(row.column("mesh_major").to_pylist(), pa.string()),
            pa.array([external_id], pa.string()),
            pa.array([list(keys)], pa.list_(pa.string())),
        ],
        names=["x", "y", "access", "external_id", "published", "title", "mesh_major", "pmid", layer],
    )
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


# ---------------------------------------------------------------------------------------------
# The control plane
# ---------------------------------------------------------------------------------------------


class Control:
    def __init__(self, base: str, cred: str):
        self.base = base
        self.headers = {"Authorization": f"Bearer {cred}"}

    def status(self) -> dict:
        r = requests.get(f"{self.base}/control/status", headers=self.headers, timeout=60)
        r.raise_for_status()
        return r.json()

    def ingest(self, body: bytes, batch_id: str, session: requests.Session, timeout=600):
        t0 = time.perf_counter()
        r = session.post(
            f"{self.base}/control/ingest",
            headers=self.headers
            | {"x-tessera-batch-id": batch_id, "Content-Type": "application/octet-stream"},
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
        row["layers"] = layer_frame_counts(r.content)
        out[f"{rung['target']:.4f}"] = row
    return out


def layer_frame_counts(content: bytes) -> dict:
    """Every frame in a viewport response after the tiles frame, by kind: rows and count sums.

    The wire is a sequence of `(u8 kind, u32 length, payload)` frames. What this needs from the
    artifact frames is a number that changes if a layer's masked membership changes, and `count`
    summed over the frame's rows is that number; the frame's own kind is carried so a change in
    which frames were served is visible too.
    """
    out: dict = {}
    offset = 0
    while offset + 5 <= len(content):
        kind = content[offset]
        length = int.from_bytes(content[offset + 1 : offset + 5], "little")
        payload = content[offset + 5 : offset + 5 + length]
        offset += 5 + length
        if kind == 1 or not payload:
            continue
        try:
            table = ipc.open_stream(payload).read_all()
        except Exception:
            continue
        entry = {"rows": table.num_rows}
        for column in ("count", "visible", "members"):
            if column in table.column_names:
                entry[column] = sum(int(v or 0) for v in table.column(column).to_pylist())
        out.setdefault(str(kind), []).append(entry)
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
    * `layers` — the artifact frames served with the viewport, by frame kind.
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
        if a["layers"] != b["layers"]:
            differences.append(
                {"principal": key, "where": "layers", "folded": a["layers"], "all_in": b["layers"]}
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
        if base_dir.exists():
            shutil.rmtree(base_dir)
        self.log(f"splitting: base {len(base_ids):,} rows, hold-out {len(held):,} rows")
        write_base_inputs(self.rung, base_dir, base_ids)
        if len(base_ids) == 0:
            # **The empty base.** A description generated from every member of an artifact that has
            # no members is satisfied by everyone, which the registry refuses at either entry point;
            # the kind comes out of the measurement's own declaration and the removal is recorded.
            self.result["content_removed"] = strip_all_members_content(base_dir)
            if self.result["content_removed"]:
                self.log(f"empty base: removed {self.result['content_removed']}")
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

        scratch = self.work / f"serve-{args.fraction:g}"
        served = Deployment(
            base_dir,
            base_dir / "bundle",
            scratch,
            (args.port0, args.port0 + 1, args.port0 + 2),
            self.binary,
        )
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
            control = Control(served.control, served.credential("operator"))
            session_cred = served.credential("session")
            ranks = json.loads((self.rung / "branch-ranks.json").read_text())
            all_terms = sorted(r["term"] for r in ranks)
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            m = serve_battery.meta(served.viewer, token)
            view = m["views"][0]["id"]
            quant = m["views"][0]["quantisation"]
            self.result["base_visible"] = serve_battery.viewport(
                served.viewer, token, view, 0,
                [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]], k=1
            )["counts"]["visible"]

            layers = self.probe_layers(control, served, view)
            head = 3 * args.write_cycle_n if args.write_cycle else 0
            hold = HoldOut(self.rung, self.held, list(layers["carried"]), head_rows=head)
            self.log(
                f"ingesting {len(self.held):,} rows at C={args.concurrency}, "
                f"layers={layers['carried']} (declined {list(layers['declined'])})"
            )
            self.result["ingest"] = self.run_ingest(control, hold.batches(), "cycle")
            self.result["layers"] = layers
            self.log(f"  {self.result['ingest']['items_per_s']} items/s")

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
            served.stop()
        return self.result

    # -- layer membership on the wire ------------------------------------------------------

    def probe_layers(self, control: Control, served: Deployment, view: str) -> dict:
        """Does the membership path carry this rung's two layers, end to end?

        One single-row batch per layer, sent before the run proper and **deleted again** so the
        probe leaves the deployment as it found it. What it can find out is which of the two the
        *wire* can express — and the answer is not the same for both, because a membership
        column's meaning is the layer's hierarchy kind
        (`tessera_types::layer::ListMeaning`): a `flat` layer's cell is an unordered set of keys,
        so one point in one cluster is one entry; a `dag` layer's cell is a **lineage**, one path
        from a root, so one cell cannot say *this article is in ten unrelated concepts*, which is
        what a MeSH-indexed article is.

        Whatever this finds is recorded and the run proceeds with the layers that work. It is not
        patched around: a fraction whose ingested rows carry no `mesh/descriptors` membership has
        a different masked count on that layer **by design**, and the equivalence block must
        report the difference rather than have it hidden by a driver that quietly filled it in.
        """
        out: dict = {"probed": {}, "carried": [], "declined": {}}
        declared = {
            layer["name"]: layer.get("hierarchy", {}).get("kind", "flat")
            for layer in tomllib.loads((self.rung / "corpus.toml").read_text()).get("layer", [])
        }
        out["hierarchy_kinds"] = declared
        points = pq.read_table(self.rung / "points.parquet").slice(0, 1)
        rosters = {
            "clusters/kmeans": "clusters-kmeans.parquet",
            "mesh/descriptors": "mesh-descriptors.parquet",
        }
        members = {
            "clusters/kmeans": "clusters-kmeans-members.parquet",
            "mesh/descriptors": "mesh-descriptors-members.parquet",
        }
        for layer, roster in rosters.items():
            keys = pq.read_table(self.rung / roster, columns=["key"]).column("key").to_pylist()[:3]
            probe_id = f"layer-probe-{uuid.uuid4().hex[:12]}"
            body = probe_batch(points, probe_id, layer, keys)
            r, _ = control.ingest(body, probe_id, requests.Session())
            entry = {"status": r.status_code, "keys_offered": len(keys), "body": r.text[:800]}
            if r.status_code == 200:
                # Withdraw it: a probe row left behind would be one row of difference between the
                # folded deployment and the all-in build, in a test whose answer is "exact zero".
                withdraw, _ = control.changes(
                    [{"external_id": base64.b64encode(probe_id.encode()).decode(), "op": "delete"}]
                )
                entry["withdrawn"] = withdraw.status_code
                rows = pq.ParquetFile(self.rung / members[layer]).metadata.num_rows
                entry["member_rows"] = rows
                kind = declared.get(layer, "flat")
                if kind in ("dag", "nested") and not self.args.carry_lineage_layers:
                    # **The path exists — the probe above was a 200 — and the semantics do not
                    # match.** Under `dag` and `nested` a membership cell is a *lineage*: entry k
                    # is the parent of entry k+1, every artifact at level 0
                    # (`tessera_types::layer::ListMeaning`). This rung's articles are in a mean of
                    # 10.6 unrelated MeSH descriptors, and a list of ten unrelated keys read as a
                    # lineage declares nine parent edges the NLM's DAG does not have.
                    #
                    # **Measured, 2026-09-03: the server does not absorb them.** A probe batch of
                    # three keys produced `an ingest batch's list column names parent edges these
                    # layers do not hold; the memberships are applied and the edges are not` —
                    # so the hierarchy is *not* silently extended, and the memberships do land.
                    # The layer is still declined by default because the cell would be saying
                    # something the data does not mean and the warning count would scale with the
                    # corpus; `--carry-lineage-layers` runs it deliberately, and on medcpt-1m
                    # that run cost 31k items/s against 157k with the column absent.
                    #
                    # Stated, not patched around: the ingested rows carry no membership on this
                    # layer, so every later count on it differs by design, and the equivalence
                    # block reports the difference.
                    out["declined"][layer] = (
                        f"declared `kind = \"{kind}\"`, whose membership cell is a lineage; this "
                        f"rung's points are in several unrelated artifacts each, which a lineage "
                        f"cannot express and which a `dag` would absorb as new parent edges"
                    )
                elif rows > self.args.max_member_rows:
                    out["declined"][layer] = (
                        f"{rows:,} member rows exceeds --max-member-rows "
                        f"{self.args.max_member_rows:,}; the driver would have to hold the whole "
                        f"membership in memory to invert it per entity"
                    )
                else:
                    out["carried"].append(layer)
            out["probed"][layer] = entry
        return out

    def probe_layers_after_ingest(self, served, session_cred, view, quant, all_terms) -> dict:
        """One zoom-0 viewport **with `layers: "all"`** after the flush, timed and allowed to fail.

        Its own measurement because it is the request that broke the first 3.6×10⁷ cell. The
        trigger is not the flush: it is `probe_layers` above, whose one-row growth into
        `mesh/descriptors` moves the level's version, after which the engine refuses the
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
        published, publish_s = wait_for(
            lambda: control.status()["write_executor"]["flush"]["flushes"] > before,
            timeout=self.args.flush_timeout,
        )
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

        # The all-in deployment, served beside it on its own ports and scratch.
        scratch = self.work / "serve-allin"
        allin = Deployment(
            self.rung,
            self.rung / "bundle",
            scratch,
            (self.args.port0 + 10, self.args.port0 + 11, self.args.port0 + 12),
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

        Addressed by `external_id` — the PMID — because that is the address an ingested row has
        that survives a delete: `tessera_id`s are per entity and a deleted holder does not block a
        re-ingest of the same external id (decision 0047, edit is delete + re-ingest).
        """
        if hold.head is None or hold.head.num_rows < 2:
            return {"skipped": "hold-out too small for a write cycle"}
        pmids = hold.head.column("pmid").to_pylist()
        n = min(self.args.write_cycle_n, len(pmids) // 3)
        if n == 0:
            return {"skipped": "hold-out too small for a write cycle"}
        deletes = pmids[:n]
        suppressions = pmids[n : 2 * n]
        full = [quant["x_min"], quant["y_min"], quant["x_max"], quant["y_max"]]

        def visible() -> int:
            token, _ = serve_battery.authorise(served.session, session_cred, all_terms)
            return serve_battery.viewport(
                served.viewer, token, view, 0, full, k=1, layers=None
            )["counts"]["visible"]

        start_visible = visible()
        out: dict = {"n": n, "visible_before": start_visible}

        for op, ids in (("delete", deletes), ("suppress", suppressions)):
            items = [
                {"external_id": base64.b64encode(p.encode()).decode(), "op": op} for p in ids
            ]
            r, wall = control.changes(items)
            expected = start_visible - len(ids) if op == "delete" else None
            reached, visibility_s = wait_for(
                lambda: visible() <= (expected if expected is not None else start_visible - len(ids)),
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
        # `deletes` is `pmids[:n]`, so the rows to re-ingest are the head's first `n` — the same
        # bytes, under fresh batch ids, which is what makes this a re-ingest rather than a replay.
        def head_slice():
            for start in range(0, n, BATCH_ROWS):
                chunk = hold.head.slice(start, min(BATCH_ROWS, n - start))
                yield start, encode_batch(chunk, hold.layers, hold.membership), chunk.num_rows

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
        "--carry-lineage-layers",
        action="store_true",
        help="send a membership column for a `dag`/`nested` layer anyway. The cell is a lineage "
        "there, so a multi-membership rung's keys become parent edges: measure it deliberately, "
        "never by default",
    )
    ap.add_argument(
        "--state-extent",
        action="store_true",
        help="rewrite the base declaration's `extent = \"auto\"` as the all-in bundle's own "
        "frame — required for f = 1.0, which auto refuses, and what makes a box-level "
        "equivalence census compare like with like",
    )
    ap.add_argument(
        "--max-member-rows",
        type=int,
        default=200_000_000,
        help="decline to carry a layer whose member table is larger than this: the driver inverts "
        "it per entity in memory, and rung 3's DAG membership is 1.66e9 rows",
    )
    ap.add_argument("--flush-timeout", type=float, default=900.0)
    ap.add_argument("--fold-timeout", type=float, default=7200.0)
    args = ap.parse_args(argv)
    started = time.time()
    result = Cycle(args).run()
    result["ran_s"] = round(time.time() - started, 1)
    Path(args.out).write_text(json.dumps(result, indent=2, default=str))
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
