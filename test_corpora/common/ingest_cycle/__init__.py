"""The ingest cycle — decision 0091's test, run as a measurement rather than as an assertion.

A rung is built from *all* of its rows. This driver holds a seeded, uniform fraction *f* of the
entities back, builds the complement, serves it, and puts the hold-out through `/control/ingest`
— then flushes, folds, and asks whether the two deployments give the same masked counts. That is
[decision 0091](../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)'s claim
stated as a number instead of a principle: *build is ingest into an empty database*, so a
deployment assembled either way must answer identically.

Which route a layer's membership takes
--------------------------------------

**The base bundle carries the points, the declarations, and the member table of any layer whose
membership the build mints from a column.** The rung's `corpus.toml` is copied with every
`[[layer]]`'s `source` removed, so each layer is declared — kind, levels, visibility rules, content
kinds — and the route its membership takes follows from what it declares:

* **A layer with supplied content** (rung 3's MeSH descriptors, every `clusters/kmeans`) is
  declared and empty at the base: `[layer.members]` is removed with the roster. Its artifacts,
  their memberships and their supplied content are published through
  `PUT /control/layers/{name}/artifacts` **after every point they depend on has been ingested**
  (owner ruling, 2026-09-03). A key naming no artifact yet is minted, and
  `LayerRegistry::resolve_or_mint` refuses to mint on a layer that declares supplied content — an
  artifact served without content its layer declared cannot be told apart from one whose content
  was withheld — so a membership column at either entry point would always arrive first and
  always be refused. Membership arrives with the artifact that holds it, and the driver drops such
  a layer's column from the base points file for the same reason.
* **A layer with no supplied content and no roster** (rung 5's `taxonomy/tree`: an open value set
  with computed content, minted from a list column) keeps `[layer.members]` at the base build,
  over the base's rows only, and its hold-out rows carry the same list on the wire as the ingest
  batch's column named for the layer — one entry per declared level, an unknown key minting the
  artifact that carries its name and the computed content its points give it
  ([decision 0128](../../docs/decisions/0128-a-layer-with-no-supplied-content-travels-as-a-column-at-ingest.md);
  contracts §3.4). Both entry points read that column by one rule (decision 0091), and this is
  where the ingest cycle exercises the mint-from-column path at scale. The member table is read in
  lockstep with the points file, both ascending by entity and a row group at a time, so it is
  never held whole.
* **An attribute-membership layer** (`publishers/source`) carries nothing: its membership is
  evaluated against the indexed column every batch already sends.

An artifact cannot depend on a point that does not exist yet, and that ordering is the only
constraint: it holds at every fraction, so at *f* = 10% the base is 90% of the points and none of
the published artifacts.

⊘ **What this drops, deliberately.** An earlier driver built the base *with* the rung's artifact
roster. That put the layers on the build side of the split and made the *f* = 100% cell impossible
for a layer whose content requires every member visible: an artifact with no members names an empty
generating set, which is satisfied by everyone and is refused at both entry points. A published
artifact names its generating set, so the case does not arise.

What it measures, in order
--------------------------

1. **The split and the base build** — `tessera build --stage-timings-json` over the complement's
   points and the declaration-only `corpus.toml`, so the base's per-stage record is on the same
   schema as the whole-corpus build's.
2. **Online ingest** — Arrow IPC batches of 10,000 rows at *C* concurrent callers, `items/s`
   acked, ack p50/p99, and every refusal counted by status (429 backpressure, 409 duplicate or
   batch-id conflict, 422 bounds or contract). The batches carry the points, their labels as a
   list, and the member list of any layer on the column route — see above.
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
        "--copy-base",
        action="store_true",
        help="serve a copy of the base bundle under the scratch instead of the base itself, so a "
        "cell that publishes does not change the base the next `--reuse-base` cell starts from",
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
    ap.add_argument("--flush-timeout", type=float, default=900.0)
    ap.add_argument("--fold-timeout", type=float, default=7200.0)
    args = ap.parse_args(argv)
    started = time.time()
    result = Cycle(args).run()
    result["ran_s"] = round(time.time() - started, 1)
    # `VmHWM` of the driver itself, whichever phase set it: the split, the hold-out's batches,
    # the publication's buckets or the census. Beside the server's own fold peak in the cell.
    result["driver_peak_rss"] = driver_rss()["peak"]
    Path(args.out).write_text(json.dumps(result, indent=2, default=str))
    print(f"wrote {args.out}")
    return 0
