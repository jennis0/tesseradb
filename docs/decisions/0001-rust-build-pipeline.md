# 0001 — The build pipeline is Rust, not Python

**Date:** 2026-07-28 · **Status:** Settled · **Supersedes:** the original "Python build pipeline" line in the corpus index

## Decision

One binary, two modes. `tessera build` and `tessera serve` are the same Rust program. Python is a
first-class **consumer** — the SDK, the supervisor, and the test-only reference oracle — and never
a component: no Python in any request path, in artifact production, or in the trusted computing
base.

## Why

Artifact production is inside the trust boundary. A bundle's bytes determine what every viewer can
see, so the code that writes them is security-relevant in the same way the serving path is. Two
languages there means two sets of dependencies to audit and two places for the same rule to be
implemented differently.

Keeping Python as a consumer preserves what it is good for — the reference oracle is deliberately
written "the slow, literal way" so that it disagrees with the engine when the engine is wrong.
That value depends on it being an *independent* implementation, which it cannot be if it is also
a component.

## Evidence

System architecture decision D4.
