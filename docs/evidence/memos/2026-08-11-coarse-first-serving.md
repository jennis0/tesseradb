# Coarse-first serving: the pop-in the client cannot fix, priced for a ruling

**Status:** Evidence + escalation. Measured 2026-08-10/11 on the 1e9 fixture; the client-side
figures are from the post-driver traces (`client-architecture.md` Appendix M).

## The case

Pan-to-first-paint on novel ground sits at its protocol floor: ~350 ms p50, decomposed as
wire+server (30–100 ms) + first-piece decode (100–200 ms) + absorb slice + fold + paint. Every
client-side lever on that path has now been pulled — pipelined centre-first pieces, two decode
lanes, piece-by-piece painting, a working anticipation ring (93% of pans need no request). What
remains is structural: **when a user explores genuinely novel ground, the first correct pixels
cannot arrive before a full-size response is fetched and decoded.** Deep continuous panning at
10⁹ is wall-to-wall novel ground — measured foreground requests of ~36k tiles at ~99% novelty,
arriving 500–800 ms after the gesture, felt as squares popping in behind the scroll.

The 10 s first-viewport materialisation is a separate server item (S1/S2, already escalated).

## The shape

A viewport answer whose **first bytes are a coarse, correct, complete-in-itself pass** — the
same masked selection at a shallower depth or smaller per-tile budget — followed by the full
answer. The client paints the coarse pass at wire+decode-of-little (modelled ~50–150 ms on the
measured stages), then refines exactly as stand-ins refine today: the coarse pass IS a served
band set, so §7.2's prefix nesting makes the refinement well-defined and every mark of the first
paint is a mark the server chose to serve — no invariant is touched.

Three candidate encodings, in rising order of protocol change:

1. **Client-issued pair** — a low-`k` (or shallow-depth) request ahead of the full one. No wire
   change; costs a second selection pass server-side and a second round trip; the two requests
   race arrivals the client must order. Buildable today; ugliest.
2. **Server-streamed pair** — one request, one response whose tile stream carries a coarse
   prelude before the full batch. One selection can serve both (the prelude is a prefix cut of
   the same selection at coarse granularity); needs a framing addition and a client that decodes
   incrementally (the piece machinery already absorbs incrementally).
3. **The §8.6 budget form** (`{bbox, budget}`, server chooses depth and allocation) with a
   progressive contract — the owner-approved direction, of which a coarse prelude is the natural
   first instalment.

## What it costs

Server: the prelude's selection is a subset of work already done under (2)/(3) — near-zero
marginal select cost; wire adds one small batch (~1/16 of the full answer at Δ=2 granularity).
Client: incremental decode of a two-part stream (the worker and absorb path already handle
pieces). The real cost is protocol surface before the api_version window closes — which is the
reason to rule on the *shape* now even if the build waits.

## Decision asked

Whether coarse-first enters the protocol as (2) a streamed prelude, (3) part of the budget
form, or is declined in favour of accepting the ~350 ms floor. NOT asked: any client-side
workaround — (1) is recorded as considered and worth avoiding.
