"""The ingest cycle — decision 0091's test, run as a measurement rather than as an assertion.

A rung is built from *all* of its rows. This driver holds a seeded, uniform fraction *f* of the
entities back, builds the complement, serves it, and puts the hold-out through `/control/ingest`
— then flushes, folds, and asks whether the two deployments give the same masked counts. That is
[decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)'s claim
stated as a number instead of a principle: *build is ingest into an empty database*, so a
deployment assembled either way must answer identically.

Which route a layer's membership takes
--------------------------------------

**The shape of the layer's member table decides the route, and nothing else does.** The rung's
`corpus.toml` is copied with the publication route's rosters removed, so every layer is declared —
kind, levels, visibility rules, content kinds — and each one's membership travels one of three
ways:

* **A member table of one row per (artifact, entity)** (every `clusters/*`, rung 3's MeSH
  descriptors) addresses artifacts, and a row with a `rank` is the generating set of that ranked
  content rather than membership. Such a layer is declared and empty at the base — its roster and
  its `[layer.members]` are both removed, and any membership column is dropped from the base points
  file — and its artifacts, their whole memberships and their supplied content are published
  through `PUT /control/layers/{name}/artifacts` **after every point they name has been ingested**.
  Content requiring every member visible is served only against a generating set, which is a set of
  points, so it can only arrive with the artifact.
* **A member table of one row per point**, a list of keys with one entry per declared level
  (`features/taxonomy`, `admin/hierarchy`), is read beside the points file at a build and arrives at
  a running service as the ingest batch's column named for the layer. Both entry points read it by
  one rule, so `[layer.members]` stays at the base build over the base's rows and every hold-out row
  carries its own list on the wire; a null entry is *in no artifact at that level*. The table is
  read in lockstep with the points file, both ascending by entity and a row group at a time, so it
  is never held whole.
  **Where such a layer declares supplied content its roster stays too**, and is published — keys
  and content, members empty — *before* the ingest: an artifact of a layer declaring supplied
  content is never minted from a key alone, at either entry point, so every key a batch's column
  names must already exist, and a key no base row names exists nowhere else. A key the base build
  already holds is compared part by part and its restated content changes nothing.
* **An attribute-membership layer** (`publishers/source`) carries nothing: its membership is
  evaluated against the indexed column every batch already sends.

An artifact cannot name a point that does not exist yet, and that ordering is the only constraint on
the publication route: it holds at every fraction, so at *f* = 10% the base is 90% of the points and
none of the published artifacts.

What it measures, in order
--------------------------

1. **The split and the base build** — `tessera build --stage-timings-json` over the complement's
   points and the declaration-only `corpus.toml`, so the base's per-stage record is on the same
   schema as the whole-corpus build's.
2. **Online ingest** — Arrow IPC batches of 10,000 rows at *C* concurrent callers, `items/s`
   acked, ack p50/p99, and every refusal counted by status (429 backpressure, 409 duplicate or
   batch-id conflict, 422 bounds or contract). The batches carry the points, their labels as a
   list, and the member list of any layer on the column route — see above. **One pass per declared
   view**, the anchor first, each from that view's own points file: a second view's row for an
   entity the anchor's pass allocated joins it there under the identity it already has.
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
   **on every declared view**: at zoom 0, on a set of boxes drawn in that view's own frame, and per
   layer — artifact count, summed masked count and the parent links by key off the kind-5 artifact
   frame, per principal. Exact zero difference, or a listed one.
7. **The write cycle** — deletes, suppressions, re-ingests, another fold and the census again; then
   a **restart** over the same bundle, cache and WAL, which must answer the same counts and the same
   census as the deployment answered before it was stopped.

⊘ **A hold-out is not a random sample of the map.** Entity ids are assigned in signature-sorted
order at a build and above the high-water at ingest (0091's own stated internal difference), so
the ingested rows land in a different place in entity space than the build would have put them.
That is *expected* and is not what the equivalence test checks: it checks the masked counts a
principal is served, which is what a client can observe.
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path
from typing import Sequence

from .census import artifact_frame_census, census, compare_census
from .control import Control, wait_for
from .cycle import Cycle, driver_rss, executor_laps, safe
from .holdout import HoldOut, MemberStream, encode_batch, wire_columns
from .publication import (
    EMPTY_ENTITIES,
    MEMBER_RECORD,
    Publication,
    external_ids_b64,
    grouped_by_key,
    in_parent_order,
    json_list,
    json_list_bytes,
    key_order_is_parent_order,
    key_ranges,
    member_columns,
    rank_groups,
)
from .split import (
    base_declaration,
    build_bundle,
    declared_layers,
    filter_parquet,
    in_sorted,
    member_table_columns,
    ranks_file,
    split_entities,
    state_extent,
    write_base_inputs,
)


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="ingest_cycle", description=__doc__.splitlines()[0])
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
        "--all-in-bundle",
        default=None,
        help="the bundle built from every row of the rung: the census reference, and the frame "
        "`--state-extent` states. Default `<rung>/bundle`",
    )
    ap.add_argument(
        "--ingest-config",
        default=None,
        help="a JSON object of `[ingest]` keys written into the served deployment's copy, for a "
        'cell that sweeps a write-path knob: `{"flush_max_age_secs": 5}`. Absent means the '
        "server's own defaults. `{\"ingest_max_batch_bytes\": 262144}` lowers the byte cap the "
        "driver reads back from `/control/status` and splits bodies under",
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
        help="the publication byte cap, clamped to the served deployment's publish_max_body_bytes "
        "(read from /control/status's limits block): a batch is split between artifacts to stay "
        "under it, and an artifact whose whole membership does not fit is published with as many "
        "members as fit and grown by PATCH in slices under it (decision 0127)",
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
    ap.add_argument(
        "--cap-bytes",
        type=int,
        default=None,
        help="`MemoryMax` on every served deployment's transient scope, in bytes. Absent is a "
        "scope with no cap, which on a rung whose bundle does not fit in memory is the box's own "
        "memory as the limit",
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
    # **The result is written whatever happened**, and the exit code says whether the cycle held:
    # a blocked run, a layer that was not published, an unequal census, a write cycle that did not
    # reach its counts, an unexpected refusal or a failed fold each leave a sentence in `failures`.
    Path(args.out).write_text(json.dumps(result, indent=2, default=str))
    print(f"wrote {args.out}")
    for sentence in result["failures"]:
        print(f"FAILED: {sentence}")
    return 1 if result["failures"] else 0
