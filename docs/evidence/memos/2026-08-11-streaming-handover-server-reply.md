# Streaming the viewport response: server-side assessment, and the design that answers it

**Status:** Design reply to `2026-08-11-streaming-handover-client-case.md`. Investigated against
the engine and server as they stand on `client/replica-cache`; figures marked *measured* come
from that memo's traces or existing bench evidence, everything else is modelled and says so.
Direction settled with the owner 2026-08-11: **tile-major streaming (the client memo's
alternative 4), not global id-major**; banded emission held in reserve. Nothing here is built.

## Result

Streaming is feasible and suitable, and the engine is already shaped for it:

- Per-tile selections are already **ascending by `tessera_id`** (`select::Selection`, and
  contracts §3.2 r7 already states it) — so every cut of a tile-major stream leaves each tile
  holding an id-order prefix of `served(T)`, which is exactly what `delta-serving.md` §7 already
  licenses a client to draw. The prefix-validity argument the client memo rests on holds against
  the code, not just the spec.
- Selection and gather are separate functions fused only by the per-tile loop
  (`viewport::tile_result`); the phase split streaming needs is a restructuring of the fold, not
  of selection, masking or counts.
- Every response coordinate the client needs up front — `identity_key`, `content_key`, stamp,
  `stale` — is minted **before** the tile sweep runs, so all of them stay HTTP headers.

No invariant is touched. I7/I2/counts-exactness are unaffected by construction — the stream
reorders bytes the response already contained. I13a needs an **annotation at the claim**, not a
change (§5).

**The global id-major merge (the client's preferred ordering) is declined**, on grounds beyond
its ~30–80 ms modelled CPU cost (§3). Tile-major keeps every structural win — the monolith and
its hitches die, counts arrive first, cancellation takes effect mid-stream — and loses only
spatially-uniform densification, the trade the client memo itself marked acceptable.

## 1. The construction (v1)

Two phases, split at the seam the code already has:

1. **The sweep** — generation load → session geometry → compose → θ anchor → tile ranges →
   the existing parallel `tile_result` fan-out, with the **gather removed**. Returns per-tile
   counts, sub-cells and selections. All calibration machinery (`SERIAL_FALLBACK_MAX_ROWS`,
   `TILE_PAR_MIN_TILES`, the indexed-collect ordering argument) survives untouched.
   **First flush at sweep completion:** the tile batch — `visible`/`matched`/`served`, `served`
   known without any gather — plus the §3.3 sub-cells. Criterion 1 satisfied exactly.
2. **Emit** — gather + serialise per tile, in emission order, into size-based flushes of
   ~1–2 MB. Each flush is a self-contained, length-prefixed Arrow IPC stream (§8.6(2)'s
   owner-annotated framing item, delivered as a side effect). A trailer frame carries the
   stage timings and totals (criterion 7); `x-tessera-stage-ns` as a header cannot survive a
   streamed body, and its bench consumers move with it.

**Emission order = request order.** The client's `tiles` list is emitted in the order given
(deduplicated, no longer re-sorted for presentation), so the replica client's centre-first
machinery collapses to ordering its own request list — one line at request construction. The
engine still sorts internally for the monotone range-derivation sweep and emits through an index
permutation; bbox requests emit in derivation order as today.

**Flush boundaries are size-based only.** The client memo's ~100 ms timer leg is dropped:
after the sweep, byte production is memcpy-speed, so the timer buys nothing — and it would cost
byte-determinism, which the serial/parallel byte-equality tests and the conformance comparator
depend on. Emission is deterministic per build.

**The order contract is written one notch looser than the implementation** (contracts §3.2
edit): each tile's points arrive ascending by `tessera_id`, each flush is self-contained, and a
client **must not assume any cross-tile grouping or ordering** beyond what the tile batch
declares. v1 in fact delivers today's grouping (tile-major, tiles in tile-batch order) — the
looseness exists so a later emission-order change (§2) is not a wire break.

## 2. Why not id-major, and what is held in reserve

The merge's modelled cost is ~30–80 ms + ~19 MB transient at the op point (~36k tiles, ~1.2M
points) — significant against 25–63 ms p50 *measured* server time. But the structural problems
land worse than the CPU:

- **The cost sits ahead of the first point flush.** `tessera_id` is a keyed permutation, so the
  globally smallest ids are spread uniformly across all tiles: an id-major first flush needs the
  head of *every* tile's gather. Either the full gather runs up front (first mark moves at
  roughly today's whole server time) or the gather goes id-major in chunks — random access
  across the whole viewport's row span, against the measured cost model's contiguity rule.
- **Abandonment inverts the economics.** The pan-heavy session aborts most streams. Id-major
  pays merge (and in its safe variant the whole gather) before the first point flush, so a
  stream cancelled at 25% has paid ~100% of the compute. Tile-major pays as it emits:
  unemitted = ungathered = unpaid.

**Held in reserve — banded emission ("v2"), noted, not planned.** B passes over fixed
id-value thresholds (each tile's band-*b* points found by binary search in its already-sorted
selection) approximate uniform densification in B steps, and every cut remains a per-tile
id-prefix (held = `served(T) ∩ id < threshold`, a prefix of a smallest-by-id set). Deferred on
locality grounds: B passes turn one contiguous per-tile gather into B strided passes touching
each tile's ranges B times — cutting against exactly the contiguous-read wins the index is built
around. It becomes worth pricing only if tile-shaped pop-in on the big novel-ground arrivals is
*measured* to matter after v1 ships; the order-contract looseness above means landing it later
touches only the emission loop.

## 3. The client memo's experiments, dispositioned

- **E1 (merge cost):** answered by declination — modelled figures above; not built, not worth a
  campaign unless banded emission is someday judged insufficient.
- **E2 (stream during select?):** select-then-stream, necessarily and at no cost — criterion 1
  (all counts before points) requires the full sweep, and `served` needs `C_θ` from the same
  scan. Time-to-first-flush = sweep time, measurable **today** from the existing
  `x-tessera-stage-ns` split (`count_ns + select_ns` vs `gather_ns + arrow_serialise_ns`); no
  prototype needed.
- **E3 (cadence sweep):** narrowed to size-based thresholds; sweep 1 vs 2 MB client-side once
  v1 exists. The timer leg is rejected for determinism (§1).
- **E4 (tile-major):** is the design.
- **E5 (abandonment):** the sweep keeps its per-tile cancellation checkpoints; the emit phase
  observes disconnect as channel closure at the next flush. A 25%-cancelled stream spends the
  sweep plus ~25% of gather/serialise — strictly better than today's all-or-nothing, and
  tile-major is the optimum among the orderings considered.

## 4. Batch cases

One verb, one framing, two sinks — no `?stream=0` monolith mode (two wire framings is two
conformance surfaces forever, against the narrow-surface rule):

- **Engine:** `Engine::viewport` → `ViewportOut` stays, as a collector over the same streaming
  producer. Its batch consumers (bench arms, `viewport_sweep`, tests, the oracle differential
  via the server) are untouched.
- **Wire:** a sequence of length-prefixed self-contained IPC streams is losslessly collectable —
  batch readers read to the end and concatenate; the SDK grows a collect helper.
- **The tile-addressed adapter** (`tile-addressed-integration.md`, S5) consumes the engine API
  and its own cache; its "points-only single Arrow stream" alias is its own emission and is
  unaffected.
- Small requests (anticipation ring, depth shadows — criterion 8) degenerate to one flush plus
  trailer, a few hundred bytes of framing overhead. Uniform across request kinds.

## 5. Costs, risks, and what needs a ruling

- **Engine restructure** — the largest piece: gather out of `tile_result`, streaming fold,
  batch collector. Calibration machinery unaffected.
- **Wire + server** — new framing module; handler → streaming body; the cancel guard moves into
  the body stream (drop-on-disconnect works at least as directly as today's handler-future
  drop).
- **200-then-error** — new failure class: an engine error after the first flush cannot change
  the status. Fail closed: abort the connection with no trailer; a missing trailer marks the
  response incomplete, and every delivered prefix remains sound (same snapshot, exact counts,
  drawable under delta-serving §7). Conformance grows truncation coverage.
- **Permit lifecycle** (criterion 6) — **needs an owner ruling** (decision 0059's
  neighbourhood). Recommended: hold both `GatePermits` through the sweep; release the compute
  permit at emit start (the gate's own doc already treats the serialise phase as
  oversubscribable); keep the slot permit to stream end; bounded channel (~2 flushes) plus a
  write-stall deadline so a slow reader is shed rather than parked — the stated backpressure
  answer the client memo asks for.
- **I13a** — annotate at the claim (decision 0013's form): the invariant's text governs shared
  work observed by *other* requests and is unviolated; `viewport.rs`'s "no partial
  `ViewportOut`" posture gains a carve-out for the requesting client's own truncated stream,
  which is client-detectable and prefix-valid.
- **Contract churn**, all in-repo and free only while 0048 holds — the client memo's argument
  for deciding now is right: contracts §3.2 (framing, tile order = request order, the
  order-independence clause), `reference/oracle/wire.py`, conformance canonicalisation, the TS
  decoder, bench stage-header consumers.
