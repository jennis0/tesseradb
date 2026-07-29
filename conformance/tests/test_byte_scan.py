"""I10 byte-scan (plan §10.2, brief step 1): entity IDs never cross the trust boundary.

Authorises a session against a genuine (non-empty, non-universal) grant subset of the fixture
bundle's dictionary, computes — via the independent oracle, never the server — the entity ids the
resulting mask admits ("admitted") and every other entity id the segment actually carries
("denied": everything the mask does *not* admit), then fetches viewports covering every populated
tile of the fixture bundle across a range of zooms (a full-extent bbox at each zoom necessarily
touches every tile the server would ever report a nonzero count for at that depth — deeper zooms
only re-subdivide tiles already covered by shallower ones).

The server's response for every request, `/v1/items/{handle}` for a sample of handles, AND the
full RUST_LOG=info log the server process wrote across the whole run (server spawned with its
stdout/stderr redirected to a file, so nothing is lost to an unread pipe) must contain no encoding
of any admitted or denied entity id, within the scan scope described below. Three distinct scan
strategies are used because a leak can present in three different shapes on the wire; each is
justified separately (fix for the code review's Critical-1/Important-3/Important-4 findings on an
earlier draft of this file, which only ran an 8-byte-window binary scan and so was blind to a
`Handle::new(entity_id as u32)`-shaped bug, to `/v1/items`'s JSON body, and effectively to any log
leak at all — see each subsection below).

**Why the binary scan covers BOTH 4-byte and 8-byte little-endian windows, not just 8.** Entity
ids are `u64` on disk (R1) but always < 2^32 in `bundle_format = 1`, and the wire's `handle` column
is `UInt32` (R5). A server that mints `Handle::new(entity_id.raw() as u32)` — i.e. hands raw entity
ids out as handles instead of independent per-session counters — is exactly the "keeps the Morton/
Roaring machinery, quietly drops I10" case this suite exists to catch, and it leaks the id as a
**4-byte** value, not an 8-byte one. An 8-byte sliding window over a `u32` buffer only happens to
equal a target id when the *next* `u32` in the buffer is exactly zero (`handle[i] + handle[i+1] <<
32 == handle[i]` iff `handle[i+1] == 0`) — a coincidence, not a property of the bug. This was a real
gap in the first draft of this test (code review Critical-1), closed by adding an explicit 4-byte
scan.

**Why the 4-byte scan is restricted to the `handle` column only, and does not also cover `x`/`y`.**
This is a deliberate, and non-obvious, scope decision — including floats in a 4-byte integer scan
was tried and produces false positives, not stronger coverage. A `float32`'s raw 4-byte bit pattern
reinterpreted as an integer is essentially adversarial noise relative to a small (`< 250_000`)
target id: over a fixture with ~250k points scanned across several zooms, the *expected number* of
coincidental 32-bit collisions between a float32 coordinate's bit pattern and some member of a
~150k-element target-id set is on the order of tens (`~250k points x ~5 zooms x 150k targets /
2^32 ≈ 44`) — enough to make the assertion fail on real, harmless fixtures every run, for reasons
that have nothing to do with I10. There also isn't a plausible bug shape that would encode an
entity id *as a coordinate's bit pattern* — `x`/`y` are always genuine floats, never a
reinterpreted integer, so scanning them at 4-byte granularity buys no real coverage. The `handle`
column has no such problem: it is a genuine `u32` integer buffer end to end, so an exact 4-byte
window match there is real signal, not noise. (The existing 8-byte scan over all three columns is
unaffected by this — an 8-byte window spans two `u32`/`float32` lanes, which collapses the false
positive rate back down to negligible, per the same-family reasoning as the original draft.)

**Residual gap, stated honestly.** Restricting `x`/`y` to the 8-byte scan and `handle` to both
widths does not achieve fully symmetric coverage: a bug that encoded an entity id by splitting it
across two adjacent `x`/`y` float lanes at a *4-byte* granularity would not be caught, because that
scan is deliberately not run (see above — it would be indistinguishable from noise). This is judged
an acceptable residual gap because `x`/`y` are declared, typed `float32` columns end-to-end in the
wire contract (R5); a real implementation bug bad enough to smuggle an id through a coordinate
value would almost certainly also produce grossly wrong/NaN-looking coordinates, which the
existing differential-oracle point-comparison tests (`reference/tests`) already scrutinise
independently. The `handle` column remains the one place I10 is actually at risk, and it gets both
scan widths.

**Why the scan targets the points batch's decoded column *value buffers*, not the raw framed
bytes wholesale.** Arrow's IPC framing (FlatBuffers schema/record-batch messages: buffer
offset/length tables, alignment padding, continuation markers) is full of small, unremarkable
integers — buffer *lengths* especially, which land in exactly the same numeric neighbourhood as a
250k-entity fixture's entity-id space. A generic sliding-byte-window scan over the whole framed
payload matches those constantly (this was verified empirically while writing this test: several
buffer-length-shaped integers reliably "matched" real entity ids by pure coincidence, with no
entity id ever actually transmitted). That is noise, not evidence, so the scan is restricted to
the `handle`/`x`/`y` columns' actual decoded value buffers — the only place the wire format could
ever legitimately carry an entity id.

**Why entity ids are filtered to a "safe" high range before the scan, and this is not a
weakening of the test.** The fixture's `entity_id` space is dense and small (`0..249_999` here,
signature-sorted per I9): built by `tessera-build` assigning `EntityId::new(position)` over the
full sorted item list. `tessera-wire::HandleTable` — read its module doc — mints per-session
`Handle`s *sequentially from 0* on first sight ("Sequential-mint leak rationale", an accepted,
documented design choice, not a bug), and the tile stream's `visible`/`matched` columns are
I2-legitimate aggregate counts, also small non-negative integers. Given the dense entity-id space,
a small integer emitted anywhere in a response (a handle value, a tile count) is *structurally
indistinguishable*, by value alone, from a low entity id — scanning against the full admitted ∪
denied set (which is effectively the *entire* entity-id universe, since every entity is one or the
other) would therefore flag constant, meaningless "leaks" purely from handle/count coincidence,
not from anything actually related to `entity_id`. This is not what I10 governs. The test instead
bounds this run's own point/tile fetching (small `k`, shallow zoom range) so that no handle or
count this run can ever produce reaches `SAFE_ID_FLOOR`, and restricts the entity-id scan to ids
at or above that floor — comfortably clear of anything a handle or count could coincidentally
produce, while still covering a substantial share of both the admitted and denied sets for this
fixture (verified below by asserting both filtered sets are non-empty). Adding the 4-byte scan
makes this floor argument *more* load-bearing, not less (code review's explicit callout): a 4-byte
window over the `handle` column matches a target id directly (no neighbouring-zero coincidence
needed), so an *unfiltered* scan would flag every single handle whose numeric value happens to
equal a low entity id — which, at this fixture's density, is nearly all of them. The floor is what
keeps that a real signal instead of guaranteed noise; see "Residual gap" above for what this
still does not cover.

**Why `/v1/items/{handle}` gets a separate, textual (decimal) scan, not the binary one.** It
returns JSON (`ItemResp { scalars: [...] }`) — a leaked id there would appear as an ASCII decimal
string (`"142857"`), not as 8 raw LE bytes, so the binary scan is structurally blind to it (code
review Important-3). A sample of handles actually returned by the viewport fetches above is
queried via `/v1/items/{handle}`, and the raw response bytes are decimal-string-scanned (see
`_decimal_windows` below) exactly like the log.

**Why the log scan needs its own decimal-string pass, not just the binary one.** `tracing`
(the server's logging framework) emits human-readable text; a leaked id there would appear as an
ASCII decimal substring in a formatted log line, never as a raw little-endian integer (code review
Important-4 — the original binary-only log scan was checking for a shape a text logger essentially
never produces, so it was close to vacuous against the realistic failure mode). Both scans are
kept: the binary scan is retained in case anything ever writes raw bytes to the log (e.g. a debug
`{:?}` dump of a wire buffer), and the decimal scan is the one that actually matches how a text
logger would leak an id.

**Known limitation, stated in the file:** absence of a matching byte or decimal-string pattern is
necessary but not sufficient evidence for I10. This is a black-box scan; it cannot see whether some
future change reintroduces `entity_id` under a width, encoding, or obfuscation that defeats all
three of these scans, and it cannot prove no code path ever *could* leak — only that this
particular run's outputs, scanned these particular ways, didn't. The handle-table code review
(`tessera-wire/src/handles.rs`: handles are an independent per-session counter, never a transform
of `entity_id`) is the other, structural half of I10's assurance; this test does not replace it.
"""

from __future__ import annotations

import io
import re
from pathlib import Path

import pyarrow.ipc as ipc
import pytest

from oracle import mask as mask_mod
from oracle.bundle import Bundle
from oracle.harness import spawn_server, stop_server
from oracle.wire import split_frames

SLICE = "s0"
GRID_MAX = 65536.0
ZOOM_RANGE = range(0, 5)  # shallow — see module doc for why this bounds handle/count values
K = 20
# Comfortably above anything ZOOM_RANGE x K could ever produce as a handle or tile count for this
# fixture (worst case sum_{d=0}^{4} 4^d * K = 341 * 20 = 6,820 distinct handles) — see module doc.
SAFE_ID_FLOOR = 100_000
ITEM_SAMPLE_SIZE = 25  # handles sampled for the /v1/items/{handle} textual scan


def _le_windows(data: bytes, width: int, *, stride: int = 1) -> set[int]:
    """Every `width`-byte little-endian window's integer value at every offset that is a multiple
    of `stride`, as a set (dedup — we only care about membership, not position or count).

    `stride` matters a great deal for this fixture. `stride=1` (the default, used for arbitrary
    unstructured bytes such as a text log) is a genuine sliding window: correct when nothing is
    known about alignment. But for a *column buffer* known to be a native fixed-width array
    (`handle: uint32`), `stride=1` is actively wrong: this fixture's target-id set is
    `>= SAFE_ID_FLOOR`, i.e. essentially the *entire* upper half of the dense `0..249_999`
    entity-id space, so almost any 4-byte value in that range is "in target_ids" by pure numeric
    density — and a byte-straddling window spanning parts of two *adjacent* array elements
    (offsets 1, 2, 3 bytes into a `u32` array) routinely produces exactly such a value from two
    small, harmless handles, with no entity id ever involved. This was caught empirically while
    fixing this test: an aligned-only scan is required for column buffers (`stride=width`), so
    only offsets that could ever really be a stored value are considered — never a byte-straddled
    artefact of scanning at a granularity finer than the array's actual element size."""
    n = len(data)
    if n < width:
        return set()
    return {int.from_bytes(data[i : i + width], "little") for i in range(0, n - width + 1, stride)}


def _decimal_windows(data: bytes, floor: int) -> set[int]:
    """Every maximal run of ASCII decimal digits in `data`, parsed as an integer, restricted to
    values `>= floor` (mirrors the binary scan's SAFE_ID_FLOOR reasoning: unfiltered, a text scan
    would flag timestamps, ports, byte counts, and any other small integer a log line legitimately
    contains). `\\b`-anchored so a target id is never matched as a sub-string of a longer number
    (e.g. id `142857` must not "match" inside logged value `1142857123`)."""
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
    """Returns `(eight_byte_windows, handle_four_byte_windows)` over the points batch's *decoded
    column value buffers only* (`handle`/`x`/`y`) — deliberately not the raw framed Arrow IPC bytes
    wholesale, and deliberately not a 4-byte scan over `x`/`y` (see module doc for both).

    Arrow's IPC framing (the FlatBuffers schema/record-batch messages: buffer offset/length
    tables, padding to 8-byte alignment, continuation markers) is full of small, unremarkable
    integers — buffer *lengths*, in particular, are typically round numbers in exactly the same
    numeric neighbourhood as a 250k-entity fixture's entity-id space. A generic sliding-byte-window
    scan over the *whole* framed payload matches those constantly (verified empirically while
    writing this test: a handful of buffer-length-shaped integers reliably collide with real
    entity ids purely by coincidence) — meaningless noise, not evidence of anything crossing the
    trust boundary. Restricting the scan to the actual column value buffers is what the wire
    format ever *could* legitimately carry an entity id in, and matches this module's stated scope
    (the tile batch is separately, deliberately excluded — see the module doc)."""
    # Every column here (`handle: uint32`, `x`/`y`: float32) has a native element width of 4
    # bytes, so all scans are stride=4-aligned (see `_le_windows`'s doc) — never a raw byte-by-byte
    # sliding window, which would manufacture byte-straddled values out of two adjacent, harmless
    # array elements and false-positive constantly against this fixture's dense target-id range.
    NATIVE_STRIDE = 4
    eight_byte: set[int] = set()
    handle_four_byte: set[int] = set()
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        for batch in reader:
            for name in ("handle", "x", "y"):
                col = batch.column(name)
                for buf in col.buffers():
                    if buf is None:
                        continue
                    raw = buf.to_pybytes()
                    eight_byte |= _le_windows(raw, 8, stride=NATIVE_STRIDE)
                    if name == "handle":
                        handle_four_byte |= _le_windows(raw, 4, stride=NATIVE_STRIDE)
    return eight_byte, handle_four_byte


def _decode_handles(points_bytes: bytes) -> list[int]:
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        handles: list[int] = []
        for batch in reader:
            handles.extend(batch.column("handle").to_pylist())
        return handles


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


def test_no_entity_id_crosses_the_wire_or_appears_in_logs(byte_scan_server, bundle_root: Path):
    server, log_path = byte_scan_server
    oracle_bundle = Bundle(bundle_root)

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

    admitted_high = {e for e in admitted if e >= SAFE_ID_FLOOR}
    denied_high = {e for e in denied if e >= SAFE_ID_FLOOR}
    assert admitted_high, "fixture/grant choice must admit >= SAFE_ID_FLOOR entities for this test to mean anything"
    assert denied_high, "fixture/grant choice must deny >= SAFE_ID_FLOOR entities for this test to mean anything"

    target_ids = admitted_high | denied_high

    eight_byte_windows: set[int] = set()
    handle_four_byte_windows: set[int] = set()
    sampled_handles: set[int] = set()
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    requests_made = 0
    for zoom in ZOOM_RANGE:
        raw = server.viewport(token, SLICE, zoom, bbox, k=K)
        _tile_bytes, points_bytes = split_frames(raw)
        # Scope decision (module doc): the tile batch (visible/matched counts) is deliberately
        # excluded from the scan — those are I2-legitimate aggregates sharing the same small
        # numeric range as entity ids, not a surface I10 governs. Only the points batch's decoded
        # column *value buffers* are scanned (not the raw framed IPC bytes wholesale), at both
        # 8-byte width (all three columns) and 4-byte width (`handle` only — see module doc for
        # why `x`/`y` are excluded from the 4-byte pass).
        eight, handle_four = _points_value_buffer_windows(points_bytes)
        eight_byte_windows |= eight
        handle_four_byte_windows |= handle_four
        sampled_handles.update(_decode_handles(points_bytes))
        requests_made += 1
    assert requests_made == len(ZOOM_RANGE)

    leaked_8 = eight_byte_windows & target_ids
    assert not leaked_8, (
        f"found {len(leaked_8)} entity id(s) encoded as an 8-byte LE integer in a viewport points "
        f"batch: {sorted(leaked_8)[:20]}"
    )

    leaked_4 = handle_four_byte_windows & target_ids
    assert not leaked_4, (
        f"found {len(leaked_4)} entity id(s) encoded as a raw 4-byte LE `handle` value in a "
        f"viewport points batch (i.e. a handle equal to a real entity id — the "
        f"`Handle::new(entity_id as u32)` bug shape): {sorted(leaked_4)[:20]}"
    )

    # --- /v1/items/{handle}: JSON body, so the leak shape is an ASCII decimal string, not bytes ---
    # (code review Important-3: the binary scans above cannot see this at all).
    sample = sorted(sampled_handles)[:ITEM_SAMPLE_SIZE]
    assert sample, "must have sampled at least one handle to exercise /v1/items"
    item_decimal_hits: set[int] = set()
    for h in sample:
        resp = server.item(token, h)
        # A handle may legitimately be denied-by-race or already retired; any 2xx/4xx body is
        # still text worth scanning either way, so no status-code assertion is made here — this
        # test is about what bytes appear, not about item-lookup semantics.
        item_decimal_hits |= _decimal_windows(resp.content, SAFE_ID_FLOOR)
    leaked_items = item_decimal_hits & target_ids
    assert not leaked_items, (
        f"found {len(leaked_items)} entity id(s) as an ASCII decimal string in a /v1/items/"
        f"{{handle}} response body: {sorted(leaked_items)[:20]}"
    )

    # --- server log: text, so scan for decimal substrings, not raw LE bytes -----------------------
    # (code review Important-4: a tracing-formatted log leaks ids as decimal text, not as a raw
    # little-endian integer, so the decimal scan is the one that actually matches the realistic
    # failure mode; the binary scan is kept too in case anything ever dumps raw wire bytes to the
    # log, but it is not expected to be where a real leak would show up).
    log_bytes = log_path.read_bytes()
    log_binary_windows = _le_windows(log_bytes, 8) | _le_windows(log_bytes, 4)
    leaked_in_logs_binary = log_binary_windows & target_ids
    assert not leaked_in_logs_binary, (
        f"found {len(leaked_in_logs_binary)} entity id(s) encoded as a raw LE integer in the "
        f"server's log output: {sorted(leaked_in_logs_binary)[:20]}"
    )

    log_decimal_windows = _decimal_windows(log_bytes, SAFE_ID_FLOOR)
    leaked_in_logs_decimal = log_decimal_windows & target_ids
    assert not leaked_in_logs_decimal, (
        f"found {len(leaked_in_logs_decimal)} entity id(s) as an ASCII decimal string in the "
        f"server's log output: {sorted(leaked_in_logs_decimal)[:20]}"
    )
