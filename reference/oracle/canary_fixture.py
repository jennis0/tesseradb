"""Synthesise a tiny points+pairs parquet pair for `test_canary.py`'s I2 scaffold, and build the
canary / canary-free bundle pair from them via the CLI (Task 15, brief step 3).

Deliberately independent of the 250k Phase 0 corpus: a small, from-scratch synthetic dataset is
easier to reason a canary term is genuinely held by *no* tested principal, and keeps the two
builds (canary, canary-free) fast enough to run twice in one test.
"""

from __future__ import annotations

import random
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from .harness import CLI_BIN, REPO_ROOT, ensure_cli_built

N_BASE_ITEMS = 400
N_TERMS = 6
EXTENT = "0,65536,0,65536"
SLICE_ID = "s0"
SEED = 20260729

# The canary's own term id — deliberately one past the base terms, and never granted to any
# session `test_canary.py` authorises.
CANARY_TERM_ID = N_TERMS

# Extreme corner of the quantisation extent (contracts §2.5: v == max lands in the top cell).
CANARY_X = 65535.9
CANARY_Y = 65535.9


def _base_dataset(rng: random.Random) -> tuple[list[tuple[int, float, float]], list[tuple[int, int]]]:
    points = []
    pairs = []
    for entity_id in range(N_BASE_ITEMS):
        x = rng.uniform(0.0, 65536.0)
        y = rng.uniform(0.0, 65536.0)
        points.append((entity_id, x, y))
        # Each item carries 1-2 of the N_TERMS base terms.
        n_terms = rng.choice([1, 1, 2])
        terms = rng.sample(range(N_TERMS), n_terms)
        for t in terms:
            pairs.append((entity_id, t))
    return points, pairs


def _write_points(path: Path, points: list[tuple[int, float, float]]) -> None:
    table = pa.table(
        {
            "entity_id": pa.array([p[0] for p in points], type=pa.uint64()),
            "x": pa.array([p[1] for p in points], type=pa.float32()),
            "y": pa.array([p[2] for p in points], type=pa.float32()),
        }
    )
    pq.write_table(table, path)


def _write_pairs(path: Path, pairs: list[tuple[int, int]]) -> None:
    table = pa.table(
        {
            "entity_id": pa.array([p[0] for p in pairs], type=pa.uint64()),
            "term_id": pa.array([p[1] for p in pairs], type=pa.uint32()),
        }
    )
    pq.write_table(table, path)


def build_canary_pair(work_dir: Path) -> tuple[Path, Path]:
    """Write the two synthetic input pairs and build both bundles under `work_dir`.

    Returns `(canary_free_bundle, canary_bundle)`.
    """
    ensure_cli_built()
    rng = random.Random(SEED)
    points, pairs = _base_dataset(rng)

    free_points_path = work_dir / "free-points.parquet"
    free_pairs_path = work_dir / "free-pairs.parquet"
    _write_points(free_points_path, points)
    _write_pairs(free_pairs_path, pairs)

    canary_points = points + [(N_BASE_ITEMS, CANARY_X, CANARY_Y)]
    canary_pairs = pairs + [(N_BASE_ITEMS, CANARY_TERM_ID)]
    canary_points_path = work_dir / "canary-points.parquet"
    canary_pairs_path = work_dir / "canary-pairs.parquet"
    _write_points(canary_points_path, canary_points)
    _write_pairs(canary_pairs_path, canary_pairs)

    free_bundle = work_dir / "bundle-free"
    canary_bundle = work_dir / "bundle-canary"

    import subprocess

    for points_path, pairs_path, out_dir in (
        (free_points_path, free_pairs_path, free_bundle),
        (canary_points_path, canary_pairs_path, canary_bundle),
    ):
        subprocess.run(
            [
                str(CLI_BIN),
                "build",
                "--points",
                str(points_path),
                "--pairs",
                str(pairs_path),
                "--extent",
                EXTENT,
                "--slice",
                SLICE_ID,
                "--out",
                str(out_dir),
                # Contracts r6 refuses to build unless a human names the identity key's
                # lineage. Each canary fixture (free and canary bundles alike) is a genuinely
                # new, from-scratch lineage every run (`oracle.harness.ensure_fixture_bundle`'s
                # module doc makes the same call for the main fixture) — naming
                # `--mint-id-key` here satisfies the rule rather than working around it.
                "--mint-id-key",
            ],
            cwd=REPO_ROOT,
            check=True,
        )

    return free_bundle, canary_bundle
