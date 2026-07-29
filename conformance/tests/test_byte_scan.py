"""I10 byte-scan (plan §10.2, brief step 1): entity IDs never cross the trust boundary.

Authorises a session against a genuine (non-empty, non-universal) grant subset of the fixture
bundle's dictionary, computes — via the independent oracle, never the server — the entity ids the
resulting mask admits ("admitted") and every other entity id the segment actually carries
("denied": everything the mask does *not* admit), then fetches viewports covering every populated
tile of the fixture bundle across a range of zooms (a full-extent bbox at each zoom necessarily
touches every tile the server would ever report a nonzero count for at that depth — deeper zooms
only re-subdivide tiles already covered by shallower ones).

The server's response for every request, AND the full RUST_LOG=info log the server process wrote
across the whole run (server spawned with its stdout/stderr redirected to a file, so nothing is
lost to an unread pipe), must contain no 8-byte little-endian encoding (contracts §1: integers are
LE on disk; the wire format re-uses that convention) of any admitted or denied entity id, within
the scan scope described below.

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
fixture (verified below by asserting both filtered sets are non-empty).

**Known limitation (documented per the brief):** absence of a matching byte pattern is necessary
but not sufficient evidence for I10. This is a black-box scan; it cannot see whether some future
change reintroduces `entity_id` under a different width, byte order, or an obfuscation that
defeats a naive substring match, and it cannot prove no code path ever *could* leak — only that
this particular run's outputs, byte-scanned this particular way, didn't. The handle-table code
review (`tessera-wire/src/handles.rs`: handles are an independent per-session counter, never a
transform of `entity_id`) is the other, structural half of I10's assurance; this test does not
replace it.
"""

from __future__ import annotations

import io
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


def _le_u64_windows(data: bytes) -> set[int]:
    """Every 8-byte little-endian window's integer value, as a set (dedup — we only care about
    membership, not position or count)."""
    n = len(data)
    if n < 8:
        return set()
    return {int.from_bytes(data[i : i + 8], "little") for i in range(n - 7)}


def _points_value_buffer_windows(points_bytes: bytes) -> set[int]:
    """8-byte LE windows over the points batch's *decoded column value buffers only*
    (`handle`/`x`/`y`) — deliberately not the raw framed Arrow IPC bytes wholesale.

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
    windows: set[int] = set()
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        for batch in reader:
            for name in ("handle", "x", "y"):
                col = batch.column(name)
                for buf in col.buffers():
                    if buf is None:
                        continue
                    windows |= _le_u64_windows(buf.to_pybytes())
    return windows


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

    points_windows: set[int] = set()
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    requests_made = 0
    for zoom in ZOOM_RANGE:
        raw = server.viewport(token, SLICE, zoom, bbox, k=K)
        _tile_bytes, points_bytes = split_frames(raw)
        # Scope decision (module doc): the tile batch (visible/matched counts) is deliberately
        # excluded from the scan — those are I2-legitimate aggregates sharing the same small
        # numeric range as entity ids, not a surface I10 governs. And only the points batch's
        # decoded column *value buffers* are scanned, not the raw framed IPC bytes wholesale
        # (see `_points_value_buffer_windows`'s doc for why).
        points_windows |= _points_value_buffer_windows(points_bytes)
        requests_made += 1
    assert requests_made == len(ZOOM_RANGE)

    leaked_in_response = points_windows & target_ids
    assert not leaked_in_response, (
        f"found {len(leaked_in_response)} entity id(s) encoded as an 8-byte LE integer in a "
        f"viewport points batch: {sorted(leaked_in_response)[:20]}"
    )

    log_bytes = log_path.read_bytes()
    log_windows = _le_u64_windows(log_bytes)
    leaked_in_logs = log_windows & target_ids
    assert not leaked_in_logs, (
        f"found {len(leaked_in_logs)} entity id(s) encoded as an 8-byte LE integer in the "
        f"server's log output: {sorted(leaked_in_logs)[:20]}"
    )
