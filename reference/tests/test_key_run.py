"""The oracle's key run reader against `reference/vectors/key_run.json`: runs with a key filter,
stored byte for byte as `mosaica_store::key_index` writes them (its test
`a_filtered_run_is_stored_as_the_shared_vectors_say` checks the same bytes)."""

from __future__ import annotations

import json
import struct
import zlib
from pathlib import Path

import pytest

from oracle.bundle import KEY_FILTER_BLOCK, key_filter_passes, read_key_run

VECTORS = json.loads((Path(__file__).resolve().parents[1] / "vectors" / "key_run.json").read_text())
RUNS = VECTORS["runs"]


def run_bytes(run: dict) -> bytearray:
    data = bytearray(run["length"])
    for offset, hexed in run["bytes"]:
        chunk = bytes.fromhex(hexed)
        data[offset : offset + len(chunk)] = chunk
    return data


def filter_span(data: bytes) -> tuple[int, int, int]:
    """The filter's first byte, the byte after its blocks, and its hash count."""
    offset, blocks, hashes = struct.unpack_from("<QQI", data, 64)
    return offset, offset + blocks * KEY_FILTER_BLOCK, hashes


def write(tmp_path: Path, data: bytes) -> Path:
    path = tmp_path / "run.keys"
    path.write_bytes(bytes(data))
    return path


@pytest.mark.parametrize("run", RUNS, ids=lambda run: f"{run['key_width']}-byte keys")
def test_a_vector_run_reads_back_its_entries(tmp_path, run):
    entries = [(int(key, 16), entity) for key, entity in run["entries"]]
    assert read_key_run(write(tmp_path, run_bytes(run))) == entries


@pytest.mark.parametrize("run", RUNS, ids=lambda run: f"{run['key_width']}-byte keys")
def test_the_filter_passes_every_key_and_rejects_most_others(run):
    data = run_bytes(run)
    start, end, hashes = filter_span(data)
    blocks = bytes(data[start:end])
    held = {int(key, 16) for key, _ in run["entries"]}
    assert all(key_filter_passes(blocks, hashes, key) for key in held)
    others = [key * 7 + 3 for key in range(20_000) if key * 7 + 3 not in held]
    passed = sum(key_filter_passes(blocks, hashes, key) for key in others)
    assert passed < len(others) // 20


@pytest.mark.parametrize("run", RUNS, ids=lambda run: f"{run['key_width']}-byte keys")
def test_a_damaged_or_truncated_filter_is_refused(tmp_path, run):
    data = run_bytes(run)
    start, end, _ = filter_span(data)
    for at in (start, end - 1, end + 1, start - 1):
        damaged = bytearray(data)
        damaged[at] ^= 0x10
        with pytest.raises(ValueError):
            read_key_run(write(tmp_path, damaged))
    for cut in (len(data) - 1, end, start):
        with pytest.raises(ValueError):
            read_key_run(write(tmp_path, data[:cut]))

    cleared = bytearray(data)
    cleared[start:end] = bytes(end - start)
    cleared[end : end + 4] = struct.pack("<I", zlib.crc32(cleared[start:end]))
    with pytest.raises(ValueError):
        read_key_run(write(tmp_path, cleared))
