# Streamed serving — the viewport response as a frame sequence

**Status:** Implemented, 2026-08-11 — r2, reviewed twice (design and implementation, both
independent; Appendix R carries both trails). r1 was written against the direction settled in
`docs/evidence/memos/2026-08-11-streaming-handover-server-reply.md` (tile-major v1; global
id-major merge declined; banded emission reserved); r2 dispositioned the design review's
thirteen findings, and the implementation review's six were dispositioned in code (Appendix R).
Companion to `delta-serving.md`, whose §7 prefix-drawing licence is what makes every cut of
this stream drawable, and to `client-interaction.md` §8.6(2), whose length-prefix-everything
item this delivers. `contracts.md` §3.2 r26 now carries the wire byte layout and governs.
All three flagged rulings are settled (owner, 2026-08-11): the streaming-occupancy posture is
[decision 0060](../decisions/0060-a-stream-lives-at-most-the-whole-stream-deadline.md), the I13a
carve-out is ratified as [decision 0061](../decisions/0061-i13a-forbids-undetectable-partials-not-streaming.md),
and Appendix C's C4 row records the accepted granularity change within its open status.

## 1. What changes and what does not

`POST /v1/viewport` stops returning one monolithic body and returns a **sequence of
self-contained, length-prefixed frames**: counts first, then point chunks as they are gathered,
then a trailer. Selection, masking, counts-exactness and every refusal are untouched — the
stream is a serialisation order, not a selection rule. I7 is evaluated exactly as today; I2's
quantities are unchanged; the response's logical content at completion is byte-derivable from
today's (same tiles, same points, same sub-cells, chunked).

The engine's request pipeline splits at the seam it already has:

- **Sweep** — generation load, session geometry, compose, θ anchor, tile ranges, then the
  existing parallel `tile_result` fan-out *minus the gather*: count, select, underlay per tile.
  Unchanged in cost model and calibration behaviour (`SERIAL_FALLBACK_MAX_ROWS`,
  `TILE_PAR_MIN_TILES`, indexed-collect ordering all survive).
- **Emit** — a serial pass over the swept tiles in response order: gather each tile's selected
  rows (`gather_tile_columns`, unchanged), accumulate into a chunk, flush at a size threshold.

`GET /v1/meta`, `/v1/categories`, `POST /v1/items` and every other route are unchanged.

## 2. The wire frame

A response body is a sequence of frames, each:

```text
u8      kind
u32 LE  length of payload
<payload: length bytes>
```

| kind | name | payload | count |
|---|---|---|---|
| 1 | tiles | Arrow IPC stream `(tile, visible, matched, served — all uint64)` | exactly one, first |
| 2 | sub-cells | Arrow IPC stream `(cell: uint64, count: uint64)` | exactly one iff the request asked for the §3.3 underlay, immediately after *tiles* |
| 3 | points | Arrow IPC stream `(tessera_id: uint64, code: uint64, …declared scalars)` | zero or more |
| 4 | trailer | JSON object (§6) | exactly one, last |

Every frame is a **complete, independently decodable** Arrow IPC stream (or JSON, for the
trailer) — a reader never walks Arrow message framing to find a boundary, which deletes the
per-language porting cost §8.6(2) records and the desynchronisation bug class the shipped
client's `frame.ts` documents. An unknown `kind` is a decoder error, never skipped: skipping
would let a future frame kind carry data an old reader silently drops.

The batch schemas are exactly today's, including `served`'s appended position. The sub-cell
frame keeps contracts r12's presence rule: requested-but-empty is a schema-only zero-row
stream inside a present frame; unrequested is **no frame at all**. Empty responses keep the
tiles frame (schema-only) and emit zero points frames.

Point chunks concatenate to exactly the points stream a monolithic response would have carried:
each frame holds whole tiles' worth of points; a chunk boundary never splits a tile. Chunk
boundaries are **not contract** — a reader must accept any chunking, including a single frame.
The server flushes at `serve.stream_flush_bytes` (default 1 MiB) of estimated payload,
whichever tile boundary first crosses it. There is deliberately **no time-based flush**: after
the sweep, production is memcpy-speed, so a timer buys nothing and would make response bytes
nondeterministic (§7).

## 3. Ordering

- **Within a tile: ascending `tessera_id`** — unchanged (contracts §3.2 r7), and the property
  every cut's validity rests on: a truncated stream leaves each tile holding an id-order prefix
  of `served(T)`, which is exactly what `delta-serving.md` §7 licenses a client to draw.
- **Across tiles: the tiles-batch order**, and points frames concatenate in it. For the `tiles`
  request form this is the **request's own order** (duplicates removed, first occurrence kept) —
  a client orders its list centre-out and receives it centre-out, which is what retires its
  response-piece-splitting machinery. For the `bbox` form it is the derivation's raster order,
  as today. (Today's boundary re-sorts the list; the engine's range derivation sorts
  *internally* and is order-independent at the interface.)
- **A client must not assume any cross-tile grouping or ordering beyond the tiles batch's
  declaration.** v1 in fact emits tile-major, but the contract is written one notch looser so a
  future emission-order change (banded, per the server-reply memo §2) is not a wire break.

## 4. The engine seam

```rust
pub struct ViewportHead {          // known before the sweep
    pub coordinates: ViewCoordinates,
    pub stamp: GenerationStamp,
    pub stale: bool,
    pub scalar_names: Vec<String>,
}

pub trait ViewportSink {
    /// Sweep complete: every tile's counts and the underlay. Exactly once, before any points.
    /// `sub_cells` is `None` when the request did not ask for the underlay, `Some` (possibly
    /// empty) when it did — the frame-presence rule (§2) needs the distinction, and an empty
    /// view cannot carry it.
    fn counts(&mut self, tiles: &[TileCount], sub_cells: Option<&[SubCellCount]>) -> SinkResult;
    /// One flush chunk: whole tiles' points, response order. Zero or more times.
    fn points(&mut self, chunk: PointColumns) -> SinkResult;
}

impl Engine {
    pub fn viewport_stream(
        &self, session: &Session, req: ViewportRequest<'_>,
        flush_bytes: usize, sink: &mut dyn ViewportSink,
    ) -> Result<(ViewportHead, StageTimings)>; // head also handed to a caller-supplied observer
}
```

(The exact head-delivery mechanism — return value versus a first sink call — is an
implementation choice; what is load-bearing is that the head is available to the server before
the sweep runs, because the HTTP headers derive from it.)

A sink returning "closed" aborts the request as a cancellation (D-C posture): the client went
away, the remaining work is abandoned at the next checkpoint. `Engine::viewport` remains, as a
collecting sink over the same producer, returning today's `ViewportOut` — the batch surface for
`tessera-bench`, tests, and any embedder; its output equals today's field for field.

The sweep's `tile_result` splits into `tile_sweep` (count + select + underlay; returns the
selected rows and the tile's resolved parts) and the existing `gather_tile_columns`. The emit
pass is serial on the calling thread: per tile, a cancellation check, gather, append; flush at
the threshold. Serial is a deliberate v1 choice — obviously correct, and its wall cost overlaps
transmission; if a measurement later shows the emit pass binding, parallel gather-ahead inside
the emit loop is a contained change.

## 5. The server transport

The handler validates and admits exactly as today, then runs the engine call in
`spawn_blocking` with a sink that serialises frames (`tessera-wire`) and hands them to the
async side:

- **First flush before status.** The handler awaits the sink's first delivery — head plus the
  serialised tiles/sub-cells frames — before constructing the `Response`. Every error up to and
  including the sweep therefore keeps its typed status exactly as today — including
  `Cancelled → 500 fail-closed` (`error.rs`'s deliberate mapping; there is no 499), which
  remains correct here since a pre-status cancellation means the requester is gone and no one
  reads the status. Only the emit phase can fail after the status is committed (§6).
  Time-to-first-byte is unchanged by this wait: the first flush *is* the earliest byte the
  protocol can send.
- **Headers stay headers**: `etag`, `x-tessera-identity-key`, `x-tessera-pin`,
  `x-tessera-stale`, `x-tessera-admission-us` as today. `x-tessera-server-us` becomes
  **time-to-first-flush** (post-admission to sweep complete); the total lands in the trailer.
  `x-tessera-stage-ns` is retired as a header and moves into the trailer under the same double
  gate.
- **Body**: a bounded channel (capacity 2 flushes) feeds `axum::body::Body::from_stream`. The
  emit loop blocking-sends; a full channel is backpressure — the gather pauses with the client.
  Two deadlines bound a reader that stops or drips: a send that stalls longer than
  `serve.stream_write_stall_ms` (default 10 s) aborts the stream, and the whole emit phase may
  not outlive `serve.stream_deadline_ms` (default 60 s) from first flush — without the second, a
  reader that accepts one flush per `stall_ms − ε` holds its slot for minutes, legally (review
  finding 2). Nothing is ever buffered unboundedly (client criterion 6). **The whole-stream
  deadline is the ruled posture** ([decision 0060](../decisions/0060-a-stream-lives-at-most-the-whole-stream-deadline.md)):
  it bounds slot occupancy at `slots × deadline` absolutely, at the cost of cutting a genuinely
  slow link on a large response; the alternatives (a separate streaming-lane bound; acceptance
  with arithmetic) were considered and declined there, with the separate lane named as the
  design to reopen if legitimate slow readers ever exist.
- **Permits**: both `GatePermits` are held through the sweep; the **compute permit is released
  when the sweep completes**, and the **slot permit at emit-loop exit** — normal completion,
  error, stall-shed or deadline — because the closure owns the permits and its return is the
  one deterministically reachable release point. (Tying release to "stream end" is
  unenforceable: hyper stops polling an unread body and nothing forces its drop — review
  finding 1.) After the closure returns, an unread connection holds at most the channel's ~2
  buffered flushes plus hyper's write buffer, and no permits. Rationale for the split: today a
  40 MB response occupies a permit for ~150 ms because the buffer decouples compute from the
  client's read pace; under streaming the emit phase runs at the client's pace, and holding
  compute for it would let slow-but-healthy readers starve the gate. Emit-phase CPU is
  naturally paced by the channel bound; emit-phase concurrency is bounded by the slots
  semaphore — `compute_admission + compute_queue`, a deliberately looser bound than the gate's
  serialise-oversubscription licence, stated rather than implied. `/control/status` gains a
  `streaming` gauge, and `waiting` subtracts it — a slot-holding, compute-released request is
  otherwise counted as phantom queue depth (review finding 8).
- **The blocking pool arithmetic, stated**: each client-paced emit loop parks one
  `spawn_blocking` thread; parked threads are bounded by the slots semaphore (144 at the
  default `compute_admission = 48`, `compute_queue = 96`) against tokio's 512-thread pool,
  shared with `/v1/items`, `/session/authorise` and the ingest handlers — whose own admission
  bound exists precisely because pool exhaustion hangs admitted work. Headroom is real but no
  longer incidental; the deadline is what keeps it bounded.
- **Cancellation**: the `CancelGuard` moves into the body stream. A client disconnect drops the
  body, which flips the token (sweep checkpoints observe it) and drops the channel receiver
  (the emit loop's next send fails immediately). Disarmed when the stream completes normally.
- **Retention**: the generation `Arc` and the session-geometry entry are now held for the emit
  phase rather than for compute alone — bounded by the whole-stream deadline. A superseded
  generation's mmaps can therefore linger up to `stream_deadline_ms` past a swap; the
  compaction disk gauges were not sized against longer.

## 6. Trailer, errors, truncation

The trailer is a JSON object with exactly these keys: `{"stream_us": <post-admission wall
microseconds to trailer emission — includes client-paced channel waits, and is therefore a
stream figure, not a server-cost figure>, "arrow_serialise_ns": <sum over frames>,
"points": <total served>, "flushes": <point frames>}`, plus `"stage_ns": "<csv>"` under the
same build-feature-and-config double gate as the old header. The emit phase's `gather_ns` laps
bracket gather-and-append only, never a channel send — a slow client must not inflate a figure
documented as CPU cost. **The latency gate's figure is the `x-tessera-server-us` header** —
post-admission to first flush, the server-cost part of the request — and the bench scripts
read it there as before; the trailer's `stream_us` is deliberately named differently so no
consumer mistakes one for the other. The trailer's **presence is the completeness signal**: a
body without a trailing kind-4 frame is an incomplete response, whatever the transport said.

An engine error during the emit phase — reachable only as a malformed bundle or store fault,
since selection is already done — sends an error through the body stream, which aborts the
connection mid-body, and no trailer is emitted. Fail-closed twice over: transport truncation
*and* the missing trailer. The delivered prefix remains sound — every frame was computed from
the one generation snapshot, the counts are exact, and each tile's delivered points are an
id-order prefix of `served(T)` — so a client may keep it as a partial band under delta-serving
§7's rules; it must simply not mark the response complete. **I13a is not touched by this**: its
text governs shared work observed by *other* requests (single-flight slots, cached builds),
and nothing here changes what a concurrent request can observe. The `viewport.rs` posture note
("no partial `ViewportOut`") gains the carve-out at the claim: the requesting client's own
truncated stream is client-detectable and prefix-valid.

## 7. Determinism and conformance

For a fixed request, config and corpus state, the body is byte-deterministic **except the
trailer** (it carries timings). Flush boundaries are pure functions of the data and
`stream_flush_bytes`; there is no timer. Byte-equality assertions (serial-vs-parallel,
warm-vs-cold, cross-session) compare the frame sequence excluding kind 4 — but the trailer is
**canonicalised, not ignored**: its key set is asserted to be exactly §6's, so the one
server-authored JSON region of the body cannot quietly acquire a session-dependent field the
comparator would never see (review finding 12; the conformance suite's "nothing stripped from
the body" premise dies with the trailer, and this is what replaces it). The conformance
canonicalisation otherwise keeps its shape: tiles sorted (emission order was already
non-contract for the tile batch), points compared as concatenated frame payloads in served
order, sub-cells as today. The reference oracle's decoder walks frames instead of one length
prefix — a smaller parser than the one it replaces.

## 8. Batch consumers

One verb, one framing. A batch reader consumes the stream to the end and concatenates points
frames; the SDK grows a collect helper; `Engine::viewport` remains the in-process batch
surface. The tile-addressed adapter (`tile-addressed-integration.md`) consumes the engine API
and is unaffected. No `?stream=0` mode: two framings is two conformance surfaces forever.

**The TypeScript client reads the body as it arrives** (built 2026-08-28): it frames the byte
stream incrementally, decodes each points frame the moment it is whole, and lands its tiles as
bands before the next frame has been received — so a wide answer draws progressively instead of
after its last byte. The two properties this rests on are §2's and §3's: a frame is an
independently decodable Arrow stream, and it holds whole tiles in the tiles batch's order, so the
run of counts a frame satisfies is found by adding up the `served` the server already sent. §6's
rules are unchanged by it — the trailer's presence is still what marks the response complete, and
a body that ends without one is still refused after every whole frame it did deliver has been
handed over. Its batch path is unchanged and is what a reader holding a whole body still uses.
Measured on GeoNames at depth 10 over 24,960 tiles (65.4 MB, 60 point frames, 14,343 bands, Node,
inline decoder, loopback): first band **435–549 ms → 31–39 ms**, whole response 438–553 →
280–317 ms.

## 9. Edits this lands (the implementation's checklist)

- `tessera-wire`: frame writer (`kind` + length + payload), per-frame encoders, **in
  `payload.rs`** — the module name is load-bearing, because `check-layers.sh`'s I10 rule greps
  that file by name and a rename would leave the rule permanently green (review finding 7). The
  monolithic `viewport_ipc` is deleted (0048 — artifacts recreated, no reader outside this
  repo). The I10 layering is unchanged: plain `tessera_id`/scalar views in, bytes out.
- `tessera-engine`: `tile_sweep`/gather split, `ViewportSink`, `viewport_stream`,
  `Engine::viewport` as collector; emit-phase cancellation checks; stats accounting for the
  emit pass (`gather_ns` moves there, bracketing gather only — §6; per-tile sweep stats
  unchanged). `ViewportRequest::tiles`' doc drops its "sorted by the caller boundary" claim.
- `tessera-server`: streaming handler as §5; `GatePermits::release_compute`;
  `serve.stream_flush_bytes`, `serve.stream_write_stall_ms` and `serve.stream_deadline_ms`
  (all refuse 0); the `streaming` status gauge; header changes as §5; trailer as §6. The
  boundary's tile dedup becomes order-preserving first-occurrence, and is named in the
  contract.
- `contracts.md` §3.2: frame table, ordering clause, tile order = request order for the `tiles`
  form (with the first-occurrence dedup rule), trailer/truncation semantics, header table
  update including the C4 note. `api_version` stays 1 on r7's inherited argument (no published
  deployment; in-repo readers move in lockstep).
- `architecture.md`, at the claim (decision 0013's form), both flagged for owner ratification:
  **§4 I13a** — the headline "no partial answer" gains the streamed-response carve-out (a
  truncated stream is marked incomplete by its missing trailer and its delivered prefix is
  exact; what I13a forbids — a partial observed as complete, by anyone — still holds);
  **Appendix C C4** — streaming publishes the sweep/emit timing split unconditionally
  (first-flush header, frame pacing) where today it is double-gated behind `bench-timing`; the
  register entry records the granularity change (review findings 3–4).
- Consumers, in lockstep: `reference/oracle/wire.py`, `clients/ts/core` (`frame.ts`,
  `decode.ts`; the TypeScript client decodes **incrementally** — built 2026-08-28, §8),
  `tessera-server/tests/http.rs`'s decoder, conformance tests that touch framing, bench
  scripts reading `x-tessera-server-us` (meaning narrows to first-flush; the gate keeps it)
  and `x-tessera-stage-ns` (moves to the trailer).

## 10. Rejected alternatives (recorded, with the argument)

- **Global id-major emission** — declined; the server-reply memo §2 carries the argument
  (merge cost lands ahead of the first point flush; abandoned streams pay ~100% of compute;
  locality). Banded emission is the reserved refinement, reachable later without a wire break
  because of §3's looseness.
- **Timer-based flushes** — declined for determinism (§2, §7).
- **Streaming during the sweep** — impossible under counts-first: `served` needs `C_θ`, which
  the selection scan itself produces, so the first flush is gated on the sweep by the
  definition, not by the implementation.
- **A second, monolithic wire mode** — declined (§8).

## Appendix R — review trail

**r1 → r2 (2026-08-11).** One independent adversarial review, thirteen findings, dispositioned
in a single pass; the reviewer's verdict was "needs rework on the permit lifecycle, not on the
wire or the engine seam", and r2 is that rework. All thirteen accepted:

1. *(major)* Slot-release point unenforceable at "stream end" → released at emit-loop exit;
   post-closure residue stated (§5).
2. *(major)* Client-paced slot occupancy is a new DoS shape 0059 never priced → whole-stream
   deadline added, marked provisional pending the owner ruling the server-reply memo
   already flagged (since ruled — [decision 0060](../decisions/0060-a-stream-lives-at-most-the-whole-stream-deadline.md)).
3. *(major)* I13a's headline text contradicts streamed truncation → architecture §4 annotation
   at the claim, flagged for owner ratification (§9).
4. *(major)* C4's register entry describes the coarser channel → Appendix C annotation for the
   granularity change (§9).
5. *(minor)* "499" did not exist → status text corrected; `Cancelled → 500` mapping kept (§5).
6. *(minor)* `counts()` could not express the underlay presence rule → `Option<&[SubCellCount]>`
   (§4).
7. *(minor)* Deleting `viewport_ipc` could vacate check-layers' filename grep → frame writer
   bound to `payload.rs` (§9).
8. *(minor)* `waiting` gauge corrupted by the split lifecycle → `streaming` gauge added (§5).
9. *(minor)* Blocking-pool claim implicit → arithmetic stated (§5).
10. *(minor)* Trailer total conflated with server cost → renamed `stream_us`; gate figure
    pinned to the first-flush header; `gather_ns` brackets gather only (§6).
11. *(minor)* Stale doc text (`ViewportRequest::tiles`, boundary dedup) → in the edit list, and
    the dedup rule promoted to contract (§9).
12. *(note)* Trailer excluded wholesale would un-guard a body region → trailer canonicalised by
    key set instead (§7).
13. *(note)* Generation/geometry retention now stream-scoped → stated, bounded by the deadline
    (§5).

What the review attacked and could not break, recorded so it is not re-litigated: the
delivered-prefix soundness claim (stronger than stated — whole-tile frames make every
delivered tile an *exact* band), the determinism claim, head-before-sweep, the emit-phase
error class, the engine seam's lifetimes, and the tile-order change's safety
(`tile_ranges_all` verified order-independent at the interface).

**Implementation review (2026-08-11).** A second independent adversarial review, of the built
code against this document. Verdict: faithful at every load-bearing point; the
handler/producer/body triangle survived every interleaving the reviewer could construct; no
finding above minor. Six findings, all accepted and fixed the same day:

1. *(minor)* The emit loop could flush an empty points chunk (`k = 0` — a legal counts-only
   request — with a Utf8 declared scalar, whose offset table is 4 estimate bytes at zero rows)
   → `!buf.is_empty()` guard, pinned by `a_zero_k_request_streams_counts_and_no_points_chunks`.
2. *(minor)* An unbounded flush threshold could accumulate a frame past the wire's `u32` length
   (a panic there) → the emit pass caps frames at `MAX_POINTS_FRAME_BYTES` (1 GiB) regardless
   of threshold.
3. *(minor)* A stale "already sorted" comment survived at a range-derivation call site →
   corrected to the r26 order contract.
4. *(note)* The dev-CORS expose list still named the retired stage header and omitted the
   delta-serving coordinates → `etag`/`x-tessera-identity-key`/`x-tessera-stale` exposed,
   `x-tessera-stage-ns` removed and pinned absent.
5. *(note)* The three independent readers enforced the frame grammar unevenly → all three now
   refuse a second tiles frame and a mispositioned sub-cells frame identically.
6. *(note)* `StreamBody` was not fused past its abort error → fused.

The review also named the dark paths; the stall-shed, mid-stream-disconnect and `streaming`
gauge behaviours are now covered end to end by
`a_stalled_or_disconnected_stream_is_shed_and_the_gauge_returns_to_zero` (which measured its
own first premise: a ~5 MB response "streams" whole into loopback socket buffers without the
producer ever parking, so the fixture is sized to ~32 MB). Producer-panic and mid-body
engine-fault injection remain untested, accepted as such.
