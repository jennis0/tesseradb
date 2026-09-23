"""Declarations the build refuses, driven through the release CLI.

Each refused schema must fail the build. The positive control builds the same corpus under a
well-formed schema whose one attribute sets neither `index` nor `render`, and must succeed and
write the record blob's base files. Each refused schema differs from the control's by one line,
and the corpus has a column for every name any schema declares, so the build has no reason to
refuse other than that line.
"""

from __future__ import annotations

import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from oracle.harness import run_build, write_deployment

# Fixed, like the catalogue's: a refusal test has no served order to care about, but minting
# would make the control build's receipt-free artefacts differ per run for no reason.
ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"

N = 8


@pytest.fixture(scope="module")
def corpus_dir(tmp_path_factory) -> Path:
    """One tiny corpus for every case: 8 items, one term, a `margin` column for the control
    schema's attribute, and a `record` column so the reserved-name case names a column that
    exists."""
    work = tmp_path_factory.mktemp("schema-refusals")
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N), type=pa.uint64()),
                "x": pa.array([10.0 * (i + 1) for i in range(N)], type=pa.float32()),
                "y": pa.array([10.0 * (i + 1) for i in range(N)], type=pa.float32()),
                "margin": pa.array([i * 3 for i in range(N)], type=pa.uint32()),
                "record": pa.array([i * 5 for i in range(N)], type=pa.uint32()),
            }
        ),
        work / "points.parquet",
    )
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N), type=pa.uint64()),
                "term_id": pa.array([0] * N, type=pa.uint32()),
            }
        ),
        work / "pairs.parquet",
    )
    return work


# Every case's schema declares the same corpus, the same view and the same frame; only the
# attribute half differs. The points file carries identity, geometry and the attribute columns, so
# the view and every attribute name one source; `[sources]` writes each path once, relative to the
# declaration (configuration.md §3).
SCHEMA_HEAD = """\
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 100.0 }
point_visibility = { source = "pairs", default = "public" }

"""


def _build(corpus_dir: Path, schema_text: str, out: Path):
    # Written into the corpus directory, because a `source` is a path relative to the document
    # that declares it and these two files are what it names.
    schema_path = corpus_dir / f"{out.name}.config.toml"
    schema_path.write_text(SCHEMA_HEAD + schema_text)
    deployment = write_deployment(
        corpus_dir / f"{out.name}.tessera.toml", bundle=out, schema=schema_path
    )
    return run_build(["--deployment", str(deployment)], key_hex=ID_KEY_HEX)


# (case name, schema). Each schema is the control's with one line added or changed.
REFUSALS = [
    (
        "multi_is_refused",
        """\
[[attribute]]
name  = "margin"
type  = "u32"
multi = true
""",
    ),
    (
        "record_is_a_reserved_name",
        """\
[[attribute]]
name = "record"
type = "u32"
""",
    ),
    (
        "a_stale_used_for_key_is_refused",
        """\
[[attribute]]
name     = "margin"
type     = "u32"
used_for = ["filter"]
""",
    ),
]


@pytest.mark.parametrize("name,schema", REFUSALS, ids=[r[0] for r in REFUSALS])
def test_a_refused_schema_fails_the_build(corpus_dir: Path, tmp_path: Path, name: str, schema: str):
    result = _build(corpus_dir, schema, tmp_path / "bundle")
    assert result.returncode != 0, (
        f"{name}: the build accepted the schema\nstdout: {result.stdout}"
    )


def test_a_neither_key_column_builds_green_and_is_blob_resident(
    corpus_dir: Path, tmp_path: Path
):
    """The positive control, and gate 2's parse half: the old "must contain render or filter"
    refusal is deleted, not reworded — a neither-key declaration parses, builds, and lands in
    the record blob, whose three base files the build must write (records §3, §7)."""
    out = tmp_path / "bundle"
    result = _build(
        corpus_dir,
        """\
[[attribute]]
name = "margin"
type = "u32"
""",
        out,
    )
    assert result.returncode == 0, (
        f"the control build failed, so every refusal above may be the corpus's rather than the "
        f"schema's\nstderr: {result.stderr}"
    )
    prefix = json.loads((out / "CURRENT").read_text())["prefix"]
    record_dir = out / prefix / "partitions" / "default" / "attrs" / "record"
    for filename in ("blocks.bin", "hasrow.roaring", "directory.arrow"):
        assert (record_dir / filename).is_file(), (
            f"the build succeeded but wrote no {filename} — the neither-key column was "
            "tolerated, not stored (records §3: the record always exists)"
        )
