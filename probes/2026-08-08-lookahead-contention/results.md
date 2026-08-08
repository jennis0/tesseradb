# Look-ahead: latency won, CPU spent, and what happens as clients are added

**Date:** 2026-08-08
**Corpus:** the 2.4M demo bundle (`run_demo.sh`), lean five-column schema, depth 10, `k = 500`,
broad principal (term 4). Warm row-projection cache throughout.
**Driver:** `load.mjs` (synthetic, no browser) and `clients/ts/viewer/smoke-latency.mjs` (browser).

## The result

Look-ahead roughly doubles the fraction of pans that need no request, costs about half again as
much server CPU per pan, and does not degrade server-side latency at up to 16 concurrent clients.

| clients | arm | pans needing no request | server CPU per pan | foreground p50 (server) | p95 (server) |
|---|---|---|---|---|---|
| 1 | off | 5 / 12 | 1.61 ms | 2 ms | 7 ms |
| 1 | **on** | **11 / 12** | 1.73 ms | 7 ms | 7 ms |
| 4 | off | 25 / 48 | 1.37 ms | 2 ms | 11 ms |
| 4 | **on** | **44 / 48** | 2.67 ms | 7 ms | 9 ms |
| 16 | off | 98 / 192 | 2.13 ms | 1 ms | 27 ms |
| 16 | **on** | **176 / 192** | 2.58 ms | 7 ms | 25 ms |

No request was shed at any client count. Server-side per-request latency is flat in the client
count and in the arm: the engine is not the bottleneck here, and anticipation does not make it one.

## What the browser measured

`smoke-latency.mjs`, one client, 420 px pans at three speeds, timing mouse-up to the paint that
answers it:

| pan speed | off: free | off p50 wait | on: free | on p50 wait |
|---|---|---|---|---|
| 300 px/s | 4 / 4 | — | 4 / 4 | — |
| 1000 px/s | 3 / 4 | 1014 ms | **4 / 4** | — |
| 2500 px/s | 3 / 4 | 2104 ms | **4 / 4** | — |

**Slow pans do not need look-ahead, and the reason is the debounce rather than the distance.** A
420 px drag at 300 px/s lasts 1.4 s, so it outlives the 140 ms debounce several times over and is
answered *during* the drag; by mouse-up the view is already correct. The same drag at 2500 px/s
takes 168 ms, arrives as one event, and must be answered afterwards. So look-ahead earns its keep
at high movement speed, which is the opposite of the intuition that a fast pan outruns a fixed
ring.

**What bounds it is the pause, not the speed.** The ring is bought only when the view has been
still for `IDLE_MS` (250 ms) and only when it has moved a quarter of a viewport since the last one.
A continuous drag re-arms that timer on every frame and so never triggers it — deliberately. Look-
ahead therefore helps a user who moves in bursts with pauses between, and does nothing at all for
one who drags continuously. The binding constraint is whether a ring fetch (4–90 ms here) fits in
the pause, not whether the ring is geometrically large enough.

## The cost is speculation, and it does not fall with predictability

The premium is roughly flat while the no-look-ahead arm gets *cheaper* as the user revisits ground,
so the relative cost grows with repetitive behaviour:

| chance of reversing per pan | off ms/pan | on ms/pan | premium |
|---|---|---|---|
| 0% (straight traverse) | 1.80 | 2.66 | +48% |
| 25% | 1.36 | 2.23 | +64% |
| 50% | 1.23 | 2.10 | +71% |

The expectation was the reverse — that a predictable user would make anticipation nearly free and
an unpredictable one would waste it. Two things defeat that. The replica already makes a revisit
free without any anticipation, so turning is exactly where the *off* arm gets cheap. And a ring is
a symmetric box: it fetches ahead in every direction at once, so its cost does not depend on
guessing right. The viewer biases the ring downwind of recent movement, which this driver does not
model — that bias is the thing that would make the premium depend on prediction, and its value is
therefore unmeasured.

## Two instruments that did not work, and why

**Driving N headless browsers does not measure the server.** Three software-rendered deck.gl
canvases at ~380 × 10³ marks saturated the box and produced no output in twenty minutes. What it
measures is rasterisation, long before it measures the engine.

**Wall-clock latency from a single-process driver is not server latency either.** `load.mjs`
multiplexes every simulated client through one connection pool, so fire-and-forget ring requests
queue ahead of a foreground one in a way separate browsers never would: at 16 clients the driver's
wall p50 reads 1116 ms while the server's own figure for the same requests is 7 ms. The table above
reports both, apart, for that reason. JSON serialisation was ruled out as the cause (2 ms for a
47 × 10³-tile ring, 19 ms for sixteen of them).

## Limits

- **One box, 2.4M items, warm cache.** At 10⁹ a cold session pays a 4,550 ms projection build,
  which dominates everything here.
- **The premium is noisy** — +8% at one client, +95% at four, +21% at sixteen, on twelve pans per
  client. The direction is consistent; the magnitude is not pinned.
- **The synthetic ring is symmetric.** It is a worst case for cost and a best case for coverage
  relative to the viewer's velocity-biased one.
- **Nothing here reaches `caching.md` §3's operating point.** At 1–2 × 10⁶ marks the client's own
  tile handling — 181 ms to enumerate a 262 × 10³-tile ring, 56 ms to plan it — exceeds a frame
  budget before any server cost applies.
