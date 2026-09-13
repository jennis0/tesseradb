"""The record blob's addressing, walked for the **one** artefact-level check the design
licenses (records §3/§10, review B7): rank, offsets, block bounds, discriminants.

The conformance relation for *values* is the fixture's inputs, never this artefact — the shipped
oracle deliberately does not open `attrs/` and this module does not change that. What it decodes
is **structure**: the block directory, the zstd block framing, the row framing and the field
framing, because addressing self-consistency is the one property the fixture cannot see from
either side — a mis-addressed blob still serves *some* value, and B6's fail-closed refusals rest
on the addressing being checkable at all. Accordingly, nothing here returns a field's value:
[`self_check`] frames a field to find where it ends and discards the bytes. A function that
decoded values out of this artefact would be the suite acquiring a second value relation beside
the generation functions, which is precisely the weaker construction records §3 declines.

The one fixture-facing comparison this module accepts (`expected_entities`) is the *strong*
direction: the has-row bitmap checked against what the generation functions planted, artefact
against fixture input, not artefact against itself.

Two futures already lean on this walk, which is why it lives in `oracle/` rather than inline in
a test: the fold byte-absence check (a folded blob must contain no byte of a blanked entity's
row — walkable with exactly these framing functions), and the coalesced-extent catalogue entries
(an extent blob has the same three files and the same addressing). Both land with the blob
lifecycle; neither changes the licence above.

Format, transcribed from `tessera-filter`'s `record` module (the byte format's single home): a
block is `row_count u32 LE | first_rank u32 LE | first_entity u32 LE`, then one LEB128 varint per
row past the first holding that row's entity less its predecessor's less one, then the rows; a row
is a LEB128 varint stating how many bytes of fields follow, then those fields; a field is
`tag u16 LE | kind u8 | value`. Rows concatenate in ascending entity order into zstd blocks cut
against a 256 KiB uncompressed target measured over the rows, a row never splitting — an oversized
row gets an oversized block of its own. The directory is one Arrow IPC file batch per blob:
`(compressed_offset, compressed_len, uncompressed_len, first_rank, row_count)`, and
`hasrow.roaring` is the portable-serialised has-row bitmap whose rank order is the row order.

**The delimiters are the block's own bytes**, which is what this walk turns on
([decision 0142](../../docs/decisions/0142-the-record-blob-delimits-a-row-by-a-length-the-row-states.md)).
The directory holds nothing per row, so there is no second file for it to address past; what
replaces that cross-check is the tiling walk — stepping over exactly `row_count` rows by the
lengths they state must land on the block's last byte, and every length must stay inside the
block on the way. A rows section re-tiled so that both still hold is the class this cannot catch,
and the manifest's digest over `blocks.bin` is what stands against it.
"""

from __future__ import annotations

import json
import struct
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc
from pyroaring import BitMap

# The blob's three base files, under `attrs/record/` (records §7).
BLOCKS_FILE = "blocks.bin"
HASROW_FILE = "hasrow.roaring"
DIRECTORY_FILE = "directory.arrow"

# The uncompressed block target (records §3), measured over a block's rows and not its header.
# Transcribed, like the field kinds below: the format has one home in Rust and this is its checked
# shadow — a drift fails [`self_check`] loudly.
BLOCK_TARGET = 256 * 1024

# A block's fixed header: row count, first rank, first entity.
BLOCK_HEADER_FIXED = 12


def _take_varint(block: bytes, pos: int, limit: int) -> tuple[int, int]:
    """One LEB128 varint at `pos`, bounded by `limit`; returns `(value, new position)`. Raises
    `ValueError` on a truncated, over-wide or redundantly padded varint, which the Rust decoder
    refuses in the same three places. Both the entity gaps and the length that heads a row are
    read through here."""
    value = 0
    shift = 0
    while True:
        if pos >= limit:
            raise ValueError("a block ends in the middle of a varint")
        byte = block[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if byte & 0x80 == 0:
            if shift > 0 and byte == 0:
                raise ValueError("a block's varint is padded with a redundant byte")
            if value > 0xFFFFFFFF:
                raise ValueError("a block's varint does not fit u32")
            return value, pos
        shift += 7
        if shift >= 32:
            raise ValueError("a block's varint does not fit u32")

# kind byte → fixed value width; the two variable-width kinds are handled by name.
_FIXED_KIND_WIDTHS = {
    0: 1,  # bool — one byte, 0 or 1, anything else refuses
    1: 1,  # u8
    2: 2,  # u16
    3: 4,  # u32
    4: 8,  # u64
    5: 1,  # i8
    6: 2,  # i16
    7: 4,  # i32
    8: 8,  # i64
    9: 4,  # f32
    10: 8,  # f64
    11: 8,  # timestamp_us
}
_KIND_UTF8 = 12
_KIND_LIST = 13


def record_dir_of(bundle_root: Path) -> Path:
    """`attrs/record/` under the bundle's current prefix (Phase 1: one partition, `default`)."""
    current = json.loads((bundle_root / "CURRENT").read_text())
    return bundle_root / current["prefix"] / "partitions" / "default" / "attrs" / "record"


def _frame_value(payload: bytes, pos: int, kind: int, where: str, failures: list[str]) -> int:
    """Advance past one value of `kind` at `pos`, appending any framing failure; returns the new
    position, or `len(payload)` after an unrecoverable failure (which abandons the row)."""
    if kind in _FIXED_KIND_WIDTHS:
        width = _FIXED_KIND_WIDTHS[kind]
        if pos + width > len(payload):
            failures.append(f"{where}: kind {kind} value overruns the payload")
            return len(payload)
        if kind == 0 and payload[pos] not in (0, 1):
            failures.append(f"{where}: bool byte is {payload[pos]}, not 0 or 1")
        return pos + width
    if kind == _KIND_UTF8:
        if pos + 4 > len(payload):
            failures.append(f"{where}: utf8 length prefix overruns the payload")
            return len(payload)
        (byte_len,) = struct.unpack_from("<I", payload, pos)
        pos += 4
        if pos + byte_len > len(payload):
            failures.append(f"{where}: utf8 bytes overrun the payload")
            return len(payload)
        try:
            payload[pos : pos + byte_len].decode("utf-8")
        except UnicodeDecodeError:
            failures.append(f"{where}: utf8 value is not valid UTF-8")
        return pos + byte_len
    failures.append(f"{where}: unknown kind {kind}")
    return len(payload)


def _walk_payload(payload: bytes, where: str, failures: list[str]) -> set[int]:
    """Frame every field of one row's payload; returns the tags seen. The payload must be
    consumed exactly — a trailing byte is an addressing defect wearing a value's clothes."""
    pos = 0
    tags: set[int] = set()
    while pos < len(payload):
        if pos + 3 > len(payload):
            failures.append(f"{where}: field header overruns the payload")
            return tags
        (tag,) = struct.unpack_from("<H", payload, pos)
        kind = payload[pos + 2]
        pos += 3
        if tag in tags:
            failures.append(f"{where}: duplicate tag {tag} in one row")
        tags.add(tag)
        if kind == _KIND_LIST:
            # Specified now, populated in the multi-value epic: elements carry the value
            # encoding only, and an element kind of `list` refuses — one level is the model.
            if pos + 5 > len(payload):
                failures.append(f"{where}: list header overruns the payload")
                return tags
            elem_kind = payload[pos]
            (count,) = struct.unpack_from("<I", payload, pos + 1)
            pos += 5
            if elem_kind == _KIND_LIST:
                failures.append(f"{where}: a list element kind of list — one level is the model")
                return tags
            for i in range(count):
                pos = _frame_value(payload, pos, elem_kind, f"{where} list[{i}]", failures)
        else:
            pos = _frame_value(payload, pos, kind, f"{where} tag {tag}", failures)
    return tags


def block_headers(record_dir: Path) -> list[dict]:
    """Each block's header facts, decompressed and parsed: `first_rank`, `first_entity`,
    `row_count` and `rows_len` — the last being the block's bytes past its header, which is what
    the 256 KiB target is measured over.

    Separate from [`self_check`] because it answers a question rather than collecting failures: a
    test asking which blocks passed the target needs the number, not a list of drifts.
    """
    directory_path = record_dir / DIRECTORY_FILE
    with ipc.open_file(directory_path) as reader:
        directory = reader.read_all().to_pylist()
    blocks_bytes = (record_dir / BLOCKS_FILE).read_bytes()
    zstd = pa.Codec("zstd")
    headers = []
    for entry in directory:
        at = entry["compressed_offset"]
        compressed = blocks_bytes[at : at + entry["compressed_len"]]
        block = zstd.decompress(
            pa.py_buffer(compressed), entry["uncompressed_len"], asbytes=True
        )
        row_count, first_rank, first_entity = struct.unpack_from("<III", block, 0)
        pos = BLOCK_HEADER_FIXED
        for _ in range(1, row_count):
            _, pos = _take_varint(block, pos, len(block))
        headers.append(
            {
                "row_count": row_count,
                "first_rank": first_rank,
                "first_entity": first_entity,
                "rows_len": len(block) - pos,
                "uncompressed_len": entry["uncompressed_len"],
            }
        )
    return headers


def self_check(
    record_dir: Path,
    *,
    expected_entities: set[int] | None = None,
    allowed_tags: set[int] | None = None,
    block_target: int = BLOCK_TARGET,
) -> list[str]:
    """Walk one blob's whole addressing — directory against blocks, block headers against both,
    ranks against has-row, row framing against block bounds, field framing against row bounds —
    and collect every failure rather than the first (the `verify()` pattern: one run reports
    every drift).

    `expected_entities` is the fixture-input half: the has-row membership the generation
    functions imply. `allowed_tags` pins which manifest positions may appear in a row — the
    blob-resident columns' — so a hot-column or entity-space value leaking into the blob fails
    here rather than doubling storage silently.
    """
    failures: list[str] = []
    blocks_path = record_dir / BLOCKS_FILE
    hasrow_path = record_dir / HASROW_FILE
    directory_path = record_dir / DIRECTORY_FILE
    for path in (blocks_path, hasrow_path, directory_path):
        if not path.is_file():
            failures.append(f"{path.name} is missing from {record_dir}")
    if failures:
        return failures

    hasrow = BitMap.deserialize(hasrow_path.read_bytes())
    entities_in_rank_order = list(hasrow)
    with ipc.open_file(directory_path) as reader:
        directory = reader.read_all().to_pylist()
    blocks_bytes = blocks_path.read_bytes()

    if expected_entities is not None and set(entities_in_rank_order) != expected_entities:
        extra = sorted(set(entities_in_rank_order) - expected_entities)[:5]
        missing = sorted(expected_entities - set(entities_in_rank_order))[:5]
        failures.append(
            f"has-row disagrees with the generation functions: {len(extra)}+ unexpected "
            f"(first {extra}), {len(missing)}+ missing (first {missing})"
        )

    zstd = pa.Codec("zstd")
    cursor = 0  # the next block's compressed offset — blocks must tile the file exactly
    rank = 0  # the next row's rank — blocks must tile the rank space exactly
    previous_entity = -1
    for index, entry in enumerate(directory):
        where = f"block {index}"
        if entry["compressed_offset"] != cursor:
            failures.append(
                f"{where}: compressed_offset {entry['compressed_offset']} != {cursor} — "
                "blocks do not tile blocks.bin"
            )
        if entry["first_rank"] != rank:
            failures.append(
                f"{where}: first_rank {entry['first_rank']} != {rank} — blocks do not tile "
                "the rank space"
            )
        compressed = blocks_bytes[
            entry["compressed_offset"] : entry["compressed_offset"] + entry["compressed_len"]
        ]
        if len(compressed) != entry["compressed_len"]:
            failures.append(f"{where}: compressed extent overruns blocks.bin")
            return failures
        try:
            block = zstd.decompress(
                pa.py_buffer(compressed), entry["uncompressed_len"], asbytes=True
            )
        except (OSError, pa.ArrowException) as e:
            failures.append(f"{where}: zstd refuses: {e}")
            return failures
        if len(block) != entry["uncompressed_len"]:
            failures.append(
                f"{where}: decompressed to {len(block)} bytes, directory says "
                f"{entry['uncompressed_len']}"
            )

        stated_rows = entry["row_count"]
        if not stated_rows:
            failures.append(f"{where}: a block with no rows")
            continue

        # The block header, and everything the other two files say about this block.
        if len(block) < BLOCK_HEADER_FIXED:
            failures.append(f"{where}: {len(block)} bytes cannot hold a block header")
            continue
        row_count, first_rank, first_entity = struct.unpack_from("<III", block, 0)
        if row_count != stated_rows:
            failures.append(
                f"{where}: states {row_count} rows where the directory addresses {stated_rows}"
            )
            continue
        if first_rank != entry["first_rank"]:
            failures.append(
                f"{where}: states first rank {first_rank} where the directory places it at "
                f"{entry['first_rank']}"
            )
        # The gaps, walked to recover every row's entity and to find where the rows begin.
        entities: list[int] = [first_entity]
        pos = BLOCK_HEADER_FIXED
        try:
            for _ in range(1, row_count):
                gap, pos = _take_varint(block, pos, len(block))
                entities.append(entities[-1] + gap + 1)
        except ValueError as e:
            failures.append(f"{where}: {e}")
            continue
        rows_at = pos
        rows_len = len(block) - rows_at
        # **The tiling walk**, which is what the directory's row offsets used to be and what the
        # extent digest used to check. Each row states how many bytes of fields follow it; walking
        # exactly `row_count` of them must land on the block's last byte, and no length may leave
        # the block on the way.
        extents: list[tuple[int, int]] = []
        at = rows_at
        tiled = True
        try:
            for _ in range(row_count):
                length, after = _take_varint(block, at, len(block))
                if length == 0:
                    failures.append(
                        f"{where}: a row states no fields — it should have no row at all"
                    )
                    tiled = False
                    break
                if after + length > len(block):
                    failures.append(
                        f"{where}: a row states {length} bytes of fields, which run past the "
                        f"{len(block)}-byte block"
                    )
                    tiled = False
                    break
                extents.append((after - rows_at, after + length - rows_at))
                at = after + length
        except ValueError as e:
            failures.append(f"{where}: {e}")
            continue
        if not tiled:
            continue
        if at != len(block):
            failures.append(
                f"{where}: {rows_len} row bytes but its {row_count} rows end at {at - rows_at} — "
                "a block carries nothing but whole rows"
            )
            continue
        if rank < len(entities_in_rank_order) and first_entity != entities_in_rank_order[rank]:
            failures.append(
                f"{where}: states first entity {first_entity} but has-row's rank-{rank} member "
                f"is {entities_in_rank_order[rank]} — the bitmap and the blocks disagree about "
                "which entity a rank names"
            )
        if row_count > 1 and rows_len > block_target:
            failures.append(
                f"{where}: {rows_len} row bytes exceed the {block_target} target across "
                f"{row_count} rows — only a single oversized row may pass the target"
            )
        for within, ((start, end), entity) in enumerate(zip(extents, entities)):
            row_where = f"{where} row {within} (rank {rank})"
            if entity > 0xFFFFFFFF:
                failures.append(f"{row_where}: entity {entity} is past the u32 ceiling")
                break
            if entity <= previous_entity:
                failures.append(
                    f"{row_where}: entity {entity} after {previous_entity} — rows must be in "
                    "ascending entity order"
                )
            previous_entity = entity
            if rank >= len(entities_in_rank_order):
                failures.append(f"{row_where}: more rows than has-row members")
                break
            if entity != entities_in_rank_order[rank]:
                failures.append(
                    f"{row_where}: the block says entity {entity} but has-row's rank-{rank} "
                    f"member is {entities_in_rank_order[rank]} — rank addressing would serve a "
                    "neighbour's row"
                )
            tags = _walk_payload(block[rows_at + start : rows_at + end], row_where, failures)
            if not tags:
                failures.append(f"{row_where}: a row with no fields — it should have no row")
            if allowed_tags is not None and not tags <= allowed_tags:
                failures.append(
                    f"{row_where}: tags {sorted(tags - allowed_tags)} are not blob-resident "
                    "columns — a hot-column or entity-space value has leaked into the blob"
                )
            rank += 1
        cursor = entry["compressed_offset"] + entry["compressed_len"]

    if cursor != len(blocks_bytes):
        failures.append(
            f"directory covers {cursor} of blocks.bin's {len(blocks_bytes)} bytes — trailing "
            "bytes no block accounts for"
        )
    if rank != len(entities_in_rank_order):
        failures.append(
            f"blocks hold {rank} rows, has-row has {len(entities_in_rank_order)} members"
        )
    return failures
