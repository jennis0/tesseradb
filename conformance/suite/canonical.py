"""Response canonicalisation — correctness-suite §12.2's six steps, implemented once.

Canonicalisation exists because two issues of one request are never byte-identical: the
`/v1/viewport` trailer carries elapsed wall-clock (`stream_us`, `arrow_serialise_ns` — contracts
§3.2), and the tiles batch's emission order under a parallel gather is not contract. A comparison
of raw bodies therefore flakes, and a flaking byte-compare gets "fixed" by weakening the
comparison — which is how a suite stops being able to fail. The canonical form removes exactly the
two sources of legitimate variation and nothing else, so that any remaining byte difference is a
defect.

**A streamed response canonicalises to separately-addressable surfaces, never one concatenated
blob.** That is not fastidiousness: the canary comparator originally returned a single blob, and a
review found that dropping the points batch entirely left every test green, because the control
fired on whatever remained. Split, each surface is pinned — a canonicalisation that silently
stopped comparing one of them fails the canary's positive control, which asserts per surface.

The steps, numbered as §12.2 numbers them:

1. Decode the frame sequence — `u8 kind` + `u32 LE length` + payload, each payload a complete
   Arrow IPC stream, JSON for the trailer. Delegated to `oracle.wire`, the one strict Python
   decoder: truncation, an unknown kind, a misplaced frame, a missing trailer or a trailer key
   outside the closed set all refuse rather than canonicalise.
2. Trailer: drop the elapsed-time fields, keep the rest. The remainder (`points`, `flushes`) is a
   compared surface — dropping the whole trailer would also make two issues identical, but would
   stop checking the two counts that are deterministic functions of served content.
3. Tiles: re-serialised sorted by tile id. Emission order under a parallel gather is not contract.
4. Points: the kind-3 payloads concatenated, compared as bytes in served order. Contracts §3.2
   orders points ascending by `tessera_id` within each tile, so comparing them *unsorted* is
   stronger than sorting them — a reordering is a defect, not noise.
5. The underlay: its own stream, its own comparison.
6. Headers are excluded — this module takes body bytes only. §12.2's exception ("except where a
   stage's entitlement is about one") is the driver's business (build-order row 3), not a second
   input here.

**Step 4 rests on chunk boundaries being a function of served content alone.** Frame boundaries
are explicitly not contract (contracts §3.2: "a reader must accept any chunking"), and the
concatenated payloads carry per-frame stream headers — so a server that re-chunked on timing would
break the comparison without any served-content defect. It holds today: the emitter flushes on an
accumulated-size threshold, which depends only on what is served. The trailer's `flushes` count
rides on the same assumption, which is why keeping it (step 2) costs nothing the concatenation has
not already spent. `test_canonical.py` pins the sensitivity, so if a timing-based flush is ever
introduced this assumption fails a test instead of surfacing as an unreproducible flake.

One field the steps do not mention: `stage_ns`, the trailer's double-gated diagnostics key
(contracts §3.2), is elapsed time too, but is absent in every configuration this suite runs — the
gate is never enabled here. It is deliberately *not* in the dropped set: §12.2 names exactly two
fields, and a run against a stage-timing build should fail loudly as nondeterminism rather than
have its diagnostics silently swallowed.
"""

from __future__ import annotations

import io
import json
from dataclasses import dataclass

import pyarrow as pa
import pyarrow.ipc as ipc

from oracle import wire

#: §12.2 step 2 — the trailer fields that legitimately differ between two issues of one request.
ELAPSED_TIME_FIELDS = frozenset({"stream_us", "arrow_serialise_ns"})


@dataclass(frozen=True)
class Json:
    """A plain JSON response — `/v1/meta`, `/v1/categories/{column}`, `/v1/items/{id}`.

    Dict equality is key-order-insensitive, which is the whole canonicalisation these surfaces
    need: nothing in them is elapsed time and nothing is emission-ordered by a parallel gather.
    """

    payload: dict


@dataclass(frozen=True)
class Streamed:
    """A canonicalised `/v1/viewport` response: the three served surfaces, plus the trailer's
    deterministic remainder.

    `tiles`, `points` and `underlay` are §12.2's three separately-addressable surfaces. `underlay`
    is `b""` when the request did not ask for one, and a *non-empty* schema-only stream when it
    asked and no cell qualified — contracts §3.2's rule that presence is a property of the request
    is preserved rather than flattened. `trailer` is the kind-4 object minus the elapsed-time
    fields, re-serialised with sorted keys; it is a fourth compared surface because its `points`
    and `flushes` counts are deterministic functions of served content (module doc).
    """

    tiles: bytes
    points: bytes
    underlay: bytes
    trailer: bytes

    def surfaces(self) -> dict[str, bytes]:
        """The compared surfaces by name — what a comparator iterates so a difference is reported
        *at the surface it landed on*, never as "the response differs"."""
        return {
            "tiles": self.tiles,
            "points": self.points,
            "underlay": self.underlay,
            "trailer": self.trailer,
        }


@dataclass(frozen=True)
class Batches:
    """`/v1/region`'s canonical form: plain Arrow in three batches — summary, preview,
    breakdowns (contracts §3.2) — a shape neither of the other two arms represents.

    ⊘ The arm exists so `Recorded` can type the response; no canonicaliser is written for it,
    because the route is not in the router and there are no wire bytes to write one against. The
    battery carries the route as a marked absence (`suite.battery.Absent`), and
    `test_battery.py` pins the absence so the day the route lands, a test fails and this
    docstring is the thing it points at.
    """

    batches: tuple[bytes, ...]


Canonical = Json | Streamed | Batches


def canonicalise_viewport(body: bytes) -> Streamed:
    """One `/v1/viewport` body, canonicalised per the module doc's six steps.

    Refuses (raises) rather than canonicalises anything malformed: a truncated body, an unknown
    frame kind, a missing trailer, a trailer key outside the closed set, or a trailer whose
    `points` count disagrees with the body — all via `oracle.wire.decode_frames`, so this module
    cannot acquire a laxity that decoder does not have.
    """
    # Step 1, and the grammar check. The decoded rows are discarded — the canonical form is bytes,
    # not decoded values — but the decode is what enforces the closed trailer key set and the
    # served-sum and points-count consistency rules before any bytes are trusted.
    _tiles, _points, _sub_cells, trailer = wire.decode_frames(body)
    frames = wire.split_frames(body)

    # Step 2. Sorted keys and fixed separators so the remainder has one serialisation; the wire
    # object is server-authored JSON whose key order is nobody's contract.
    kept = {k: v for k, v in trailer.items() if k not in ELAPSED_TIME_FIELDS}
    trailer_bytes = json.dumps(kept, sort_keys=True, separators=(",", ":")).encode()

    # Step 3. `combine_chunks` before writing so the canonical stream is a single batch whatever
    # the emitter's internal chunking was — tile emission chunking is no more contract than tile
    # emission order.
    tiles_payload = next(payload for kind, payload in frames if kind == wire.FRAME_TILES)
    with ipc.open_stream(io.BytesIO(tiles_payload)) as reader:
        table = pa.Table.from_batches(list(reader), reader.schema)
    sorted_tiles = table.sort_by([("tile", "ascending")]).combine_chunks()
    sink = io.BytesIO()
    with ipc.new_stream(sink, sorted_tiles.schema) as writer:
        for batch in sorted_tiles.to_batches():
            writer.write_batch(batch)

    # Steps 4 and 5. Concatenation in frame order is served order; the underlay's single payload
    # (the grammar admits at most one kind-2 frame) joins over the empty sequence to `b""` when
    # unrequested, keeping absent distinct from schema-only-empty.
    points_bytes = b"".join(payload for kind, payload in frames if kind == wire.FRAME_POINTS)
    underlay_bytes = b"".join(payload for kind, payload in frames if kind == wire.FRAME_SUB_CELLS)

    # Step 6 is structural: this function's one parameter is the body.
    return Streamed(
        tiles=sink.getvalue(),
        points=points_bytes,
        underlay=underlay_bytes,
        trailer=trailer_bytes,
    )
