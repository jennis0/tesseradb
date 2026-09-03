"""The ingest cycle — decision 0091's test, run as a measurement rather than as an assertion.

A rung is built from *all* of its rows. This driver holds a seeded, uniform fraction *f* of the
entities back, builds the complement, serves it, and puts the hold-out through `/control/ingest`
— then flushes, folds, and asks whether the two deployments give the same masked counts. That is
[decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)'s claim
stated as a number instead of a principle: *build is ingest into an empty database*, so a
deployment assembled either way must answer identically.

Everything is ingested after the build (owner ruling, 2026-09-03)
----------------------------------------------------------------

**The base bundle carries points and declarations, and nothing else.** The rung's `corpus.toml` is
copied with every `[[layer]]`'s `source` and `[layer.members]` removed, so each layer is declared —
kind, levels, visibility rules, content kinds — and empty. Its artifacts, their memberships and
their supplied content are then published through `PUT /control/layers/{name}/artifacts` **after
every point they depend on has been ingested**. An artifact cannot depend on a point that does not
exist yet, and that ordering is the only constraint: it holds at every fraction, so at *f* = 10% the
base is 90% of the points and none of the artifacts.

⊘ **What this drops, deliberately.** An earlier driver built the base *with* the rung's artifact
roster and sent membership on the wire as a column of the ingest batch. That put the layers on the
build side of the split, made the *f* = 100% cell impossible for a layer whose content requires
every member visible (an artifact with no members names an empty generating set, which the registry
refuses at both entry points), and met the lineage reading of a list column, which a
multi-membership `dag` layer cannot use (`docs/evidence/memos/2026-09-03-dag-membership-at-ingest.md`).
Publication says *this artifact holds these members* directly, so none of the three arises.

What it measures, in order
--------------------------

1. **The split and the base build** — `tessera build --stage-timings-json` over the complement's
   points and the declaration-only `corpus.toml`, so the base's per-stage record is on the same
   schema as the whole-corpus build's.
2. **Online ingest** — Arrow IPC batches of 10,000 rows at *C* concurrent callers, `items/s`
   acked, ack p50/p99, and every refusal counted by status (429 backpressure, 409 duplicate or
   batch-id conflict, 422 bounds or contract). The batches carry points alone: no membership
   column, because membership arrives with the artifact that holds it.
3. **Publication** — every layer's whole roster, in batches under a byte cap, with each artifact's
   whole member set (base and hold-out alike, by external addressing) and its ranked content with
   its generating set. Its own figure: artifacts/s and members/s.
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

#: `layer name -> (roster file, member file)` for the rungs this driver runs. The names are the
#: rung's own, and a layer whose roster is absent is skipped — which is how a rung prepared without
#: `mesh.py` runs the same cell with one layer instead of two.
LAYER_SOURCES = {
    "clusters/kmeans": ("clusters-kmeans.parquet", "clusters-kmeans-members.parquet"),
    "mesh/descriptors": ("mesh-descriptors.parquet", "mesh-descriptors-members.parquet"),
}


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


def base_declaration(text: str) -> tuple[str, list[dict]]:
    """The rung's `corpus.toml` as a **declaration-only** one: every layer stated, none supplied.

    A `[[layer]]` block says two kinds of thing. Its declaration — kind, levels, views, the three
    disclosure controls, the content kinds — is what a running deployment holds and what
    `PUT /control/layers` takes. Its `source` and `[layer.members]` are *acquisition*: where the
    rows come from, which is build-only and is the half decision 0091 excludes from the rule that
    the two entry points say the same things (`configuration.md` §2). Removing exactly that half
    leaves a layer that exists, is empty, and can be published into.

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


def write_base_inputs(rung: Path, out: Path, base_ids: np.ndarray) -> dict:
    """The complement's inputs: **the points, and the declaration. Nothing else.**

    No artifact roster and no member table is copied, and that is the whole shape of this driver
    (owner ruling, 2026-09-03). Every artifact, every membership and every supplied content is
    published on the wire after the points it depends on have been ingested, so a roster beside the
    build would be the same layer supplied twice — once as a build input and once as a publication —
    and the level's keys would collide on the second.

    `corpus.toml` is rewritten by [`base_declaration`], which removes each layer's acquisition and
    keeps its declaration.
    """
    out.mkdir(parents=True, exist_ok=True)
    kept = {"points": filter_parquet(rung / "points.parquet", out / "points.parquet", "entity_id", base_ids)}
    for name in ("branch.parquet", ".env"):
        source = rung / name
        if source.exists():
            shutil.copy2(source, out / name)
    declaration, removed = base_declaration((rung / "corpus.toml").read_text())
    (out / "corpus.toml").write_text(declaration)
    kept["declaration_only"] = removed
    (out / "tessera.toml").write_text((rung / "tessera.toml").read_text())
    return kept


# ---------------------------------------------------------------------------------------------
# The hold-out, as ingest batches
# ---------------------------------------------------------------------------------------------


def encode_batch(table: pa.Table) -> bytes:
    """One Arrow IPC stream for a slice of the hold-out. **Points alone.**

    `access` is the passthrough plugin's wire form — a comma-separated descriptor list — and
    `external_id` is the **source entity id, eight bytes little-endian** — the same form the build
    mints under `--mint-external-ids` (see [`external_ids`]). That is what makes an ingested row
    addressable on `/control/changes` afterwards, and what an artifact's `members` names it by on
    the same footing as a base row. The PMID travels beside it as the `pmid` attribute, as before.

    **No membership column, and that is the ruling rather than an omission.** A column named for a
    layer is the *other* way a point can name its artifacts, and it is read as a lineage on a
    `nested` or `dag` layer (`tessera_types::layer::ListMeaning`). Publication says the same thing
    from the artifact's side, without that reading and without a per-row edge check, so the driver
    says it once and there.
    """
    branches = table.column("branches").to_pylist()
    entities = table.column("entity_id").to_pylist()
    arrays = [
        table.column("x").cast(pa.float64()).combine_chunks(),
        table.column("y").cast(pa.float64()).combine_chunks(),
        pa.array([",".join(b) for b in branches], pa.string()),
        pa.array([int(e).to_bytes(8, "little") for e in entities], pa.binary()),
        table.column("published").combine_chunks(),
        table.column("title").combine_chunks(),
        table.column("mesh_major").combine_chunks(),
        table.column("pmid").combine_chunks(),
    ]
    names = ["x", "y", "access", "external_id", "published", "title", "mesh_major", "pmid"]
    batch = pa.RecordBatch.from_arrays(
        [pa.array(a) if not isinstance(a, pa.Array) else a for a in arrays], names=names
    )
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class HoldOut:
    """The held-back rows, streamed out of the rung's own parquet as ingest batches.

    **Streamed, never materialised.** At *f* = 100% of rung 3 the hold-out is the whole corpus —
    4 GB of parquet, tens of gigabytes of Arrow — and holding it beside a running server on a
    47 GB box is the run failing for a reason that has nothing to do with what it measures. Only
    `head_rows` rows are kept, for the write cycle, which needs the same bytes twice.
    """

    def __init__(self, rung: Path, held: np.ndarray, head_rows: int = 0):
        self.rung = rung
        self.held = np.sort(held)
        self.head_rows = head_rows
        self.head: pa.Table | None = None

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
                yield emitted, encode_batch(whole.slice(0, rows)), rows
                emitted += rows
                rest = whole.slice(rows)
                pending = [rest] if rest.num_rows else []
                pending_rows = rest.num_rows
        if pending_rows:
            whole = pa.concat_tables(pending)
            yield emitted, encode_batch(whole), pending_rows
            emitted += pending_rows
        self.total = emitted
        if head:
            self.head = pa.concat_tables(head)


# ---------------------------------------------------------------------------------------------
# Publication — the artifacts, after their points
# ---------------------------------------------------------------------------------------------


def external_ids(points: Path) -> np.ndarray:
    """`entity_id -> base64 external id`, as a fixed-width bytes array indexed by entity id.

    **The external id is the source entity id, little-endian, because that is the one address both
    halves of the split share.** A publication names its members by external id, and its members
    are base rows and ingested rows alike: the ingested half carries whatever `external_id` the
    batch supplied, and the built half carries whatever the build minted — which is
    `source_id.to_le_bytes()` under `--mint-external-ids` (`tessera-build`'s `ExternalIdRow`), and
    nothing at all without it. So the driver builds the base with that flag and sends the same
    eight bytes on ingest, and one member list then addresses both.

    ⊘ **This is why the PMID is no longer the external id.** An earlier driver sent the PMID, which
    is the natural caller identifier for this corpus and is still the `pmid` attribute — but the
    build has no way to mint *that* as an external id from a column, so a published membership over
    base rows was unaddressable and the whole batch was refused, naming member 0 of artifact 0.
    Measured 2026-09-03.

    Encoded **once per entity** rather than once per member: rung 3's member table names each
    article ~46 times. `S16` and not `object`: 3.6×10⁷ Python `bytes` are several gigabytes of
    interpreter objects beside a running server, and base64 of eight bytes is twelve characters.
    NumPy strips trailing NULs on the way out and base64 contains none, so the value that comes
    back is exactly what went in.
    """
    ids = pq.read_table(points, columns=["entity_id"]).column("entity_id").to_numpy()
    out = np.zeros(int(ids.max()) + 1, dtype="S16")
    for lo in range(0, len(ids), 1 << 21):
        chunk = ids[lo : lo + (1 << 21)]
        out[chunk] = [
            base64.b64encode(int(e).to_bytes(8, "little")) for e in chunk.tolist()
        ]
    return out


def member_groups(path: Path, keys: Sequence[str]) -> dict:
    """`(key index, rank) -> entity ids`, read off a rung's member table in one pass.

    A member row carries a `rank`: **null is the artifact's membership, `k` is `contents[k]`'s
    generating set** — the same reading the build's member pass makes (`tessera-build/src/layers.rs`),
    so the wire publishes what a build would have planned.

    Grouped by a single `lexsort` over the whole table rather than by a per-key dictionary: at
    4.6×10⁷ rows the dictionary is the cost of the run, and the sort is three columns of numbers.
    """
    idx_parts, rank_parts, entity_parts = [], [], []
    value_set = pa.array(list(keys), pa.string())
    reader = pq.ParquetFile(path)
    for batch in reader.iter_batches(batch_size=1 << 21, columns=["key", "rank", "entity"]):
        idx = pc.index_in(batch.column("key"), value_set=value_set)
        if idx.null_count:
            missing = pc.filter(batch.column("key"), pc.is_null(idx)).to_pylist()[:3]
            raise ValueError(f"{path.name}: member rows name keys the roster does not: {missing}")
        idx_parts.append(idx.to_numpy(zero_copy_only=False).astype(np.int32))
        rank = batch.column("rank")
        rank_parts.append(np.where(
            np.asarray(rank.is_null()), np.int16(-1), rank.fill_null(0).to_numpy().astype(np.int16)
        ))
        entity_parts.append(batch.column("entity").to_numpy().astype(np.uint64))
    if not idx_parts:
        return {}
    idx = np.concatenate(idx_parts)
    rank = np.concatenate(rank_parts)
    entity = np.concatenate(entity_parts)
    del idx_parts, rank_parts, entity_parts
    order = np.lexsort((rank, idx))
    idx, rank, entity = idx[order], rank[order], entity[order]
    del order
    # One integer per `(key, rank)` group, so the boundaries are a single `diff`. 1024 ranks is
    # three orders above any ranking a rung writes, and it is checked rather than assumed: a wider
    # ranking would silently fold two groups into one.
    if rank.max(initial=0) >= 1023:
        raise ValueError(f"{path.name}: a content rank of {int(rank.max())} does not fit this pass")
    key = idx.astype(np.int64) * 1024 + (rank.astype(np.int64) + 1)
    edges = np.flatnonzero(np.diff(key)) + 1
    out = {}
    for lo, hi in zip(np.r_[0, edges], np.r_[edges, len(key)]):
        out[(int(idx[lo]), int(rank[lo]))] = entity[lo:hi]
    return out


def json_list(values: np.ndarray) -> bytes:
    """A JSON array of base64 external ids, assembled as bytes.

    `json.dumps` over 7.6×10⁵ strings is the largest single cost in a publication and it produces
    exactly this; the join does the same work without building the intermediate list.
    """
    if not len(values):
        return b"[]"
    return b'["' + b'","'.join(values.tolist()) + b'"]'


class Publication:
    """One layer's roster, published in batches under a byte cap.

    **Every artifact carries its whole member set** — base rows and ingested rows alike, addressed
    by external id, which is the address both halves share. The batch is the commit unit at the
    route, so an artifact is published entire or not at all; the cap therefore splits *between*
    artifacts, and one artifact larger than the cap is sent alone.

    ⊘ **Parents are not published, because the route has no field for them.** A layer's lineage is
    `IncomingArtifact::parent_keys` inside the service and `parent` in the rung's roster, and
    `IncomingArtifactBody` — the JSON this route takes — carries `key`, `members`, `content`,
    `attached_to` and the shape fields, and nothing else. So a `dag` layer publishes as a flat one:
    the memberships and the content land, every parent edge the roster declared is dropped, and the
    count of them is recorded here beside the numbers rather than patched around
    (`docs/evidence/memos/2026-09-03-dag-membership-at-ingest.md`).
    """

    def __init__(self, roster: Path, members: Path, external: np.ndarray, max_bytes: int):
        self.table = pq.read_table(roster)
        self.members = members
        self.external = external
        self.max_bytes = max_bytes

    def bodies(self) -> tuple[list, dict]:
        """`[(level, body bytes, artifacts, members)]`, and what the roster declared.

        Assembled in full before the first request, so the publication's own wall measures the
        service and not pyarrow — the driver's share is reported separately as `prepared_s`. What
        that costs is the whole roster's bodies in memory at once: ~700 MB for 4.6×10⁷ members,
        which is why a layer past `--max-member-rows` is declined rather than published slowly.
        """
        rows = self.table.to_pylist()
        keys = [r["key"] for r in rows]
        groups = member_groups(self.members, keys)
        stats = {
            "artifacts": len(rows),
            "members": 0,
            "generating_set_entries": 0,
            "edges_declared": 0,
            "edges_published": 0,
            "artifacts_with_several_parents": 0,
        }
        by_level: dict[int, list[tuple[bytes, int]]] = {}
        for i, row in enumerate(rows):
            parents = row.get("parent") or []
            stats["edges_declared"] += len(parents)
            stats["artifacts_with_several_parents"] += 1 if len(parents) > 1 else 0
            members = self.external[groups.get((i, -1), np.zeros(0, np.uint64))]
            stats["members"] += len(members)
            parts = [b'{"key":', json.dumps(row["key"]).encode(), b',"members":', json_list(members)]
            contents = row.get("contents") or []
            if contents:
                blocks = []
                for rank, values in enumerate(contents):
                    generated = self.external[groups.get((i, rank), np.zeros(0, np.uint64))]
                    stats["generating_set_entries"] += len(generated)
                    blocks.append(
                        b'{"values":'
                        + json.dumps(list(values)).encode()
                        + b',"generated_from":'
                        + (json_list(generated) if len(generated) else b"[]")
                        + b"}"
                    )
                parts += [b',"content":[', b",".join(blocks), b"]"]
            if row.get("attached_layer"):
                parts += [
                    b',"attached_to":',
                    json.dumps(
                        {
                            "layer": row["attached_layer"],
                            "level": int(row.get("attached_level") or 0),
                            "key": row["attached_key"],
                        }
                    ).encode(),
                ]
            parts.append(b"}")
            level = int(row.get("level") or 0)
            by_level.setdefault(level, []).append((b"".join(parts), len(members)))
        out = []
        for level, blocks in by_level.items():
            batch, size, artifacts, members = [], 0, 0, 0
            for block, n in blocks:
                if batch and size + len(block) > self.max_bytes:
                    out.append((level, self._body(level, batch), artifacts, members))
                    batch, size, artifacts, members = [], 0, 0, 0
                batch.append(block)
                size += len(block) + 1
                artifacts += 1
                members += n
            if batch:
                out.append((level, self._body(level, batch), artifacts, members))
        return out, stats

    def _body(self, level: int, blocks: list[bytes]) -> bytes:
        return (
            b'{"level":' + str(level).encode() + b',"addressing":"external","artifacts":['
            + b",".join(blocks)
            + b"]}"
        )


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

            head = 3 * args.write_cycle_n if args.write_cycle else 0
            hold = HoldOut(self.rung, self.held, head_rows=head)
            self.log(f"ingesting {len(self.held):,} rows at C={args.concurrency}")
            self.result["ingest"] = self.run_ingest(control, hold.batches(), "cycle")
            self.log(f"  {self.result['ingest']['items_per_s']} items/s")

            # **Every artifact, after every point it depends on.** Before the flush, deliberately:
            # an ingested row is resolvable by its external id from the moment it is acked
            # (`Session::resolve_external_ids` consults the live map first), so the ordering the
            # ruling states — points before the artifacts that name them — is the only one there is.
            self.result["publish"] = self.publish_layers(control)

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

    # -- the artifacts, on the wire --------------------------------------------------------

    def publish_layers(self, control: Control) -> dict:
        """Publish every declared layer's roster, in batches under the byte cap. Its own figure.

        **The whole roster, and the whole of each artifact's membership** — base rows and ingested
        rows alike. An artifact exists because the layer declares it, so publishing only the ones
        whose members survived the split would make the two deployments differ in their *roster* as
        well as in their membership, which is a second variable in a test that has one.

        A layer whose member table is larger than `--max-member-rows` is **declined and recorded**,
        not silently skipped: the driver inverts the table in memory to address it per artifact, and
        rung 3's DAG membership is 1.66×10⁹ rows. The layer is then declared and empty on the folded
        deployment, every count on it differs from the all-in build's by design, and the equivalence
        block reports the difference rather than hiding it.
        """
        external = external_ids(self.rung / "points.parquet")
        out: dict = {"layers": {}, "declined": {}}
        totals = {"artifacts": 0, "members": 0, "wall_s": 0.0, "requests": 0}
        for layer, (roster, members) in LAYER_SOURCES.items():
            roster_path, members_path = self.rung / roster, self.rung / members
            if not roster_path.exists():
                continue
            rows = pq.ParquetFile(members_path).metadata.num_rows
            if rows > self.args.max_member_rows:
                out["declined"][layer] = (
                    f"{rows:,} member rows exceeds --max-member-rows "
                    f"{self.args.max_member_rows:,}; the driver would have to hold the whole "
                    f"membership in memory to address it per artifact"
                )
                self.log(f"  {layer}: DECLINED, {rows:,} member rows")
                continue
            t0 = time.perf_counter()
            bodies, stats = Publication(
                roster_path, members_path, external, self.args.publish_max_bytes
            ).bodies()
            prepared_s = time.perf_counter() - t0
            statuses: dict[str, int] = {}
            refusal = None
            session = requests.Session()
            t0 = time.perf_counter()
            published = {"artifacts": 0, "members": 0}
            for level, body, artifacts, members_n in bodies:
                r, _ = control.publish(layer, body, session)
                statuses[str(r.status_code)] = statuses.get(str(r.status_code), 0) + 1
                if r.status_code == 201:
                    published["artifacts"] += artifacts
                    published["members"] += members_n
                elif refusal is None:
                    refusal = {"level": level, "status": r.status_code, "body": r.text[:1500]}
            wall = time.perf_counter() - t0
            entry = dict(stats)
            entry.update(
                {
                    "requests": len(bodies),
                    "prepared_s": round(prepared_s, 2),
                    "wall_s": round(wall, 2),
                    "published_artifacts": published["artifacts"],
                    "published_members": published["members"],
                    "artifacts_per_s": round(published["artifacts"] / wall, 1) if wall else None,
                    "members_per_s": round(published["members"] / wall, 1) if wall else None,
                    "statuses": statuses,
                    "first_refusal": refusal,
                }
            )
            out["layers"][layer] = entry
            totals["artifacts"] += published["artifacts"]
            totals["members"] += published["members"]
            totals["wall_s"] += wall
            totals["requests"] += len(bodies)
            self.log(
                f"  {layer}: {published['artifacts']:,} artifacts, {published['members']:,} "
                f"members in {wall:.1f} s ({entry['artifacts_per_s']} artifacts/s, "
                f"{entry['members_per_s']} members/s), {len(bodies)} requests, "
                f"{stats['edges_declared']:,} parent edges the wire cannot carry"
            )
        totals["wall_s"] = round(totals["wall_s"], 2)
        totals["artifacts_per_s"] = (
            round(totals["artifacts"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        totals["members_per_s"] = (
            round(totals["members"] / totals["wall_s"], 1) if totals["wall_s"] else None
        )
        out["totals"] = totals
        # ⊘ **The route has no field for a parent**, so a layer's lineage is dropped whatever its
        # kind. Counted rather than worked around — see [`Publication`] and the handover memo.
        out["edges_not_expressible"] = sum(
            entry["edges_declared"] for entry in out["layers"].values()
        )
        return out

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
                yield start, encode_batch(chunk), chunk.num_rows

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
        "--publish-max-bytes",
        type=int,
        default=32 * 1024 * 1024,
        help="the publication byte cap: a batch is split between artifacts to stay under it. One "
        "artifact larger than this is sent alone, the batch being the commit unit; the route's own "
        "cap is 64 MiB",
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
        help="decline to publish a layer whose member table is larger than this: the driver holds "
        "it in memory to address it per artifact, and rung 3's DAG membership is 1.66e9 rows",
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
