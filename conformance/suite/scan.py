"""Byte windows the entity-id sweeps compare against their targets: `test_byte_scan.py` over the
catalogue and `test_two_modes.py` over every many-shard answer."""

from __future__ import annotations

import base64


def le_windows(data: bytes, width: int, *, stride: int = 1) -> set[int]:
    """Every `width`-byte little-endian window's integer value at every offset that is a multiple
    of `stride`, as a set (dedup — we only care about membership, not position or count).

    `stride` matters a great deal for this fixture. `stride=1` (used for arbitrary unstructured
    bytes such as a text log) is a genuine sliding window: correct when nothing is known about
    alignment. But for a *column buffer* known to be a native fixed-width array, `stride=1` would
    be actively wrong — it would manufacture byte-straddled values out of two adjacent, harmless
    array elements. Every column scan in `test_byte_scan.py` uses `stride` equal to that column's
    own native element width — 8 for `mosaica_id`; 4, deliberately, for `code`, whose own width is
    8 but whose half-offset windows are the only ones that see a value straddling two adjacent
    positions (`_points_value_buffer_windows` there). So only offsets that could carry a stored
    value, or a value smuggled across two of them, are considered."""
    n = len(data)
    if n < width:
        return set()
    return {int.from_bytes(data[i : i + width], "little") for i in range(0, n - width + 1, stride)}


def value_buffer(column, width: int) -> bytes:
    """A fixed-width column's value buffer, trimmed of Arrow's alignment padding, which would
    otherwise read as windows of zero."""
    buf = [b for b in column.buffers() if b is not None][-1]
    return buf.to_pybytes()[: len(column) * width]


def cursor_windows(cursor: str) -> set[int]:
    """Every 8-byte little-endian window of a cursor's decoded bytes, at every offset: the cursor
    is opaque, so no alignment can be assumed."""
    return le_windows(base64.urlsafe_b64decode(cursor + "=" * (-len(cursor) % 4)), 8)
