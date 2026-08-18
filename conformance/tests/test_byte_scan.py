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

**Which corpus, and what changing it cost.** This scan ran against a 250,000-item prefix of the
Phase 0 corpus, which lives outside the repository and runs to tens of gigabytes — so the suite
could not run from a clean checkout and could not run in CI at all. It now runs against the
synthetic adversarial mask catalogue (`oracle.catalogue`, 150,000 items), which is generated from a
seed.

**What it costs, measured rather than waved past.** Two things, and the second is the one that
matters. A realistic term distribution, which this test does not depend on: the scan is about
**encodings crossing the boundary**, not about how terms are distributed, and the target-id set is
every entity the segment carries either way. But `SAFE_ID_FLOOR` stayed at 100,000 while the corpus
shrank from 250,000 entities to 150,000 — so the *floor-filtered* sweeps, which are every sweep
except the `tessera_id` column's, went from seeing **60% of the entity-id space to 33%**. A leak of
an id below the floor was invisible to them before and still is; what changed is how many ids that
covers. The floor was left at 100,000 deliberately: the only route to more reach is lowering it, and
the residual this module already records — process ids, which `pid_max` can push well above 65,535 —
gets worse as it falls. Coverage was traded for a scan that does not manufacture its own failures.

The arithmetic below is restated at 150,000 rather than assumed to carry over. What the move gains
is a designed entity-ID layout: `SAFE_ID_FLOOR`'s preconditions are satisfied by the catalogue's
`high_tail` block **by construction and checked by `catalogue.verify()`**, where on the Phase 0
prefix they held by accident of which terms happened to be granted.

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
reasoning below for `code`). `tessera_id` is different in kind: it is a keyed permutation output,
essentially uniform over `2^64`, and a 4-byte-aligned scan would inspect each 32-bit half of that
uniform value *on its own* — a quantity with no special relationship to the entity-id space at
all. Against this fixture's 150,000-entity target-id set, the *expected number of coincidental
32-bit matches* from scanning every row's low and high 32-bit half across this test's zoom/`k`
budget (order 10,000-15,000 distinct halves observed) is `~13,000 * 150,000 / 2^32 ≈ 0.5 hits per
run` — order-one, not order-zero, and the figure was quoted as "tens" for as long as this paragraph
has existed, which overstated the noise that argues against scanning more — the same order of magnitude the original module doc computed for the "don't 4-byte
-scan `x`/`y`" case, and for the same underlying reason: a pseudorandom 32-bit quantity compared
against a large, dense target-id set produces chance matches at a rate the 8-byte-aligned,
per-element scan does not, because the *whole* 64-bit value is astronomically unlikely to
coincide with a value `< 150,000` (see `SAFE_ID_FLOOR`'s section below for the arithmetic).
**Rule, stated explicitly (brief step 1.2): the `tessera_id` column's own buffer never gets a
narrower-than-8-byte scan; every other buffer, every metadata field and every log line is swept
at whatever width is safe for its own shape.** Retiring `handle`'s 4-byte pass therefore does not
reduce coverage of the one bug shape that mattered (raw id zero-extended into the id column) — the
8-byte aligned scan already owns it — and avoids reintroducing exactly the chance-collision flake
this suite's own history (the code review this file's original docstring cites) fought hard to
eliminate for the geometry column.

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
run** against the bundle this module scans by
`test_the_catalogue_bundles_identity_column_agrees_with_its_key` below, which is what makes this
citation load-bearing rather than a reference to an uncalled method (it was the latter until the
seam review caught it; and it became a reference to a check performed on a *different* bundle when
this module moved off the Phase 0 corpus, which is why the check now runs here. Do not remove it
without revisiting this paragraph). The `tessera_id` column
remains the one place I10 is actually at risk on the wire, and it gets the width that matters most.

**Why `code` keeps the stride the `x`/`y` pair had, not the width it now has.** The geometry
column is one `uint64` position (contracts §3.2), where it used to be two `float32` lanes. The
sweep still runs at **stride 4** over it, which is a superset of its own aligned windows: the extra
half-offset windows are the only thing that sees an entity id straddling the high half of one code
and the low half of the next, which is the smuggling shape the two float lanes made visible. What
*did* change is that the aligned case now matters too. A `float32` pair made "an entity id written
where geometry belongs" implausible — a coordinate is never a reinterpreted integer end to end — but
a `u64` position column and a `u64` identifier are the same shape, so a build writing a
`tessera_id` into `code` is now an ordinary bug. Floor-filtering catches it for every id at or
above the floor, the same guarantee `tessera_id`'s own column carries; both plants are in
`test_every_scan_mechanism_catches_a_planted_entity_id`.

**Why the scan targets the points batch's decoded column *value buffers*, not the raw framed bytes
wholesale.** Unaffected by this revision: Arrow's IPC framing (buffer offset/length tables,
alignment padding, continuation markers) is full of small, unremarkable integers — buffer
*lengths* especially — which land in exactly the same numeric neighbourhood as this fixture's
entity-id space. A generic sliding-byte-window scan over the whole framed payload matches those
constantly (verified empirically while writing the original version of this test); the scan is
restricted to the `tessera_id`/`code` columns' actual decoded value buffers, the only place the
wire format could ever legitimately carry an entity id, an identity key, or an external id.

**`SAFE_ID_FLOOR`, re-derived rather than inherited (brief step 1.2).** The constant survives, but
its job changes completely, because its old job no longer exists.

- *Old job (retired):* bound handle values and tile counts — both small, dense, sequentially
  produced integers — away from the entity-id space so they could not coincidentally look like a
  leaked id. Handles are gone from the wire; this job has nothing left to do.
- *New job:* protect the **decimal-text** scans (the server log, and `/v1/items` JSON bodies)
  against **legitimate small integers this harness actually emits** — ports (ephemeral range, at
  most `65535`), `k` (`<= 500`), zoom (`<= 6`), the idset (`1`), the shard id (small), and
  HTTP status codes (`< 600`). None of these exceeds `65535`; `SAFE_ID_FLOOR = 100_000` clears all
  of them with headroom and is kept at its old numeric value because nothing about the new design
  makes a smaller floor either necessary or safer. (Process ids are the one source of legitimate
  small integers not bounded by this reasoning — Linux's default `pid_max` can exceed `100_000` on
  some configurations. This was true of the pre-r6 design too and is not re-litigated here; stated
  as an honest residual, matching this file's practice of naming what it does not cover.)
- *What no longer needs a floor at all:* the **binary, per-element-aligned** entity-id scan of the
  `tessera_id` column, and *only* that column. Entity ids are `< 150,000`; `tessera_id` values are
  uniform over `2^64`. The chance any single stored `tessera_id` value coincides with *any* member
  of a 150,000-entity target set is `150,000 / 2^64 ≈ 8.1e-15` — summed over every row this
  fixture could ever produce (150,000 of them), the expected number of coincidental full-width
  matches across the *entire* fixture is `150,000 * 150,000 / 2^64 ≈ 1.2e-9`, i.e. it will not
  happen. The `tessera_id`-column scan below therefore runs against the **full, unfiltered**
  admitted/denied sets — no floor applied — which is *strictly stronger* coverage than the pre-r6
  test had for its equivalent column (that test could only ever check ids `>= SAFE_ID_FLOOR`).
- *What still needs a floor, and why, precisely:* `code` is **not** exempt the way `tessera_id`
  is. Under `x`/`y` this was caught empirically — an ordinary `x == 0.0` has four zero bytes, and
  the 8-byte window spanning it and a small neighbour is `0`, which numerically equals entity id
  **0**, a real and always-present member of a dense `0..N` id space; unfiltered it fired on
  essentially every run. A single `uint64` position does not remove that: a point at the extent
  origin quantises to `0` on both axes and therefore has `code == 0` exactly. The same value, the
  same collision, reached by a shorter route — so `code`'s windows are still checked against the
  **floor-filtered** target set. The reasoning is the one the pre-r6 design settled on: not that a
  bit pattern is "adversarial noise" against a dense id space in general, but that this *specific*
  value is common, legitimate, and numerically indistinguishable from entity id 0. The log's **binary** scan keeps the same floor for an independent
  reason: it retains a stride-1 (unstructured-bytes) 4-byte pass — the log is text, not a typed
  column, so no alignment can be assumed, and a stride-1 4-byte scan over `L` bytes of log against
  a target set of size `T` produces an expected `L * T / 2^32` chance hits. `SAFE_ID_FLOOR` bounds
  `T` down to only the top third of the fixture's dense id space (50,000 of 150,000; it was the
  high half at the old corpus size, and the sentence was restated rather than left to drift), keeping this in the same
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

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from oracle import catalogue
from oracle import mask as mask_mod
from oracle.catalogue import catalogue_points_path
from oracle.harness import open_bundle_with_source, spawn_server, stop_server
from oracle import wire
from oracle.wire import decode_viewport_with_subcells, split_frames

VIEW = "s0"
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
# emits (ports <= 65535, k <= 500, zoom <= 6, idset, shard id, HTTP status). The binary,
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
    element width — 8 for `tessera_id`; 4, deliberately, for `code`, whose own width is 8 but
    whose half-offset windows are the only ones that see a value straddling two adjacent positions
    (`_points_value_buffer_windows`). So only offsets that could carry a stored value, or a value
    smuggled across two of them, are considered."""
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
    """Returns `(tessera_id_windows, code_windows)`: every 8-byte-aligned window over the points
    batch's decoded `tessera_id` column value buffer, and separately every 8-byte window over the
    `code` column's value buffer — deliberately not the raw framed Arrow IPC bytes wholesale
    (module doc: framing is full of small, unremarkable buffer-length integers), and deliberately
    never a narrower-than-8-byte pass over `tessera_id` (module doc's chance-collision arithmetic).

    `tessera_id` is scanned at its own native stride (8 — one window per stored value, aligned,
    never byte-straddled). `code` — the 64-bit position that replaced the `x`/`y` `f32` pair — is
    scanned at **stride 4**, which is a superset of its own aligned windows and additionally sees
    a value straddling two adjacent codes' halves. That straddle is the same smuggling shape the
    pre-r6 sweep of two 4-byte float lanes existed to catch, so widening the column did not retire
    the case; it is why the stride did not follow the width.

    The two results are kept **separate**, not merged into one set: `tessera_id`'s windows are safe
    to compare against the full, unfiltered target-id set (module doc's `SAFE_ID_FLOOR` section);
    `code`'s are not, because a point at the extent origin has `code == 0`, which numerically
    equals entity id 0 — the same floor-filtering reasoning a genuine `0.0` coordinate needed, for
    the same reason. **One thing genuinely changed**: a build writing a `tessera_id` into the
    `code` column is now a plausible bug shape, which two float columns made implausible.
    Floor-filtering catches it for every id at or above the floor — the same guarantee
    `tessera_id`'s own column carries.
    """
    tessera_windows: set[int] = set()
    code_windows: set[int] = set()
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        for batch in reader:
            for name, width in (("tessera_id", 8), ("code", 8)):
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
                    # `code` at stride 4, not at its own width: the extra half-offset windows are
                    # what see a value straddling two adjacent codes. Kept separate from
                    # `tessera_id`'s windows below, because a point at the extent origin has
                    # `code == 0` — an ordinary position, not a bug — whose 8-byte window
                    # numerically equals entity id 0. That is exactly the "floor exists to keep
                    # small legitimate values out of the target set" problem the module doc
                    # describes for the log/text scans, just arriving via a different route (a
                    # real position rather than log noise) — so `code` windows are checked
                    # against the FLOOR-FILTERED target set, same as the log/decimal scans, not
                    # the full one.
                    code_windows |= _le_windows(trimmed, 8, stride=4)
    return tessera_windows, code_windows


def _subcell_value_buffer_windows(subcell_bytes: bytes) -> set[int]:
    """8-byte-aligned LE windows over the sub-cell batch's `cell` and `count` value buffers.

    The §3.3 underlay arrives as its own kind-2 frame (contracts §3.2 r26), handed back whole by
    `split_frames`. Before the underlay was swept at all, every one of its bytes was outside the
    scan — and the columns are exactly the shape that matters: `cell`
    is a Morton prefix up to 2^32-1 and `count` a small integer, both landing squarely in the dense
    entity-id neighbourhood the module doc's `SAFE_ID_FLOOR` reasoning was built for.

    Both columns are `uint64`, so a window is a single element rather than a straddle; they are
    returned together and compared against the FLOOR-FILTERED target set, for the same reason
    `code` is — a genuine `count` of 0 or a `cell` prefix of 0 is an all-zero window that
    numerically equals entity id 0.
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


# A real `high_tail` member, and above SAFE_ID_FLOOR — both asserted in the plant test rather than
# left to this comment. Resize `high_tail` and an unasserted constant would silently start
# exercising the sweeps against a value the real scan's target set does not contain: the control
# would still pass and would still prove the mechanisms fire, but no longer that they fire on a
# value class the real scan is watching for.
PLANTED_ENTITY_ID = 145_000


def test_the_catalogue_bundles_identity_column_agrees_with_its_key(catalogue_bundle):
    """The structural half of I10's assurance, against **the bundle this module scans**.

    The module doc leans on `Bundle.verify_identity_cross_check` as the thing that scrutinises the
    identity construction itself, so that the byte-scan does not have to. That citation was
    load-bearing because `reference/tests/test_identity.py` actually ran the check — against the
    Phase 0 fixture, which this module no longer uses. Repointing the scan at the catalogue would
    otherwise have quietly turned a live citation into a reference to a check nobody performs on
    the bundle under test. It runs here instead.
    """
    catalogue_bundle.verify_identity_cross_check(VIEW)


def _points_stream(tessera_ids: list[int], codes: list[int]) -> bytes:
    """A points batch in the wire's own schema (contracts §3.2), built by the harness."""
    schema = pa.schema(
        [
            pa.field("tessera_id", pa.uint64()),
            pa.field("code", pa.uint64()),
        ]
    )
    batch = pa.record_batch(
        [
            pa.array(tessera_ids, type=pa.uint64()),
            pa.array(codes, type=pa.uint64()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


def _subcell_stream(cells: list[int], counts: list[int]) -> bytes:
    schema = pa.schema([pa.field("cell", pa.uint64()), pa.field("count", pa.uint64())])
    batch = pa.record_batch(
        [pa.array(cells, type=pa.uint64()), pa.array(counts, type=pa.uint64())], schema=schema
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


def test_every_scan_mechanism_catches_a_planted_entity_id():
    """The negative control conformance §4.3 asks for: **a planted emission the scanner must
    flag.**

    The scan above is pass-only in the direction that matters. It asserts that no entity id appears
    anywhere, and a scan mechanism broken so that it never returns anything — a mis-parsed buffer,
    a wrong stride, a decoder that silently yields no batches — reports exactly the same green. The
    existing control (`known_tessera_id in tessera_id_windows`) proves the byte-window mechanism
    against **real traffic**, which is worth having and is a different claim: it shows the scan can
    recover a value that really was transmitted. It does not show the scan fires on an **entity
    id**, which is the value it exists to catch and the one that never legitimately appears.

    **The gap between the two controls is not hypothetical, and was measured rather than argued.**
    Filtering `_le_windows` to values `>= 2^32` — the shape of a plausible "suppress obviously
    spurious small windows" change, offered as noise reduction — leaves the scan above **passing**,
    because every real `tessera_id` is a uniform 64-bit value and sails over the filter. The plant
    below fails immediately, because an entity id is exactly the small value such a filter discards
    and exactly the value I10 is about. That sabotage was run on 2026-08-01: one passed, one failed,
    and the one that passed is the one that was there before.

    So each mechanism is handed a synthetic payload carrying `PLANTED_ENTITY_ID` in the encoding
    that mechanism is responsible for, and must return it. The plant lives here rather than in the
    server: making the real wire carry an entity id would mean building a route capable of emitting
    one, which is the leak this invariant forbids, gated behind a feature that must then never
    reach a release binary. The falsifiability question is a question about the scanner, and it is
    answerable where the scanner is.

    Each mechanism is also handed a clean payload and must return **nothing** from the target set.
    A scan that flagged everything would satisfy the plant while making the real assertions
    unfalsifiable in the other direction.
    """
    planted = PLANTED_ENTITY_ID
    assert planted in catalogue.BLOCKS["high_tail"].entities, (
        "the planted id is no longer a member of the catalogue's high_tail block, so it is not in "
        "the target set the real scan checks against and this control has drifted off it"
    )
    assert planted >= SAFE_ID_FLOOR
    targets = {planted}
    clean_ids = [0xDEAD_BEEF_1234_5678, 0x0BAD_C0DE_9876_5432]

    # 1. The `tessera_id` column, at the column's own 8-byte stride. This is the bug shape the
    #    module doc names: a server minting `tessera_id = entity_id as u64` instead of applying the
    #    keyed permutation, so the raw id sits zero-extended in the identity lane.
    tid_w, _code = _points_value_buffer_windows(_points_stream([planted], [0x0102_0304_0506_0708]))
    assert tid_w & targets, "the tessera_id column sweep did not catch a raw entity id in it"
    tid_clean, _ = _points_value_buffer_windows(
        _points_stream(clean_ids, [0x0102_0304_0506_0708, 0x0807_0605_0403_0201])
    )
    assert not (tid_clean & targets), "the tessera_id column sweep flagged a clean batch"

    # 2. The `code` column. Two plants, because the column carries two distinct bug shapes.
    #
    #    2a. **Aligned**: a whole entity id written into one `code` lane. This is the shape the
    #        float columns made implausible and a `u64` position column makes plausible — a build
    #        putting an identifier where a position belongs. Caught by any stride dividing 8.
    lane_w = _points_value_buffer_windows(_points_stream([1], [planted]))[1]
    assert lane_w & targets, "the code column sweep did not catch a raw entity id in one lane"

    #    2b. **Straddling**: the id's two halves in the high half of one code and the low half of
    #        the next, which is what a leak smuggled through a position buffer looks like. This is
    #        the case that pins the stride: it can only be seen by a window starting at byte 4, so
    #        changing the sweep from `stride=4` to `stride=8` deletes precisely this and leaves
    #        2a passing. The same sabotage was measured against the pre-r6 `x`/`y` lanes.
    lo = planted & 0xFFFF_FFFF
    hi = planted >> 32
    straddle_w = _points_value_buffer_windows(_points_stream([1, 2], [lo << 32, hi]))[1]
    assert straddle_w & targets, (
        "the code column sweep did not catch an entity id straddling two adjacent codes"
    )

    clean_code = _points_value_buffer_windows(
        _points_stream([1, 2, 3], [1 << 40, 2 << 40, 3 << 40])
    )[1]
    assert not (clean_code & targets), "the code column sweep flagged a clean batch"

    # 3. The §3.3 underlay's appended sub-cell stream.
    sub_w = _subcell_value_buffer_windows(_subcell_stream([planted], [7]))
    assert sub_w & targets, "the sub-cell sweep did not catch a raw entity id in the cell column"
    assert not (_subcell_value_buffer_windows(_subcell_stream([1 << 40], [7])) & targets), (
        "the sub-cell sweep flagged a clean batch"
    )

    # 4/5. The log and the drill-down body: text, so both an ASCII decimal run and a raw LE integer
    #      at each width the real sweep uses. Note the decimal plant is embedded in a realistic log
    #      line rather than standing alone, so the digit-run regex is exercised on the shape it
    #      actually meets — a bare `str(planted)` would pass a scanner that only matched whole
    #      buffers.
    log_line = f"2026-08-01T00:00:00Z INFO tessera_engine: gathered entity_id={planted} rows=1\n"
    log_bytes = log_line.encode() + b"prefix" + planted.to_bytes(8, "little") + b"suffix"
    assert _decimal_windows(log_bytes, SAFE_ID_FLOOR) & targets, (
        "the decimal-text sweep did not catch an entity id written into a log line"
    )
    assert _le_windows(log_bytes, 8) & targets, "the 8-byte binary log sweep did not catch a plant"
    assert _le_windows(log_bytes, 4) & targets, "the 4-byte binary log sweep did not catch a plant"

    clean_log = b"2026-08-01T00:00:00Z INFO tessera_server: served zoom=4 k=20 status=200\n"
    assert not (_decimal_windows(clean_log, SAFE_ID_FLOOR) & targets), (
        "the decimal-text sweep flagged a clean log line"
    )
    assert not (_le_windows(clean_log, 8) & targets), "the binary log sweep flagged a clean line"

    # The floor is what keeps the text sweeps from flagging the harness's own small integers. An
    # id below it is invisible to them by construction, which is a real and stated limit of the
    # text half — asserted, so that lowering the floor without revisiting the reasoning fails here.
    assert not (_decimal_windows(b"served k=500 zoom=6 status=200\n", SAFE_ID_FLOOR)), (
        "SAFE_ID_FLOOR no longer excludes the legitimate small integers this harness emits"
    )


@pytest.fixture(scope="module")
def byte_scan_server(tmp_path_factory, catalogue_bundle_root):
    """A dedicated server instance for this module, logging to a file (not an unread pipe) so the
    full RUST_LOG=info output can be scanned after the run."""
    tmp_dir = tmp_path_factory.mktemp("byte-scan-server")
    log_path = tmp_dir / "server.log"
    srv, proc = spawn_server(
        catalogue_bundle_root,
        tmp_dir,
        log_path=log_path,
        env_extra={"RUST_LOG": "info"},
    )
    yield srv, log_path
    stop_server(proc)


def test_no_entity_id_key_or_misplaced_external_id_crosses_the_wire_or_appears_in_logs(
    byte_scan_server, catalogue_bundle_root: Path
):
    server, log_path = byte_scan_server
    oracle_bundle = open_bundle_with_source(
        catalogue_bundle_root, catalogue_points_path()
    )
    assert oracle_bundle.identity_key is not None, "fixture must be a post-r6 bundle"

    # The grant set is **named, not sliced**. A "first half of the dictionary" grant admits and
    # denies whatever the corpus happens to lay out first, and on this corpus that puts every
    # entity id above `SAFE_ID_FLOOR` on one side of the grant — which fails the floor
    # preconditions below for a reason that reads as a fixture accident rather than as the
    # deliberate layout property it is. `high_tail` is granted and `filler_tail` is not, so ids
    # above the floor exist on both sides; `boundary` and `cross_hi` come along to keep the
    # admitted set substantial (so there is plenty to find if something leaks) and to keep the mask
    # spanning more than one Roaring container.
    granted_blocks = ("boundary", "cross_hi", "high_tail")
    granted_descriptors = [catalogue.BLOCKS[n].descriptor.encode("ascii") for n in granted_blocks]
    granted_terms = {catalogue.BLOCKS[n].term_id for n in granted_blocks}

    auth = server.authorise([d.decode("ascii") for d in granted_descriptors])
    token = auth["token"]

    admitted = mask_mod.mask_of(granted_terms, oracle_bundle.pairs_path())

    seg = oracle_bundle.segment(VIEW)
    all_entities = {int(e) for e in seg.entity_id.tolist()}
    denied = all_entities - admitted

    # Unfiltered target set — used ONLY for the tessera_id column's own scan (module doc: no
    # floor needed there; `code` and the log/item text scans below use the floor-filtered set).
    target_ids = admitted | denied
    assert admitted, "fixture/grant choice must admit >= 1 entity for this test to mean anything"
    assert denied, "fixture/grant choice must deny >= 1 entity for this test to mean anything"

    # Floor-filtered target set — used for `code`'s windows, the log (binary + decimal) and the
    # /v1/items decimal scan (module doc).
    admitted_high = {e for e in admitted if e >= SAFE_ID_FLOOR}
    denied_high = {e for e in denied if e >= SAFE_ID_FLOOR}
    assert admitted_high, "fixture/grant choice must admit >= SAFE_ID_FLOOR entities"
    assert denied_high, "fixture/grant choice must deny >= SAFE_ID_FLOOR entities"
    # Both preconditions above are properties of the catalogue's layout, guaranteed by its
    # `high_tail` block and checked by `catalogue.verify()`. This equality is what stops the two
    # from drifting apart — the layout would go on satisfying a floor this scan no longer uses.
    assert SAFE_ID_FLOOR == catalogue.HIGH_ID_FLOOR
    target_ids_high = admitted_high | denied_high

    identity_key = oracle_bundle.identity_key
    identity_key_hex = oracle_bundle.manifest["identity"]["key"]
    identity_key_raw = bytes.fromhex(identity_key_hex)

    tessera_id_windows: set[int] = set()
    code_windows: set[int] = set()
    sampled_ids: set[int] = set()
    all_raw_responses: list[bytes] = []
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    subcell_windows: set[int] = set()
    subcell_rows_seen = 0
    for zoom in ZOOM_RANGE:
        # Ask for the §3.3 underlay on every request, so the appended third stream is actually
        # produced and swept. Without this the stream never existed during the scan at all.
        raw = server.viewport(token, VIEW, zoom, bbox, k=K, underlay_offset=UNDERLAY_OFFSET)
        all_raw_responses.append(raw)
        # The framed body (contracts §3.2 r26): every frame is tagged and length-prefixed, and
        # `split_frames` REFUSES an unknown kind — that refusal is what replaced the old "nothing
        # may sit unscanned after the sub-cell stream" tail check: a new frame kind cannot arrive
        # unswept, because the decoder every consumer shares will not decode the body at all
        # until this scan learns about it.
        frames = split_frames(raw)
        subcell_frames = [p for kind, p in frames if kind == wire.FRAME_SUB_CELLS]

        _t2, _p2, sub_cells = decode_viewport_with_subcells(raw)
        subcell_rows_seen += len(sub_cells)
        for payload in subcell_frames:
            subcell_windows |= _subcell_value_buffer_windows(payload)
        # Scope decision (module doc): the tile batch (visible/matched counts) is deliberately
        # excluded from the scan — those are I2-legitimate aggregates, not a surface I10 governs
        # — and so is the trailer, whose closed key set of timing figures the shared decoder
        # validates (`oracle.wire.decode_frames`). Only the points batches' decoded column
        # *value buffers* are scanned, per points frame.
        for kind, payload in frames:
            if kind != wire.FRAME_POINTS:
                continue
            tid_w, code_w = _points_value_buffer_windows(payload)
            tessera_id_windows |= tid_w
            code_windows |= code_w
            sampled_ids.update(_decode_tessera_ids(payload))
    # Used for checks (identity key, external ids) that need to look at the whole points batch,
    # not just the entity-id sweep's column-specific split above.
    all_points_windows = tessera_id_windows | code_windows

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
    # negligible chance-collision risk for a uniform 64-bit column). `code`'s windows are checked
    # against the floor-filtered set only, because a point at the extent origin has `code == 0`,
    # which numerically equals entity id 0 (module doc, `_points_value_buffer_windows`).
    leaked_tid = tessera_id_windows & target_ids
    assert not leaked_tid, (
        f"found {len(leaked_tid)} entity id(s) encoded as an 8-byte-aligned LE integer in the "
        f"tessera_id column of a viewport points batch: {sorted(leaked_tid)[:20]}"
    )
    leaked_code = code_windows & target_ids_high
    assert not leaked_code, (
        f"found {len(leaked_code)} entity id(s) encoded as an 8-byte LE integer in — or spanning "
        f"two adjacent lanes of — the code column of a viewport points batch: "
        f"{sorted(leaked_code)[:20]}"
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
