# 0008 — The candidate-list selection route is declined

**Date:** 2026-07-31 · **Status:** Settled

## Decision

There is one selection route: direct evaluation of the definition from the mask, at every
coverage. The second route the design specified — per-node precomputed lists of the top *c·k*
items, unmasked, filtered at query time — is **not built** and will not be.

## Why

Four reasons, argued at length in `crates/tessera-engine/src/select.rs`. The two that decide it:

- **It inverts I7.** Filtering a precomputed unmasked list at query time means a sparse principal
  can see an empty tile where items exist. Tippecanoe's `--retain-points-multiplier` fails exactly
  here, below a pass rate of 1/N. The failure is silent — the map simply goes blank for the
  principals with the least access.
- **Descent cost is linear in 1/coverage, not logarithmic.** The measured node counts are 21 at
  5% coverage, 85 at 1%, 5,461 at 0.01%. This argument originally supported building the route;
  the measurements are what turned it around, and it is retained as the reason.

## Enforcement

`scripts/check-layers.sh` fails if the `NO CANDIDATE-LIST ROUTE` marker is removed from
`select.rs`. Deleting the direct path "to simplify" is the failure mode, not a tidy-up.

## Evidence

Phase 2 roadmap, ruling 1. Argument and measurements in `crates/tessera-engine/src/select.rs`.
