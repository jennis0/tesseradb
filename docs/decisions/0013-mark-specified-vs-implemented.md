# 0013 — Specified-but-unbuilt machinery is marked per claim

**Date:** 2026-08-01 · **Status:** Settled

## Context

Three independent audits of the design corpus against the code found the same thing: the corpus is
written in the present tense about machinery that does not exist. The documents describe the
intended end state; the implementation is behind it.

Mostly that is harmless ambition. In three places it was not, because on a security property the
present tense reads as an assurance:

- Of the three deny-retirement rules, one is built. The other two are safe only because nothing
  retires at all — fail-closed, but not the mechanism described.
- Three of thirteen invariants are covered by the conformance suite as designed.
- I13 named two different properties, only one of which is implemented, so a reviewer grepping the
  invariant number concluded both were covered.

## Decision

A claim about unbuilt machinery is marked **at the claim**, never only in a preamble:

> **⊘ Specified, not implemented.** What exists instead, and what the reader must not assume.

Where the unbuilt thing is a guarantee, the marker also states the current behaviour and whether
it is safe. The full set is summarised in [`../design/README.md`](../design/README.md) so it is
countable.

## Why not the alternatives

Splitting each document into built and planned sections fragments arguments that legitimately span
both. Marking only the security-bearing cases is cheaper but leaves a reader unable to tell in
general — and the whole problem is that the reader could not tell.

## Evidence

[`../design/DIVERGENCE-REGISTER.md`](../design/DIVERGENCE-REGISTER.md).
Convention in [`../agents/writing.md`](../agents/writing.md).
