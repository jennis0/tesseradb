"""The ingest cycle: build is ingest into an empty database, checked as a measurement.

Holds back a seeded fraction of the entities, builds the complement as the base, serves it, and
ingests the hold-out through `/control/ingest`. After flush and fold, checks whether the two
assemblies give the same masked counts.

A layer's membership takes one of three routes, by the shape of its member table:

* One row per (artifact, entity): declared and empty at the base, and published only after every
  point it names has been ingested.
* One row per point, one entry per declared level: read beside the points file at a build, and an
  ingest batch's column at a running service. A null entry means no membership at that level.
  Where the layer supplies content, its roster is published with empty member lists before the
  ingest, because a key that names no artifact is refused rather than minted without its content.
* Attribute-membership: evaluated against an indexed column every batch already sends.

What it measures, in order:

1. The split and the base build.
2. Online ingest: batches at N concurrent callers, ack latency, refusals by status.
3. Publication: every layer's roster, member sets, ranked content and parent list.
4. Flush: wall time and time to visibility.
5. The fold: wall and RSS, read from `/control/status`.
6. Equivalence: masked counts against the all-in build, per view and per layer.
7. The write cycle: deletes, suppressions, re-ingests, another fold and census, then a restart
   that must answer the same counts.

A hold-out is not a random sample: entity ids are assigned differently at ingest than at a build,
so equivalence compares masked counts, not ids.
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
    ranks_for,
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
    ap.add_argument(
        "--ranks",
        default=None,
        help="the principal ladder's ranks file. Default the rung's own, or one derived into "
        "--work from each view's point_visibility field where the rung has none",
    )
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
    # `VmHWM` of the driver itself, whichever phase set it, beside the server's own fold peak.
    result["driver_peak_rss"] = driver_rss()["peak"]
    # The result is written whatever happened; the exit code says whether the cycle held. A
    # blocked run, an unpublished layer, an unequal census, a write cycle short of its counts, an
    # unexpected refusal or a failed fold each leave a sentence in `failures`.
    Path(args.out).write_text(json.dumps(result, indent=2, default=str))
    print(f"wrote {args.out}")
    for sentence in result["failures"]:
        print(f"FAILED: {sentence}")
    return 1 if result["failures"] else 0
