"""The declaration surface's refusal catalogue, driven through the real CLI (records §2).

`tessera-build` has unit tests for each refusal; this module holds the *conformance* half: the
release binary, a real invocation, and the assertion that each schema records §2 refuses fails
the build **naming its reason** — decision 0013's discipline, refusals naming what is absent or
which ruling fences them, so an operator reading the message can find the authority rather than
a "not supported". An implementation that kept the parse but returned success, or refused with a
bare error, keeps the machinery while dropping the contract; this is the test that fails it.

What each case pins, beyond the non-zero exit:

- `multi = true` — unbuilt, names records §5 (accepting it would store one value per item under
  a declaration promising several);
- `render` with `multi = true` — the **permanent** fence, names decision 0039, and must win over
  the bare-`multi` refusal so a caller who set both hears the fence that survives the epic that
  lifts the other;
- `index` on a rendered number — store-once would serve the filter from a hot column that cannot
  express absence; names decision 0064, whose render half is the restoration path;
- a column named `record` — `attrs/record/` is the blob's namespace (review N10), so the name is
  reserved;
- a stale `used_for` key — the retired surface refuses loudly (decision 0048's shape: replaced,
  not aliased), naming the key so a migrating operator sees *what* is stale rather than a parse
  position.

The positive control builds the same corpus under a well-formed schema whose one attribute sets
**neither** key — the shape the old surface refused — and must succeed *and* write the record
blob's base files: the refusals above are then the schemas' own, not the corpus's, and the
neither-key column is blob-resident rather than merely tolerated (records §3).
"""

from __future__ import annotations

import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from oracle.harness import run_build

# Fixed, like the catalogue's: a refusal test has no served order to care about, but minting
# would make the control build's receipt-free artefacts differ per run for no reason.
ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"

EXTENT_ARG = "0,100,0,100"
N = 8


@pytest.fixture(scope="module")
def corpus_dir(tmp_path_factory) -> Path:
    """One tiny corpus for every case: 8 items, one term, and a `margin` column so the control
    schema's declared attribute has values to store."""
    work = tmp_path_factory.mktemp("schema-refusals")
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N), type=pa.uint64()),
                "x": pa.array([10.0 * (i + 1) for i in range(N)], type=pa.float32()),
                "y": pa.array([10.0 * (i + 1) for i in range(N)], type=pa.float32()),
                "margin": pa.array([i * 3 for i in range(N)], type=pa.uint32()),
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


def _build(corpus_dir: Path, schema_text: str, out: Path):
    schema_path = out.parent / f"{out.name}.schema.toml"
    schema_path.write_text(schema_text)
    return run_build(
        [
            "--points",
            str(corpus_dir / "points.parquet"),
            "--pairs",
            str(corpus_dir / "pairs.parquet"),
            "--schema",
            str(schema_path),
            "--extent",
            EXTENT_ARG,
            "--slice",
            "s0",
            "--out",
            str(out),
            "--id-key",
            ID_KEY_HEX,
        ]
    )


# (case name, schema, the fragments the refusal must contain). Fragments are chosen to be the
# *reason's name* — the section or decision the message cites, plus the attribute it blames —
# not the message's full prose, which is the Rust side's to word.
REFUSALS = [
    (
        "multi_is_unbuilt",
        """\
[[attribute]]
name  = "margin"
type  = "u32"
index = true
multi = true
""",
        ["records §5", "margin"],
    ),
    (
        "render_with_multi_names_the_permanent_fence",
        """\
[[attribute]]
name   = "margin"
type   = "u32"
render = true
multi  = true
""",
        # 0039 and not records §5: both refusals apply to this declaration, and the caller must
        # hear the one that survives the epic that lifts the other.
        ["0039", "margin"],
    ),
    (
        "index_on_a_rendered_number",
        """\
[[attribute]]
name   = "margin"
type   = "u32"
render = true
index  = true
""",
        ["0064", "margin"],
    ),
    (
        "record_is_a_reserved_name",
        """\
[[attribute]]
name  = "record"
type  = "u32"
index = true
""",
        ["record", "reserved"],
    ),
    (
        "a_stale_used_for_key_refuses_loudly",
        """\
[[attribute]]
name     = "margin"
type     = "u32"
used_for = ["filter"]
""",
        ["used_for"],
    ),
]


@pytest.mark.parametrize("name,schema,fragments", REFUSALS, ids=[r[0] for r in REFUSALS])
def test_a_refused_schema_fails_the_build_naming_its_reason(
    corpus_dir: Path, tmp_path: Path, name: str, schema: str, fragments: list[str]
):
    result = _build(corpus_dir, schema, tmp_path / "bundle")
    assert result.returncode != 0, (
        f"{name}: the build accepted a schema records §2 refuses\nstdout: {result.stdout}"
    )
    output = result.stderr + result.stdout
    for fragment in fragments:
        assert fragment in output, (
            f"{name}: the refusal does not name {fragment!r} (decision 0013 — a refusal names "
            f"what is absent or which ruling fences it)\noutput: {output}"
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
