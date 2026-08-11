# The viewer's render/fetch pipeline: cost model, floors, and what "done" is

**Status:** Evidence. Measured 2026-08-10 against `data/bench-fixtures/1e9` (10⁹ items, no
declared columns), full and heavy principals, local socket, Firefox on WSL2. Trace files named
below are in the repository root; figures are from `?trace=1` instrumentation unless marked
modelled or assumed.

## Result

The pipeline meets the frame targets at 10⁹ — latest capture 47.3 fps average, in-frame p95
~20 ms — and the server is nowhere near being the limit: in a 26 s interaction window every
foreground arrival was answered with zero novel bytes and the session's server share was
20–63 ms p50 against 400–780 ms pan-to-paint. **The binding constraint is the client's own
pipeline, and within it the first-paint path on novel ground: request → first absorbed piece
measured p50 347 ms, p95 9.6 s** (`T21-31-32`). That p50 is close to its floor under the current
protocol (§3); the p95 tail and the perceived pop-in are where the remaining leverage is.

The day's fixes, each verified by the following trace:

| change | before → after |
|---|---|
| batched GPU flush (was one `buffer.write` per band) | slab sync max 790 ms → 180 ms |
| owned picking-colour buffer | deck main-thread cost 252 → ~42 ms/s at 1.7 × 10⁶ marks |
| per-tile density matching (was per-band, floor 1/band) | zoom-out patches up to ~200× overdrawn → bounded by tile target |
| fold-time stand-in filter | 12,095 tiles flashing to 2× per arrival (`T21-26-09`) → zero density transitions >2× (`T21-31-32`) |
| pipelined centre-first pieces (was strictly serial) | untested by trace yet; removes Σ(wire+decode) serialisation |

Two of the five (the batched flush, the per-tile matching) repaired regressions introduced by
the *previous* day's fixes — evidence that the fix loop was running ahead of the model. This
memo is the model.

## 1. The stages, and what each measured

A foreground view change onto novel ground passes through, in order:

| stage | thread | measured (1e9) | scales with |
|---|---|---|---|
| wait (debounce / leading edge) | — | p50 0, p95 115–175 ms | client policy |
| wire + server | — | p50 20–63 ms, p95 ~140 ms | bytes; server: tiles × coverage |
| decode | worker (2 fg lanes) | **not traced separately** | points (~1.2 µs/pt, modelled from the 2.8 s / 2.3 × 10⁶ pt measurement) |
| split + absorb | main, 6 ms slices | ~64 ms per response, max slice 58 ms | points + bands |
| derive / refresh fold | main | derive p50 22–28 ms; refresh p50 6–22 ms | **bands** (~10⁵/frame) |
| slab sync + GPU flush | main | p50 16–20 ms | bands + appended marks |
| layers + deck + paint | main | painted p50 14–32 ms | marks drawn (10⁶) |

Instrumentation gaps found while building this table, worth closing before further tuning:
response bytes report 0 under piece splitting (only the last piece's response is kept); decode
time is invisible (inside `arrived` but not split out); and the density audit deliberately skips
tiles that appear from nothing — which is exactly the transition today's pop-in is made of.

## 2. What the traces rule out

- **Server capacity.** All-cache-answered windows still hitched; the limit is client-side.
- **GPU raster.** The Firefox profile showed the renderer idle; SwiftShader/native both paint
  10⁶ marks without being the frame budget.
- **Upload volume.** With owned buffers and span writes, deck attribute updates are no longer a
  measurable share.
- **Stand-in density error.** The audit's provisional→exact median ratio is ~1.0 after the
  fold filter: what pops in now is not a density correction.

## 3. The floor for pan-to-paint on novel ground

Modelled from the stage table, first paint = wire+server (~30–100 ms local) + first piece's
decode (~100–200 ms at current piece sizes) + one absorb slice (~6–20 ms) + fold + paint
(~40–60 ms) ≈ **200–350 ms** — which is where the measured p50 (347 ms) already sits. The
client can still buy at the margins (a deliberately small first piece; faster Arrow decode),
but **halving pan-to-paint on novel ground is not reachable by client tuning under the current
protocol.** The moves that reach it are design work, not patches:

1. **Anticipation hit rate** — a miss *is* novel ground. The ring exists and roughly doubles
   the no-request pan share (measured 42–51% → ~92% in the look-ahead probe); its budget and
   prediction are the cheapest lever on how often the floor is paid at all.
2. **Coarse-first serving** — a first response pass at low `k` (or a server-chosen budget form,
   §8.6's annotation) so *something* correct paints at wire+decode-of-little, refined by the
   full answer. Protocol design; belongs against `delta-serving.md`, not in the viewer.
3. **First-session materialisation** — the 10 s first-viewport wait at broad principals is the
   S1/S2 owner decision priced in `2026-08-10-1e9-session-materialisation.md`; no client change
   touches it.

The p95 tail (1.7–9.6 s arrivals) is a separate, client-side item: absorb slices yield to the
frame loop and starve under continuous gesturing, and until this morning pieces were fetched
serially. The pipelining change addresses the serialisation; whether the starvation remains is
the first thing the next trace should answer. NOT yet measured post-change.

## 4. Mechanism ledger

What this branch added to the client, and why each is kept. The test for each was "does the
next trace show it paying": all currently do, but the *set* should be re-examined whenever one
of them changes — several pairs interact.

- **Request pipelining + queued view** (viewportLayer) — completions 7/62 → 15/16.
- **Piece splitting, now centre-first and pipelined** (replica) — bounds per-response decode.
- **Two foreground decode lanes + background lane** (decoder) — overlap exists only because of
  the previous item; if piece splitting changes, revisit the lane count.
- **Piece-by-piece painting via the absorb hook** (main/viewportLayer) — first paint no longer
  waits for the last piece.
- **Settle pass + `standInStale`** (viewportLayer/assemble) — full stand-in derivation off the
  arrival path. Interacts with the fold filter: the filter removes the *double-draw* the settle
  used to be the only cure for, so the settle's remaining job is extent, not density.
- **Depth hysteresis (`holdDepth`)** (prefetch/budget) — kills per-frame depth flapping;
  releases at settle. Watch for at-rest 8/9 alternation in derive sequences.
- **Per-tile density matching + fold-time filter** (assemble) — the two halves of "a stand-in
  patch must read as the drawn depth": size it right, and remove it when exact ground arrives.
  Note they operate at different granularity from the derive-time coverage clip (rects); a
  future consolidation could unify all three on the tile grid.
- **Owned GPU buffers: positions, colours, picking; span flush** (slab/viewportLayer) — deck
  binds, never copies. `?gpu=0` reverts wholesale; that knob is the debugging story and must
  survive any refactor.
- **Density audit** (viewportLayer, `?trace=1` only) — the instrument that found the
  double-draw. Extend with appear-from-empty counting if pop-in work continues.

## 5. What "done" means

Agreed targets: **average fps > 45 and p95 frametime < 100 ms during interaction** — both met
in the latest capture. Proposed additions, so pop-in has a number and the loop has an exit:

- request → first absorbed piece on novel ground: **p50 ≤ 350 ms** (the protocol floor),
  **p95 ≤ 1 s** (no starvation tail);
- density audit clean in steady interaction: no >2× tile transitions, provisional→exact
  median within 100 ± 25;
- anything beyond that filed against the three design items in §3, not patched in the viewer.
