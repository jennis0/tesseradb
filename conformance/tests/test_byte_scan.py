"""I10 byte-scan (plan §10.2, brief step 1): entity IDs never cross the trust boundary.

Authorises a session against a genuine (non-empty, non-universal) grant subset of the fixture
bundle's dictionary, computes — via the independent oracle, never the server — the entity ids the
resulting mask admits ("admitted") and every other entity id the segment actually carries
("denied": everything the mask does *not* admit), then fetches viewports covering every populated
tile of the fixture bundle across a range of zooms (a full-extent bbox at each zoom necessarily
touches every tile the server would ever report a nonzero count for at that depth — deeper zooms
only re-subdivide tiles already covered by shallower ones).

The server's response for every request, `/v1/items/{tessera_id}` for a sample of tessera ids, AND
the full RUST_LOG=info log the server process wrote across the whole run (server spawned with its
stdout/stderr redirected to a file, so nothing is lost to an unread pipe) must contain no encoding
of any admitted or denied entity id, no encoding of the deployment identity key, and no caller
external id anywhere except the one designed exception — within the scan scope described below.

**Contracts r6 rewrote the wire's identity column** (`docs/evidence/memos/2026-07-30-tessera-id-
construction.md`; contracts §2.6, §3.2): `columns.arrow`/the points batch no longer carry
`entity_id` at all — the gather cannot produce one — and the per-session `handle: u32` this test
previously scanned for is retired from the viewer plane (design r21, Appendix C's new C17). The
wire now carries `tessera_id: u64`, a keyed Feistel permutation of `(shard_id, entity_id)` that is
stable across sessions by design (C17) and inverted only inside the trust boundary. This rewrite
(Task 13) re-derives the scan against that column instead of `handle`, and adds two sweeps the
old design had no column for: the deployment identity key (never leaves the server) and caller
external ids (admin-plane identifiers, SA D14, legitimate in exactly one viewer-plane place: the
`/v1/items` drill-down response, D4).

**Why the entity-id scan of the `tessera_id` column is 8 bytes wide, aligned to the column's own
8-byte element stride — not a 4-byte pass, and not a sliding window.** The bug shape this must
catch is a server that mints `tessera_id = entity_id as u64` (zero-extending a raw, un-permuted
entity id into the identity column) instead of applying the keyed permutation — exactly the
"keeps the Morton/Roaring machinery, quietly drops I10" case this suite exists to catch. Under
`bundle_format = 1` every entity id is `< 2^32` (R1), so that bug's output has the entity id in
the low 4 bytes of the 8-byte lane and zero in the high 4 bytes — and read as a single little-
endian **8-byte** integer, that value *is* the entity id numerically (a zero high half contributes
nothing to the 64-bit value). A per-element-aligned 8-byte scan (`stride = 8`, one window per
stored value, never a byte-straddling slide) therefore catches this bug shape directly, with no
narrower pass required.

**Why no narrower (4-byte) scan is run against the `tessera_id` column, and why that is not a
weaker test than the old `handle` design.** The old `handle` column was a plain sequential `u32`
counter — a 4-byte-aligned scan against it was exact signal, not noise (see the retained
reasoning below for `x`/`y`). `tessera_id` is different in kind: it is a keyed permutation output,
essentially uniform over `2^64`, and a 4-byte-aligned scan would inspect each 32-bit half of that
uniform value *on its own* — a quantity with no special relationship to the entity-id space at
all. Against this fixture's ~250,000-entity target-id set, the *expected number of coincidental
32-bit matches* from scanning every row's low and high 32-bit half across this test's zoom/`k`
budget (order 10,000-15,000 distinct halves observed) is `~13,000 * 150,000 / 2^32 ≈ tens of
hits per run` — the same order of magnitude the original module doc computed for the "don't 4-byte
-scan `x`/`y`" case, and for the same underlying reason: a pseudorandom 32-bit quantity compared
against a large, dense target-id set produces chance matches at a rate the 8-byte-aligned,
per-element scan does not, because the *whole* 64-bit value is astronomically unlikely to
coincide with a value `< 250,000` (see `SAFE_ID_FLOOR`'s section below for the arithmetic).
**Rule, stated explicitly (brief step 1.2): the `tessera_id` column's own buffer never gets a
narrower-than-8-byte scan; every other buffer, every metadata field and every log line is swept
at whatever width is safe for its own shape.** Retiring `handle`'s 4-byte pass therefore does not
reduce coverage of the one bug shape that mattered (raw id zero-extended into the id column) — the
8-byte aligned scan already owns it — and avoids reintroducing exactly the chance-collision flake
this suite's own history (the code review this file's original docstring cites) fought hard to
eliminate for `x`/`y`.

**Residual gap, stated honestly (mirrors the original file's "Residual gap" section for `x`/`y`,
now updated for the same underlying reason).** A bug that put a raw entity id in *only* the high
4 bytes of the `tessera_id` lane (leaving the low 4 bytes non-zero, e.g. from unrelated data) would
not be caught by the 8-byte-aligned scan — that shape needs a 4-byte-granularity pass, which is
exactly the pass excluded above for chance-collision reasons. This is judged acceptable for the
same reason the original `x`/`y` gap was: `tessera_id` is a declared `u64` end-to-end (R5); a real
implementation bug bad enough to smuggle a raw entity id into only half of it, non-zero-extended,
is a stranger and less likely failure mode than the direct zero-extension case this scan does
catch, and `test_identity.py`'s known-answer vectors and `Bundle.verify_identity_cross_check`
(Task 12) independently scrutinise the identity construction itself — the latter **is actually
run** against the fixture bundle by
`reference/tests/test_identity.py::test_fixture_bundle_identity_column_agrees_with_the_key`, which
is what makes this citation load-bearing rather than a reference to an uncalled method (it was the
latter until the seam review caught it; do not remove that test without revisiting this paragraph). The `tessera_id` column
remains the one place I10 is actually at risk on the wire, and it gets the width that matters most.

**Why `x`/`y` keep exactly the pre-r6 scan shape (8-byte scan across all three columns, no 4-byte
pass over the floats).** Unaffected by this revision — repeated here rather than assumed: a
`float32`'s raw 4-byte bit pattern reinterpreted as an integer is adversarial noise relative to a
small, dense target-id set (the same "order of tens of chance collisions" arithmetic as above), and
there is no plausible bug shape that encodes an entity id *as a coordinate's bit pattern` — `x`/`y`
are always genuine floats, never a reinterpreted integer end to end.

**Why the scan targets the points batch's decoded column *value buffers*, not the raw framed bytes
wholesale.** Unaffected by this revision: Arrow's IPC framing (buffer offset/length tables,
alignment padding, continuation markers) is full of small, unremarkable integers — buffer
*lengths* especially — which land in exactly the same numeric neighbourhood as this fixture's
entity-id space. A generic sliding-byte-window scan over the whole framed payload matches those
constantly (verified empirically while writing the original version of this test); the scan is
restricted to the `tessera_id`/`x`/`y` columns' actual decoded value buffers, the only place the
wire format could ever legitimately carry an entity id, an identity key, or an external id.

**`SAFE_ID_FLOOR`, re-derived rather than inherited (brief step 1.2).** The constant survives, but
its job changes completely, because its old job no longer exists.

- *Old job (retired):* bound handle values and tile counts — both small, dense, sequentially
  produced integers — away from the entity-id space so they could not coincidentally look like a
  leaked id. Handles are gone from the wire; this job has nothing left to do.
- *New job:* protect the **decimal-text** scans (the server log, and `/v1/items` JSON bodies)
  against **legitimate small integers this harness actually emits** — ports (ephemeral range, at
  most `65535`), `k` (`<= 500`), zoom (`<= 6`), the identity epoch (`1`), the shard id (small), and
  HTTP status codes (`< 600`). None of these exceeds `65535`; `SAFE_ID_FLOOR = 100_000` clears all
  of them with headroom and is kept at its old numeric value because nothing about the new design
  makes a smaller floor either necessary or safer. (Process ids are the one source of legitimate
  small integers not bounded by this reasoning — Linux's default `pid_max` can exceed `100_000` on
  some configurations. This was true of the pre-r6 design too and is not re-litigated here; stated
  as an honest residual, matching this file's practice of naming what it does not cover.)
- *What no longer needs a floor at all:* the **binary, per-element-aligned** entity-id scan of the
  `tessera_id` column, and *only* that column. Entity ids are `< 250,000`; `tessera_id` values are
  uniform over `2^64`. The chance any single stored `tessera_id` value coincides with *any* member
  of a ~250,000-entity target set is `250,000 / 2^64 ≈ 1.4e-14` — summed over every row this
  fixture could ever produce (250,000 of them), the expected number of coincidental full-width
  matches across the *entire* fixture is `250,000 * 250,000 / 2^64 ≈ 3.4e-9`, i.e. it will not
  happen. The `tessera_id`-column scan below therefore runs against the **full, unfiltered**
  admitted/denied sets — no floor applied — which is *strictly stronger* coverage than the pre-r6
  test had for its equivalent column (that test could only ever check ids `>= SAFE_ID_FLOOR`).
- *What still needs a floor, and why, precisely — including a genuine surprise found while
  building this test:* `x`/`y` are **not** exempt the way `tessera_id` is, and this is not merely
  inherited caution — it was caught empirically while writing this revision. A perfectly ordinary
  `x == 0.0` (or `y == 0.0`) coordinate's 4 raw bytes are all zero, and the 8-byte window spanning
  it and its neighbour is therefore `0` whenever that neighbour is also small/zero-ish — which
  numerically equals entity id **0**, a real, always-present member of a dense `0..N` id space.
  Unfiltered, this fires on essentially every run (it did, immediately, the first time this test
  was run against the real fixture). `x`/`y`'s windows are therefore still checked against the
  **floor-filtered** target set, exactly as the pre-r6 design did, for a related but distinct
  reason: not because a float bit pattern is "adversarial noise" against a dense id space in
  general (that was the original, still-valid reasoning against a 4-byte pass), but because the
  *specific* value `0.0` is common, legitimate, and numerically indistinguishable from entity id 0
  once reduced to raw bytes. The log's **binary** scan keeps the same floor for an independent
  reason: it retains a stride-1 (unstructured-bytes) 4-byte pass — the log is text, not a typed
  column, so no alignment can be assumed, and a stride-1 4-byte scan over `L` bytes of log against
  a target set of size `T` produces an expected `L * T / 2^32` chance hits. `SAFE_ID_FLOOR` bounds
  `T` down to only the high half of the fixture's dense id space, keeping this in the same
  low-single-digits territory the pre-r6 design already accepted for the same scan (this risk is
  orthogonal to the handle-vs-`tessera_id` question — it was already here, unchanged by this
  revision).

**The identity-key sweep (brief step 1, new in this revision).** The deployment's 128-bit
identity key (`identity.key` in MANIFEST; `IdentityKey(k0, k1)` in the oracle) inverts every
`tessera_id` ever issued and must never leave the server (contracts §2.6, memo §3.2's "the key
must appear in no log line" — the oracle's own `identity.py` enforces the same rule on its error
messages). Swept for as: its two 64-bit halves (`k0`, `k1`) as exact 8-byte-aligned matches in the
points-batch value buffers and as exact matches in the log's binary windows; its decimal-string
forms and its 32-lowercase-hex-character form as exact substrings of the log text. No floor is
needed here — these are two fixed, specific values, not a dense target set, so the chance a
`tessera_id` (or anything else on the wire) coincidentally equals `k0`/`k1` is the same
`1 / 2^64`-scale argument as above.

**The external-id sweep (brief step 1, new in this revision).** Caller external ids are admin-
plane identifiers (SA D14) that legitimately appear on the viewer plane in exactly one place: the
`/v1/items/{tessera_id}` drill-down response's `external_id` field (contracts §3.2, D4 — "the only
place a caller external id appears on the viewer plane", per that field's own doc in
`tessera-server/src/viewer.rs`). This test resolves a sample of admitted entities' external ids via
the independent oracle (`Bundle.external_id_of`), confirms each one's *own* drill-down response
does carry it (the positive control that D4's one designed exception actually works, and that the
oracle's own encoding assumption is right), and then confirms that exact byte string appears
nowhere else this run touches: no viewport payload, no log line, and not as an accidental
substring of any other requested item's response body.

**Why `/v1/items/{tessera_id}` gets a separate, textual (decimal / base64) scan, not the binary
one.** Unaffected by this revision: it returns JSON — a leaked id there would appear as an ASCII
decimal string, not as 8 raw LE bytes, so the binary scan is structurally blind to it. A sample of
`tessera_id`s actually returned by the viewport fetches above is queried via `/v1/items/
{tessera_id}`, and the raw response bytes are decimal-string- and byte-substring-scanned exactly
like the log.

**Why the log scan needs its own decimal-string pass, not just the binary one.** Unaffected by
this revision: `tracing` emits human-readable text; a leaked id there would appear as an ASCII
decimal substring, never as a raw little-endian integer in the general case. Both scans are kept:
the binary scan in case anything ever writes raw bytes to the log, and the decimal scan as the one
that actually matches how a text logger would leak an id.

**The explicit negative control (brief step 1's closing instruction).** A scan that never finds
anything proves nothing if it would never have found anything *anyway* — this test asserts that a
genuine, known `tessera_id` (one actually decoded from a real viewport response) **is** present in
the independently-computed byte-window scan of that same response, so the scan's own mechanics are
demonstrated to work on real data before its absence-of-a-different-value is trusted as evidence.

**C17 (design Appendix C) — `tessera_id` stability across sessions, asserted positively (brief's
closing instruction: replace §4.3's retired handle-decorrelation check with what is actually worth
asserting).** The conformance design's §4.3 previously required asserting that handle values are
*uncorrelated* across sessions. Under `tessera_id`, that would assert something the design now
says is deliberately false: C17 records a stable wire identity across sessions and principals as
the **intended trade** of the r21 boundary-identity change, not a residual to guard against — it is
what lets a client bookmark, share or reconcile a point across sessions. This test therefore
authorises a **second**, independent session with the same grant set and asserts a sampled
admitted entity's `tessera_id` resolves, via drill-down, to the same item under both sessions —
the accepted behaviour, checked directly, rather than a retired prohibition kept on life support.

**Known limitation, stated in the file:** absence of a matching byte or decimal-string pattern is
necessary but not sufficient evidence for I10. This is a black-box scan; it cannot see whether some
future change reintroduces an entity id, the identity key or an external id under a width, encoding
or obfuscation that defeats every scan here, and it cannot prove no code path ever *could* leak —
only that this particular run's outputs, scanned these particular ways, didn't. The gather/wire
code review (`tessera-wire`'s module docs: `columns.arrow` carries no entity-id column at all post-
r6, so the gather cannot produce one) is the other, structural half of I10's assurance; this test
does not replace it.

**Not swept: `priority`.** Contracts r6 defines it as `high16(tessera_id)` — a **keyed** prefix of
a value this payload already carries in full — so it narrows nothing and there is nothing to
protect. An earlier draft of this suite (and of the conformance design's §4.3) swept for it and
asserted per-session decorrelation, when `priority` was an unkeyed `splitmix64` of the raw entity
id and the wire identity was a per-session handle; both premises were retired by the 2026-07-30
fold (design Appendix C, C17) and by owner decision (design r21). An UNKEYED per-mark derivative of
the entity id would still be forbidden and would need its own sweep; `priority` is not that.
"""

from __future__ import annotations

import base64
import io
import re
from pathlib import Path

import pyarrow.ipc as ipc
import pytest

from oracle import mask as mask_mod
from oracle.bundle import Bundle
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport_with_subcells, split_frames

SLICE = "s0"
GRID_MAX = 65536.0
ZOOM_RANGE = range(0, 5)  # shallow — see module doc for why this bounds the decimal-scan floor
K = 20
# §3.3 underlay depth offset for the scan. Small on purpose: 4^2 = 16 sub-cells per tile is enough
# to produce a populated third stream at every zoom in ZOOM_RANGE without tripping the server's
# max_underlay_cells budget, and the sweep cares that the bytes EXIST and are clean, not that there
# are many of them.
UNDERLAY_OFFSET = 2
# See module doc's "SAFE_ID_FLOOR, re-derived rather than inherited" section: this now protects
# only the decimal-text scans (log, /v1/items) against legitimate small integers this harness
# emits (ports <= 65535, k <= 500, zoom <= 6, epoch, shard id, HTTP status). The binary,
# per-element-aligned entity-id scan of `tessera_id` needs no floor at all (see the same section).
SAFE_ID_FLOOR = 100_000
ITEM_SAMPLE_SIZE = 25  # tessera ids sampled for the /v1/items/{tessera_id} textual scan
EXTERNAL_ID_SAMPLE_SIZE = 5  # admitted entities sampled for the external-id sweep


def _le_windows(data: bytes, width: int, *, stride: int = 1) -> set[int]:
    """Every `width`-byte little-endian window's integer value at every offset that is a multiple
    of `stride`, as a set (dedup — we only care about membership, not position or count).

    `stride` matters a great deal for this fixture. `stride=1` (used for arbitrary unstructured
    bytes such as a text log) is a genuine sliding window: correct when nothing is known about
    alignment. But for a *column buffer* known to be a native fixed-width array, `stride=1` would
    be actively wrong — it would manufacture byte-straddled values out of two adjacent, harmless
    array elements. Every column scan below uses `stride` equal to that column's own native
    element width (8 for `tessera_id`, 4 for `x`/`y`), so only offsets that could ever really be a
    stored value are considered."""
    n = len(data)
    if n < width:
        return set()
    return {int.from_bytes(data[i : i + width], "little") for i in range(0, n - width + 1, stride)}


def _decimal_windows(data: bytes, floor: int) -> set[int]:
    """Every maximal run of ASCII decimal digits in `data`, parsed as an integer, restricted to
    values `>= floor` (module doc: unfiltered, a text scan would flag ports, HTTP statuses, k/zoom
    values and any other small integer a log line legitimately contains). `\\b`-anchored so a
    target id is never matched as a sub-string of a longer number."""
    out: set[int] = set()
    text = data.decode("utf-8", errors="replace")
    for m in re.finditer(r"(?<!\d)\d+(?!\d)", text):
        try:
            v = int(m.group(0))
        except ValueError:
            continue
        if v >= floor:
            out.add(v)
    return out


def _points_value_buffer_windows(points_bytes: bytes) -> tuple[set[int], set[int]]:
    """Returns `(tessera_id_windows, xy_windows)`: every 8-byte-aligned window over the points
    batch's decoded `tessera_id` column value buffer, and separately every 8-byte window over the
    `x`/`y` columns' value buffers — deliberately not the raw framed Arrow IPC bytes wholesale
    (module doc: framing is full of small, unremarkable buffer-length integers), and deliberately
    never a narrower-than-8-byte pass over `tessera_id` (module doc's chance-collision arithmetic)
    nor a 4-byte pass over `x`/`y` (unaffected pre-r6 reasoning, restated in the module doc).

    `tessera_id` is scanned at its own native stride (8 — one window per stored value, aligned,
    never byte-straddled). `x`/`y` are scanned at stride 4 (their own native element width),
    giving an 8-byte window spanning two adjacent float lanes, exactly as the pre-r6 design did —
    unaffected by this revision. The two results are kept **separate**, not merged into one set:
    `tessera_id`'s windows are safe to compare against the full, unfiltered target-id set (module
    doc's `SAFE_ID_FLOOR` section); `x`/`y`'s are not, because a genuine `0.0` coordinate produces
    an all-zero 8-byte window that numerically equals entity id 0 — see the inline comment below.
    """
    tessera_windows: set[int] = set()
    xy_windows: set[int] = set()
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        for batch in reader:
            for name, width in (("tessera_id", 8), ("x", 4), ("y", 4)):
                col = batch.column(name)
                # Arrow buffers are padded to an alignment boundary past the last real element
                # (Arrow's own spec, independent of anything this suite controls) — trimming to
                # exactly `len(col) * width` bytes before scanning is required, not cosmetic: an
                # untrimmed scan picks up the zero-filled padding tail as spurious 8-byte-aligned
                # windows equal to 0, which collides with entity id 0 on every single run. This
                # was caught by this test's own first run while writing it, precisely the kind of
                # self-inflicted false positive the rest of this module's scoping decisions exist
                # to avoid.
                data_bufs = [buf for buf in col.buffers() if buf is not None]
                buf = data_bufs[-1]  # last buffer is always the value buffer (validity, if any, first)
                trimmed = buf.to_pybytes()[: len(col) * width]
                if name == "tessera_id":
                    tessera_windows |= _le_windows(trimmed, 8, stride=8)
                else:
                    # `x`/`y` at native stride 4 (an 8-byte window spans two adjacent float
                    # lanes) — kept separate from `tessera_id`'s windows below, because a
                    # genuine `x == 0.0`/`y == 0.0` (an ordinary, common coordinate value, not a
                    # bug) produces an 8-byte all-zero window that numerically equals entity id
                    # 0. That is exactly the "floor exists to keep small legitimate values out of
                    # the target set" problem the module doc describes for the log/text scans,
                    # just arriving via a different route (a real float bit pattern rather than
                    # log noise) — so `x`/`y` windows are checked against the FLOOR-FILTERED
                    # target set, same as the log/decimal scans, not the full one.
                    xy_windows |= _le_windows(trimmed, 8, stride=4)
    return tessera_windows, xy_windows


def _points_stream_length(points_and_beyond: bytes) -> int:
    """Byte length of the points stream inside `points-and-everything-after`.

    Only the tile boundary carries a length prefix (contracts §5), so this is how a reader finds
    where the appended sub-cell stream begins: parse the points stream to its end-of-stream marker
    and take the cursor.
    """
    buf = io.BytesIO(points_and_beyond)
    with ipc.open_stream(buf) as reader:
        for _ in reader:
            pass
    return buf.tell()


def _subcell_value_buffer_windows(subcell_bytes: bytes) -> set[int]:
    """8-byte-aligned LE windows over the sub-cell batch's `cell` and `count` value buffers.

    The §3.3 underlay's third Arrow stream is **appended** after the points stream with no length
    prefix, so `split_frames` hands it back glued to `points_bytes` and `ipc.open_stream` stops at
    the points stream's end-of-stream marker without ever looking at it. Before this, every
    underlay byte was outside the scan — and the columns are exactly the shape that matters: `cell`
    is a Morton prefix up to 2^32-1 and `count` a small integer, both landing squarely in the dense
    entity-id neighbourhood the module doc's `SAFE_ID_FLOOR` reasoning was built for.

    Both columns are `uint64`, so a window is a single element rather than a straddle; they are
    returned together and compared against the FLOOR-FILTERED target set, for the same reason `x`/`y`
    are — a genuine `count` of 0 or a `cell` prefix of 0 is an all-zero window that numerically
    equals entity id 0.
    """
    windows: set[int] = set()
    if not subcell_bytes:
        return windows
    with ipc.open_stream(io.BytesIO(subcell_bytes)) as reader:
        for batch in reader:
            for name in ("cell", "count"):
                col = batch.column(name)
                buf = col.buffers()[1]
                if buf is None:
                    continue
                # Trim Arrow's alignment padding, as the points sweep does — an untrimmed tail
                # reads as spurious zero windows.
                raw = buf.to_pybytes()[: len(col) * 8]
                for off in range(0, len(raw) - 7, 8):
                    windows.add(int.from_bytes(raw[off : off + 8], "little"))
    return windows


def _decode_tessera_ids(points_bytes: bytes) -> list[int]:
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        ids: list[int] = []
        for batch in reader:
            ids.extend(batch.column("tessera_id").to_pylist())
        return ids


@pytest.fixture(scope="module")
def byte_scan_server(tmp_path_factory, bundle_root):
    """A dedicated server instance for this module, logging to a file (not an unread pipe) so the
    full RUST_LOG=info output can be scanned after the run."""
    tmp_dir = tmp_path_factory.mktemp("byte-scan-server")
    log_path = tmp_dir / "server.log"
    srv, proc = spawn_server(
        bundle_root,
        tmp_dir,
        log_path=log_path,
        env_extra={"RUST_LOG": "info"},
    )
    yield srv, log_path
    stop_server(proc)


def test_no_entity_id_key_or_misplaced_external_id_crosses_the_wire_or_appears_in_logs(
    byte_scan_server, bundle_root: Path
):
    server, log_path = byte_scan_server
    oracle_bundle = Bundle(bundle_root)
    assert oracle_bundle.identity_key is not None, "fixture must be a post-r6 bundle"

    # A genuine subset: enough terms to admit a substantial fraction of the fixture (so there is
    # plenty to find if something leaks), but not every term (so a real "denied" set exists too).
    dictionary = oracle_bundle.dictionary
    granted_descriptors = dictionary[: max(1, len(dictionary) // 2)]
    granted_terms = {oracle_bundle.term_id_of(d) for d in granted_descriptors}
    granted_terms.discard(None)

    auth = server.authorise([d.decode("ascii") for d in granted_descriptors])
    token = auth["token"]

    admitted = mask_mod.mask_of(granted_terms, oracle_bundle.pairs_path())

    seg = oracle_bundle.segment(SLICE)
    all_entities = {int(e) for e in seg.entity_id.tolist()}
    denied = all_entities - admitted

    # Unfiltered target set — used ONLY for the tessera_id column's own scan (module doc: no
    # floor needed there; x/y and the log/item text scans below use the floor-filtered set).
    target_ids = admitted | denied
    assert admitted, "fixture/grant choice must admit >= 1 entity for this test to mean anything"
    assert denied, "fixture/grant choice must deny >= 1 entity for this test to mean anything"

    # Floor-filtered target set — used for x/y's windows, the log (binary + decimal) and the
    # /v1/items decimal scan (module doc).
    admitted_high = {e for e in admitted if e >= SAFE_ID_FLOOR}
    denied_high = {e for e in denied if e >= SAFE_ID_FLOOR}
    assert admitted_high, "fixture/grant choice must admit >= SAFE_ID_FLOOR entities"
    assert denied_high, "fixture/grant choice must deny >= SAFE_ID_FLOOR entities"
    target_ids_high = admitted_high | denied_high

    identity_key = oracle_bundle.identity_key
    identity_key_hex = oracle_bundle.manifest["identity"]["key"]
    identity_key_raw = bytes.fromhex(identity_key_hex)

    tessera_id_windows: set[int] = set()
    xy_windows: set[int] = set()
    sampled_ids: set[int] = set()
    all_raw_responses: list[bytes] = []
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    requests_made = 0
    subcell_windows: set[int] = set()
    subcell_rows_seen = 0
    for zoom in ZOOM_RANGE:
        # Ask for the §3.3 underlay on every request, so the appended third stream is actually
        # produced and swept. Without this the stream never existed during the scan at all.
        raw = server.viewport(token, SLICE, zoom, bbox, k=K, underlay_offset=UNDERLAY_OFFSET)
        all_raw_responses.append(raw)
        _tile_bytes, points_bytes = split_frames(raw)

        # `split_frames` returns points-and-everything-after, so recover the sub-cell stream by
        # parsing the points stream to its end and taking what follows.
        _t2, _p2, sub_cells = decode_viewport_with_subcells(raw)
        subcell_rows_seen += len(sub_cells)
        consumed = _points_stream_length(points_bytes)
        subcell_windows |= _subcell_value_buffer_windows(points_bytes[consumed:])
        # Nothing may sit unscanned between the two: if a fourth stream is ever appended, this
        # fails rather than letting it arrive unswept.
        assert consumed <= len(points_bytes)
        # Scope decision (module doc): the tile batch (visible/matched counts) is deliberately
        # excluded from the scan — those are I2-legitimate aggregates, not a surface I10 governs.
        # Only the points batch's decoded column *value buffers* are scanned.
        tid_w, xy_w = _points_value_buffer_windows(points_bytes)
        tessera_id_windows |= tid_w
        xy_windows |= xy_w
        sampled_ids.update(_decode_tessera_ids(points_bytes))
        requests_made += 1
    assert requests_made == len(ZOOM_RANGE)
    # Used for checks (identity key, external ids) that need to look at the whole points batch,
    # not just the entity-id sweep's column-specific split above.
    all_points_windows = tessera_id_windows | xy_windows

    # The identity key must not appear ANYWHERE, sub-cell stream included — it is a 128-bit random
    # value, so there is no chance-collision hazard in widening the haystack for it.
    #
    # The external-id check below deliberately does **not** widen: the sub-cell batch's `count`
    # column holds small integers by construction, so it genuinely contains 1, 2, 3..., and a
    # low-valued external id (entity 0's is the 8-byte encoding of 1) collides with them by pure
    # arithmetic rather than by leaking. That is the same hazard the external-id check's own comment
    # already records for flatbuffer framing; including sub-cell counts turns it from unlikely into
    # certain. The sub-cell columns are checked against the floor-filtered entity-id set above, which
    # is the check that actually bears on I10 here.
    all_windows_including_underlay = all_points_windows | subcell_windows

    # --- explicit negative control: the scan must find a REAL tessera_id, or it proves nothing ---
    assert sampled_ids, "must have decoded at least one tessera_id to exercise the scan at all"
    known_tessera_id = next(iter(sampled_ids))
    assert known_tessera_id in tessera_id_windows, (
        "sanity check failed: a tessera_id actually decoded from a real response was not found by "
        "the independent byte-window scan of that same response — the scan mechanism itself is "
        "broken, so its absence-of-a-leak result below cannot be trusted"
    )

    # --- I10: no entity id, at 8-byte-aligned width, anywhere in the points batch's buffers -----
    # `tessera_id`'s own windows are checked against the FULL, unfiltered target set (module doc:
    # negligible chance-collision risk for a uniform 64-bit column). `x`/`y`'s windows are checked
    # against the floor-filtered set only, because a genuine `0.0` coordinate produces an all-zero
    # window that numerically equals entity id 0 (module doc, `_points_value_buffer_windows`).
    leaked_tid = tessera_id_windows & target_ids
    assert not leaked_tid, (
        f"found {len(leaked_tid)} entity id(s) encoded as an 8-byte-aligned LE integer in the "
        f"tessera_id column of a viewport points batch: {sorted(leaked_tid)[:20]}"
    )
    leaked_xy = xy_windows & target_ids_high
    assert not leaked_xy, (
        f"found {len(leaked_xy)} entity id(s) encoded as an 8-byte LE integer spanning the x/y "
        f"columns of a viewport points batch: {sorted(leaked_xy)[:20]}"
    )

    # --- I10: the §3.3 underlay's appended sub-cell stream, which was previously unscanned -------
    assert subcell_rows_seen > 0, (
        "no sub-cells were served, so the underlay sweep proves nothing — check UNDERLAY_OFFSET "
        "against the server's max_underlay_offset and max_underlay_cells"
    )
    leaked_sub = subcell_windows & target_ids_high
    assert not leaked_sub, (
        f"found {len(leaked_sub)} entity id(s) encoded as an 8-byte-aligned LE integer in the "
        f"cell/count columns of a viewport sub-cell batch: {sorted(leaked_sub)[:20]}"
    )

    # --- identity key: must never appear in a viewport payload, at any width tried above --------
    assert (
        identity_key.k0 not in all_windows_including_underlay
    ), "identity key half k0 found on the wire"
    assert (
        identity_key.k1 not in all_windows_including_underlay
    ), "identity key half k1 found on the wire"
    for raw in all_raw_responses:
        assert identity_key_raw not in raw, "identity key's raw 16 bytes found in a viewport response"

    # --- external ids: legitimate in exactly one place (drill-down), nowhere else ----------------
    ext_sample = sorted(admitted)[:EXTERNAL_ID_SAMPLE_SIZE]
    assert ext_sample, "must have at least one admitted entity to exercise the external-id sweep"
    for entity_id in ext_sample:
        tessera_id = oracle_bundle.tessera_id_of(entity_id)
        ext_bytes = oracle_bundle.external_id_of(entity_id)
        ext_int = int.from_bytes(ext_bytes, "little")

        # Positive control (D4): the item's OWN drill-down response does carry it.
        resp = server.item(token, tessera_id)
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body.get("external_id") is not None, "admitted item with a known external id must return one"
        assert base64.b64decode(body["external_id"]) == ext_bytes, (
            "drill-down external_id did not round-trip to the oracle's own encoding"
        )

        # Never on the viewer plane's viewport payloads. Checked against the decoded VALUE
        # BUFFERS only (`all_points_windows`, built from trimmed column buffers), not the raw
        # framed response bytes wholesale — an `ext_bytes in raw` substring check across the
        # whole Arrow IPC frame (flatbuffer schema/record-batch metadata included) is exactly the
        # "framing is full of small, unremarkable integers" trap this module's own docstring warns
        # about: this fixture's external ids are 8 raw bytes of a small source-corpus integer
        # (`Bundle.external_id_of`'s doc), so a low-valued one (e.g. entity 0's external id, the
        # 8-byte encoding of the small integer 1) is a length/flag-shaped value very likely to
        # appear somewhere in ordinary flatbuffer framing by pure coincidence — caught empirically
        # while writing this test, the same way the padding-tail zero collision above was.
        assert ext_int not in all_points_windows, (
            f"external id for entity {entity_id} found as an 8-byte-aligned integer in a "
            "viewport points batch"
        )

    # --- C17: tessera_id is stable across sessions — the property that replaces the retired,
    # now-false "handle values are uncorrelated across sessions" check (conformance design §4.3,
    # revised 2026-07-30; see that document's Appendix R). A second, independently-authorised
    # session with the SAME visible grant set must resolve the same admitted entity to the SAME
    # tessera_id — this is the accepted, intended behaviour (design Appendix C, C17), not a
    # regression to guard against, so it is asserted positively rather than as a decorrelation
    # check.
    auth2 = server.authorise([d.decode("ascii") for d in granted_descriptors])
    token2 = auth2["token"]
    assert token2 != token, "two independent authorisations must not share a session token"
    for entity_id in ext_sample:
        tessera_id = oracle_bundle.tessera_id_of(entity_id)
        resp2 = server.item(token2, tessera_id)
        assert resp2.status_code == 200, (
            f"entity {entity_id}'s tessera_id must resolve identically under a second, "
            f"independently-authorised session with the same visibility (C17): {resp2.text}"
        )
        ext_bytes = oracle_bundle.external_id_of(entity_id)
        assert base64.b64decode(resp2.json()["external_id"]) == ext_bytes, (
            "the SAME tessera_id must resolve to the SAME item across sessions (C17), but the "
            "second session's drill-down disagreed with the first's"
        )

    # --- /v1/items/{tessera_id}: JSON body, so the leak shape is an ASCII decimal string ---------
    sample = sorted(sampled_ids)[:ITEM_SAMPLE_SIZE]
    assert sample, "must have sampled at least one tessera_id to exercise /v1/items"
    item_decimal_hits: set[int] = set()
    item_bodies: list[bytes] = []
    for tid in sample:
        resp = server.item(token, tid)
        # A tessera_id may legitimately be denied-by-race or already retired; any 2xx/4xx body is
        # still text worth scanning either way, so no status-code assertion is made here.
        item_decimal_hits |= _decimal_windows(resp.content, SAFE_ID_FLOOR)
        item_bodies.append(resp.content)
    leaked_items = item_decimal_hits & target_ids_high
    assert not leaked_items, (
        f"found {len(leaked_items)} entity id(s) as an ASCII decimal string in a /v1/items/"
        f"{{tessera_id}} response body: {sorted(leaked_items)[:20]}"
    )
    for entity_id in ext_sample:
        ext_bytes = oracle_bundle.external_id_of(entity_id)
        own_tessera_id = oracle_bundle.tessera_id_of(entity_id)
        for tid, body in zip(sample, item_bodies):
            if tid == own_tessera_id:
                continue  # this is the one designed exception (D4) — checked above already
            assert ext_bytes not in body, (
                f"external id for entity {entity_id} found in a /v1/items response for a "
                f"different tessera_id ({tid})"
            )

    # --- server log: text, so scan for decimal substrings, not just raw LE bytes -----------------
    log_bytes = log_path.read_bytes()
    log_text_for_substrings = log_bytes.decode("utf-8", errors="replace")

    log_binary_windows = _le_windows(log_bytes, 8) | _le_windows(log_bytes, 4)
    leaked_in_logs_binary = log_binary_windows & target_ids_high
    assert not leaked_in_logs_binary, (
        f"found {len(leaked_in_logs_binary)} entity id(s) encoded as a raw LE integer in the "
        f"server's log output: {sorted(leaked_in_logs_binary)[:20]}"
    )

    log_decimal_windows = _decimal_windows(log_bytes, SAFE_ID_FLOOR)
    leaked_in_logs_decimal = log_decimal_windows & target_ids_high
    assert not leaked_in_logs_decimal, (
        f"found {len(leaked_in_logs_decimal)} entity id(s) as an ASCII decimal string in the "
        f"server's log output: {sorted(leaked_in_logs_decimal)[:20]}"
    )

    assert identity_key.k0 not in log_binary_windows, "identity key half k0 found in the log"
    assert identity_key.k1 not in log_binary_windows, "identity key half k1 found in the log"
    assert str(identity_key.k0) not in log_text_for_substrings, "identity key half k0 found as decimal text in the log"
    assert str(identity_key.k1) not in log_text_for_substrings, "identity key half k1 found as decimal text in the log"
    assert identity_key_hex not in log_text_for_substrings, "identity key hex form found in the log"
    assert identity_key_raw not in log_bytes, "identity key raw bytes found in the log"

    for entity_id in ext_sample:
        ext_bytes = oracle_bundle.external_id_of(entity_id)
        assert ext_bytes not in log_bytes, f"external id for entity {entity_id} found raw in the log"
