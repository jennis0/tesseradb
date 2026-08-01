# 0030 — Response determinism is an implementation detail, not a guarantee

**Date:** 2026-08-01 · **Status:** Settled

## Context

The same request against the same generation currently produces **byte-identical** responses
whatever the configured thread count. It is true by construction rather than by luck: the parallel
tile loop collects into a shape that keeps it on rayon's *indexed* collect path, so output order
equals input order, and the serial and parallel folds are byte-identical. Several tests assert it.

## Decision

**It is a documented implementation detail. It is not a guarantee, and downstream must not rely on
it.**

Say all three things wherever it is described: the property holds today, a future optimisation may
remove it, and a client that depends on byte-stable responses is depending on something the service
does not promise.

## Why not promise it

Promising it taxes every future optimisation for as long as the system exists: no non-deterministic
reduce, no work-stealing that reorders output, no vectorised path that produces a different but
equally correct ordering. That is a large permanent constraint bought for a property no caller has
asked for — a client reads the response, and two byte-different encodings of the same served set
are equally correct to it.

## The one consumer that may rely on it, and why that is not a contradiction

The conformance suite's canary comparison compares responses between fixture states, and it does not
yet canonicalise before comparing — so it currently depends on determinism to make the comparison
meaningful.

That is legitimate, because **the suite pins its own configuration**. Relying on determinism at a
fixed thread count is a much weaker thing than a guarantee across configurations, and the suite
controls the variable. This must be stated where the suite describes the comparison, or the first
person to run it at a different thread count gets a failure with no explanation.

If the suite ever needs to compare across configurations, it canonicalises first — which its own
design already specifies and has not yet implemented.

## Evidence

Register row A7. `crates/tessera-engine/src/viewport.rs` — the collect-shape argument and why
`Vec<Result<..>>` rather than `Result<Vec<..>>`; `crates/tessera-engine/tests/viewport.rs` — the
assertions. `docs/design/conformance.md` §4.2 for the canary comparison.
