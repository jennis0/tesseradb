"""The Python worked decode (`reference/examples/decode_viewport.py`) against the golden
fixtures under `clients/ts/core/test/fixtures`, held to the same answer as the JavaScript one by
`clients/ts/wire-example/test/expected.json`.

The example imports nothing of Tessera's and is loaded here by path, so this test does not pull
it into the oracle package. `pyarrow` is the one dependency; without it the whole module skips
with the reason printed rather than passing.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

import pytest

try:
    import pyarrow  # noqa: F401
except ImportError as e:  # pragma: no cover — the loud skip the brief asks for
    print(f"\ntest_wire_example: SKIPPED — pyarrow is not importable ({e}); "
          "run scripts/setup-reference-venv.sh", file=sys.stderr)
    pytest.skip(f"pyarrow is not importable: {e}", allow_module_level=True)

REPO = Path(__file__).resolve().parents[2]
EXAMPLE = REPO / "reference" / "examples" / "decode_viewport.py"
FIXTURES = REPO / "clients" / "ts" / "core" / "test" / "fixtures"
EXPECTED = REPO / "clients" / "ts" / "wire-example" / "test" / "expected.json"


def _load_example():
    spec = importlib.util.spec_from_file_location("decode_viewport_example", EXAMPLE)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    # Registered before execution: `dataclasses` resolves a class's module through `sys.modules`.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


example = _load_example()
expected = {k: v for k, v in json.loads(EXPECTED.read_text()).items() if not k.startswith("_")}


@pytest.mark.parametrize("name", sorted(expected))
def test_decodes_to_the_agreed_frames_counts_and_first_ids(name: str) -> None:
    want = expected[name]
    v = example.decode_viewport((FIXTURES / name).read_bytes())
    assert v.frames == want["frames"]
    assert v.tiles.num_rows == want["tiles"]
    assert (v.sub_cells.num_rows if v.sub_cells is not None else None) == want["sub_cells"]
    assert (v.artifacts.num_rows if v.artifacts is not None else None) == want["artifacts"]
    assert v.point_rows == want["points"]
    assert v.trailer["points"] == want["points"]

    first_point = example.first_rows(v.points, 1)
    got = str(first_point[0]["tessera_id"]) if first_point else None
    assert got == want["first_point_tessera_id"]
    got_artifact = (str(v.artifacts.column("tessera_id")[0].as_py())
                    if v.artifacts is not None else None)
    assert got_artifact == want["first_artifact_tessera_id"]


def test_served_per_tile_sums_to_the_points_delivered() -> None:
    v = example.decode_viewport((FIXTURES / "viewport-plain.bin").read_bytes())
    assert sum(v.tiles.column("served").to_pylist()) == v.point_rows


def test_a_truncated_body_and_a_missing_trailer_are_refused() -> None:
    body = (FIXTURES / "viewport-plain.bin").read_bytes()
    with pytest.raises(ValueError, match="past the end"):
        example.split_frames(body[:-3])
    trailer_len = 5 + len(example.split_frames(body)[-1][1])
    with pytest.raises(ValueError, match="incomplete"):
        example.split_frames(body[:-trailer_len])


def test_an_unknown_frame_kind_is_refused_rather_than_skipped() -> None:
    body = bytearray((FIXTURES / "viewport-plain.bin").read_bytes())
    body[0] = 9
    with pytest.raises(ValueError, match="unknown frame kind 9"):
        example.split_frames(bytes(body))
