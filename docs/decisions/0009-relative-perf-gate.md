# 0009 — The per-track performance gate is relative, not absolute

**Date:** 2026-07-31 · **Status:** Settled

## Decision

Parallel tracks gate on the 2.4M criterion baseline used as a **no-regression (relative)** check.
The 10⁹ A/B and the absolute-budget restatement move to stage close, when disk is freed
deliberately.

## Accepted risk, recorded so it is chosen rather than discovered

A regression that only manifests at 10⁵× scale stays hidden until the end of the stage.

## Why

Running the 10⁹ measurement per track costs disk that has to be reclaimed between runs, and the
tracks are not independently comparable at that scale anyway. A relative gate catches the common
case; the scale-dependent case is caught once, deliberately, where the measurement work is already
scheduled.

## Evidence

Owner decision in the stage 2.1 ledger. Baseline at
`docs/archive/plans/bench-baselines/2m4-criterion-baseline.json`.
