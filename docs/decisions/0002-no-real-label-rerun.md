# 0002 — No real-label rerun

**Date:** 2026-07-28 · **Status:** Settled

## Decision

The Phase 0 measurements over the synthetic 10⁹ corpus are accepted as final. There will be no
rerun against a real access-labelled corpus.

## Why

No real access-labelled corpus is available to this project. Blocking on one would block
indefinitely.

## The caveat that survives

Three headline results are **policy-dependent** — they depend on how a real deployment's labels
are distributed, not on the engine: signature alignment, posting compression, and union cost. Any
deployment with real labels should re-run the Phase 0 measurements before trusting those figures.

This is deployment guidance, not a project obligation.

## Evidence

Design r18. Measurements in [`../../probes/`](../../probes/).
