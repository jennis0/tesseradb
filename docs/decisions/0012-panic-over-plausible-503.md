# 0012 — A loud panic behind an unconstructable type, over a plausible 503

**Date:** 2026-08-01 · **Status:** Settled

## Decision

Where a code path is unreachable because its type has no constructor, it is left as
`unimplemented!()` rather than returning a plausible error.

## Why

The alternative — returning `Err(ExecutorDead)` — produces a 503 that looks like ordinary
degradation. The day someone adds a constructor, that path silently "works": it returns a
believable error for a condition that is not the one it names, and nothing fails loudly enough to
be noticed.

A panic behind a type nobody can construct cannot fire today, and cannot be ignored tomorrow.

## Note

These were the first `unimplemented!()` in `crates/`. The tree previously had exactly zero, so
"grep finds none" is no longer a usable invariant check.

## Evidence

Controller note, stage 2.1 Task 0 gate.
