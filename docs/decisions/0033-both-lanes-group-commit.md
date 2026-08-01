# 0033 — Both write lanes group-commit, in separate windows

**Date:** 2026-08-01 · **Status:** Settled

## Context

The write path has two lanes. Ingest is bounded and may be shed under load; deny dispositions —
delete, suppress, unsuppress, predicate change — are never refused for load, because refusing a
security operation is fail-open. The deny lane is a separate unbounded queue drained to empty
before any ingest work, which is what makes "a deny is never queued behind work of unbounded
duration" structurally true rather than a scheduling intention.

Ingest acquired a commit window: many submissions are allocated in one signature-sorted run,
appended, fsynced once, applied in one generation swap, and then every waiter is acknowledged.
The deny lane did not, and the cost was severe and invisible. One `/control/changes` request
carrying N dispositions cost N sequential fsyncs, measured at roughly 310 denies per second — so a
million-item revocation would have taken about fifty-three minutes.

## Decision

**Both lanes group-commit, in their own windows.** Denies do not join the ingest window.

The deny window is a second, independent window on the priority lane. It keeps everything that
makes that lane what it is: its own unbounded queue, drained before ingest, unable to answer 429.

## Why denies do not share the ingest window

The specification permits it; sharing was measured against and declined.

The two properties a shared window was meant to deliver already hold, by stronger mechanisms.
**Acknowledgement is coupled to application** by the type system — a successful receipt cannot be
constructed without a token minted only at the generation swap, at two sites a build rule pins.
**Starvation is bounded** by the drain order, and the bound is asserted by three tests.

What sharing would have bought is one fsync for the deny alone while ingest saves nothing. What it
would have cost is a per-entry durability fold over operations the specification scopes
differently, an ordering hazard the separate lanes do not have, and an entry type that stops saying
what is true.

## What the cost actually was

The dominant term was the handler, not the lane. It awaited each disposition's receipt before
submitting the next, so the deny queue never held more than one item — and a window keyed on
draining that queue would have batched nothing at all. Submission and waiting are now separate
operations, so a request enqueues a chunk before collecting any of it, and the executor commits the
chunk as k appends, one fsync, one overlay clone, one swap and k acknowledgements.

Two further costs surfaced only once the fsyncs were amortised: applying a change cloned the whole
overlay per item, making a batch quadratic in its own size; and per-item external-ID resolution
became the dominant remaining term, so it uses the batched resolution the ingest path already had.

Measured effect, and the figures are in
[`../evidence/memos/2026-08-01-deny-batching-and-window-compression.md`](../evidence/memos/2026-08-01-deny-batching-and-window-compression.md):
one request of a thousand suppressions falls from a thousand fsyncs to one, and from 3.289 seconds
to 31.9 milliseconds.

## What does not change

**A deny is still never refused for load.** The lane is chosen by the command rather than by which
method a caller invoked, and there is deliberately no route from it to a backpressure response —
a structural absence that survives an edit a comment would not.

**A failed append still applies deletions and suppressions and nothing else.** The rule that a deny
whose append fails is applied anyway — so the item is hidden and the caller still receives an
error — covers deletion and suppression only. Widening it to unsuppress re-exposes a suppressed
item while the response says nothing was applied. Batching makes this a per-entry fold over a
shared failure, which is the most dangerous part of the change and the one its test guards.
