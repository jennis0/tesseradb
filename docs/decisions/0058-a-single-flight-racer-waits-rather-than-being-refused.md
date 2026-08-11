# 0058 — A single-flight racer waits for the build rather than being refused

**Date:** 2026-08-09 · **Status:** Settled (owner ruling) · **Mechanism: not built** — the message
was corrected in `dbb3477`; the behaviour this rules on is the remaining work

## Context

`SingleFlightCache::get_or_derive` refuses a caller that finds a key already being built:

```rust
Some(Slot::Building { .. }) => {
    self.building_refusals.fetch_add(1, Ordering::Relaxed);
    return Err(Building);
}
```

That reaches the client as `429 backpressure, Retry-After: 1`. Found when the viewer's look-ahead
gave it two concurrent viewport requests for the first time: three concurrent requests on a cold
session return 429, 429, 200 while `/control/status` reports the compute gate holding 48 free
permits and a `shed_total` of 0. The gate sheds nothing. What refuses is the row-projection cache,
because two of one client's own requests wanted the same key at the same moment.

Commit `dbb3477` gave that refusal its own error variant, so the body no longer blames the compute
gate. **This decision is about the refusal itself, which that commit did not touch.**

## The decision

**A caller that finds `Slot::Building` waits for that build and is served its result**, rather than
being refused. Three conditions:

- **Bounded.** A waiter that is not satisfied within a timeout still receives
  `ApiError::SingleFlightBackpressure`. The variant survives as the timeout path.
- **Cancellable.** A waiter observes the request's existing `CancelToken`, so a disconnected client
  releases immediately rather than holding a slot to the timeout.
- **Decision [0044](0044-invisible-means-stale-serve-plus-background-refresh.md)'s merge-window
  residual is unchanged.** A racer inside a merge's span-rebase window is still shed.

## Why

**It is not backpressure, and calling it that hid the defect for as long as it did.** Backpressure
means the server is saturated and the work should not happen now. Here the server is idle and the
work is happening anyway — the winner is building it. Refusing sheds no load; it makes a second
client redo the same wait later.

**The client's retry budget is shorter than the build it is waiting for, and that is the failure.**
`Retry-After` is a fixed `1` second; the viewer retries twice with 1 s then 2 s of backoff and then
shows `refused`. A full row-projection build at 10⁹ is measured at **4,550 ms**
(`crates/tessera-engine/src/refresh.rs`). The client therefore exhausts its retries *before the
server finishes work that was always going to succeed*, and the user gets a blank map with an error
— caused by a race between two of their own requests, on a server doing nothing heavy. On a small
corpus the build is milliseconds, a retry lands, and nobody notices; this is a defect that gets
worse exactly as the corpus reaches the scale the system is for.

**0044 never ruled on this path.** It sanctioned *"a bounded 429 residual **only** for same-key
racers during a merge's refresh window"*, and its argument was specific: stale-serve is unsound
across a merge because the merged span's row ids change meaning (I11), so a racer must not be served
the stale entry. It then placed the case at issue here outside its own scope — *"full builds happen
only at session establishment — not update-induced"*. The cold-build racer inherited the merge
window's treatment without the merge window's argument, and that argument does not transfer: there
is nothing stale to serve, only a build to wait for.

**Waiters cannot accumulate.** The compute gate already bounds concurrency at 48 in flight plus 96
queued, so the number of parked waiters is bounded by the gate rather than by anything new.

**F4's property is preserved.** The reason the map lock is held only for the O(1)
`Building`/`Ready` transition is that *distinct* sessions' first viewports must not serialise behind
one global lock — measured, and pinned by
`distinct_key_first_viewports_overlap_instead_of_serialising`. A per-slot notification does not
reintroduce a global lock, and same-key callers serialising is the entire point of single-flight.

## What was rejected

**Raising `Retry-After`, or the client's retry count.** It asks the client to guess an interval the
server already knows, and leaves the shape — refuse rather than deduplicate — in place. Every
consumer would have to be told the guess.

**Requiring clients to serialise their own requests.** That pushes an engine defect onto every
client, and the tile-addressed route (issue #7) is inherently parallel — one request per tile
is its whole access pattern, so it would meet this on every cold session.

**Leaving it, on the ground that one request at a time never sees it.** True of the viewer until
look-ahead, and false of every client the design intends to support.

## Evidence

- Reproduction and its non-cause: `a_single_flight_shed_does_not_blame_the_compute_gate`
  (`crates/tessera-server/tests/http.rs`) — concurrent viewports on a cold session produce the 429
  while the gate's `shed_total` stays 0.
- Build cost at 10⁹: `refresh.rs`'s table, 4,550 ms for a full projection rebuild.
- Client retry budget: `clients/ts/viewer/src/viewportLayer.ts`, `MAX_RETRIES = 2` with `1000 * 2 **
  attempt` backoff.
- The refusals are already counted — `building_refusals`, on `/control/status` for both the
  projection and fragment caches — which is how the mechanism was identified rather than guessed.

## Consequences

- `crates/tessera-engine/src/single_flight.rs` gains a waiter path; `Err(Building)` becomes the
  timeout answer rather than the immediate one.
- **The wedge hazard changes shape and must not be lost.** That module records that a builder
  panicking between claiming a slot and filling it leaves the key permanently `Building`. Today
  that is an endless stream of refusals; with waiters it becomes a stall, and the timeout is what
  bounds it. It is the reason the timeout is a condition of this ruling and not a tuning knob.
- `building_refusals` keeps its meaning but changes population: it counts timeouts, not races.
  Anything reading it as a race rate needs a second counter for waits satisfied.
- The timeout's **value** is left open, to be argued from the build cost it must exceed rather than
  inherited from the gate's fixed `1`.
