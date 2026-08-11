# 0059 — Per-principal admission is not capped; the wait budget bounds it instead

**Date:** 2026-08-09 · **Status:** Settled (owner ruling)

## Context

Decision [0058](0058-a-single-flight-racer-waits-rather-than-being-refused.md) turns a same-key
racer from an immediate 429 into a bounded wait. A waiter blocks inside the engine, and the request
holds its `ComputeGate` permits across the whole `spawn_blocking` closure, so waiting converts a
slot that was released immediately into one held for as long as the build. The question this raises:
should a principal be capped at some share of the gate, so that one client — accidentally or
otherwise — cannot occupy it.

The gate is process-wide and unpartitioned. `ComputeGate::admit` acquires an outer *slots* permit
(`compute_admission + compute_queue`, 48 + 96 by default) non-blocking, then an inner *compute*
permit under `admission_timeout_ms`. Nothing keys on the caller. The session is resolved before
admission, so a cap is mechanically available — this is a policy question, not a plumbing one.

## The decision

**No per-principal cap.** 0058 lands with two bounds instead:

- **The wait budget is configurable** — `serve.single_flight_wait_ms` — so an operator can bound
  worst-case slot occupancy without a rebuild, rather than living with a constant argued from one
  corpus scale.
- **Parked waiters are counted** — `waiters_now` and `waits_satisfied` on `/control/status`,
  alongside the existing `building_refusals`, which 0058 repopulates with timeouts.

**Revisited only on measurement**, and if it is revisited the shape is load-adaptive rather than a
fixed per-principal ceiling. A quota on *sessions* belongs on the authorise plane, not on the
compute gate.

## Why

**The exposure is pre-existing, and waiting is the cheaper half of it.** A principal opening 48
sessions on 48 distinct cold keys already holds every compute permit for a measured 4,550 ms each,
doing real CPU throughout. That is reachable today, it is worse than anything waiting enables, and
the gate carries no per-principal partition by design (SA §9 keeps per-auth-hash labels off shared
dashboards; SA D2 puts session minting on its own plane precisely because it is the expensive
surface). A cap introduced for the waiter path would leave the larger case untouched.

**The admitted ceiling does not move.** Stage 1 is a non-blocking `try_acquire` over 144 permits, so
a client firing 100 requests is shed past that point whatever the engine does underneath. What
0058 changes is the *occupancy window* — from 4,550 ms, builder-bound, to the wait budget — and the
*amplification*: one build's CPU can now hold several slots idle rather than one. Amplification of
CPU is the wrong thing to defend here, because CPU was never the binding cost of saturating this
gate. Issuing 144 requests is.

**0058 closes the accidental case a cap would be aimed at.** The reason concurrent viewports on one
cold session occupy several slots today is that each independently claims, is refused, and retries.
With waiting they resolve off a single build, and once it publishes they are on the steady-state
`peek` path, which claims no slot at all. The occupancy becomes a one-off at session establishment,
bounded by the budget — not a standing condition. Adding a cap now would be mechanism against a
shape this change removes.

**A fixed cap binds when the server is idle and not when it is loaded.** Cap a principal at eight
and a legitimately panning viewer is refused its ninth tile request against a 90 %-idle server —
a worse failure than the blank map 0058 exists to fix, and harder to diagnose because it is
independent of load. The only version worth having engages above some gate occupancy, which is a
fairness design with its own contract, its own client-visible behaviour, and its own review. It is
not a rider on a concurrency fix.

**The counters that would enforce a cap are not counters that may be published.** A per-principal
ceiling needs per-auth-hash state, and SA §9 rules that per-auth-hash labels stay off shared
dashboards — shed counts are unmasked corpus quantities. Such a cap would therefore be enforceable
but not observable through the operational surface, and the natural debugging move (expose the
per-principal gauge) is the disclosure. Recorded here because it is the first thing whoever builds
this will get wrong.

**Stated honestly: the gauge shipped here does not attribute.** `waiters_now` is process-wide. It
answers whether waiting occupies the gate at all, which is the precondition for the question; it
does **not** show that one principal holds a large share, and reading it that way would be wrong.
Attribution needs an investigation with per-principal instrumentation on the admin plane, and that
investigation is the trigger for revisiting this — not the gauge on its own.

## What was rejected

**A fixed per-principal slot cap now.** Above: it binds at the wrong times, it misses the larger
pre-existing case, and it sets a client-visible entitlement that is the owner's to define rather
than an implementer's to pick.

**A per-key waiter cap.** 0058 already declined one on the ground that the gate is the bound, and
that reasoning survives: a per-key cap limits only the cheap variant of the occupancy while the
expensive one — distinct keys, real builds — is unaffected. It would also refuse legitimate racers
on exactly the key they are all waiting for, which is the defect.

**Releasing the compute permit while parked** (wait in the server, re-enter the gate). It removes
the occupancy entirely and keeps the engine synchronous, so it is the strongest alternative. It is
rejected because the re-entering request must re-win admission, so under precisely the load where
occupancy matters it is shed at stage 1 and the original defect returns; fixing that needs
head-of-queue re-entry, which is more mechanism than a condvar. It also polls rather than being
notified, adding a tick of latency to the common case. Recorded rather than dismissed: if the
waiter gauge ever shows sustained occupancy, this is the design to reopen, not the cap.

## Consequences

- `serve.single_flight_wait_ms` joins the serve config, defaulting to a value argued from the
  measured build cost it must outlast (6,000 ms against 4,550 ms at 10⁹).
- `/control/status`'s projection-cache block gains `waiters_now` and `waits_satisfied`;
  `building_refusals` keeps its name and becomes the timeout count, per 0058.
- No per-principal state is introduced anywhere on the request path.
- The compute gate's `shed_total` will move under cold-start load that previously produced
  single-flight 429s instead. That is a shift between two counters, not new shedding, and both
  docs say so.
