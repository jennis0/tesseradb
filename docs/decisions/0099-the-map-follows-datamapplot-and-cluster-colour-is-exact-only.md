# 0099 — The map's look follows DataMapPlot, and colour by cluster is exact only

**Date:** 2026-08-24 (look) and 2026-08-25 (exact only) · **Status:** Settled (owner rulings;
transcribed 2026-08-25 from [`client-components.md`](../design/client-components.md) §5.10 and §11).

## The decision

- The map's look follows **DataMapPlot**: cluster colours, a single-hue density wash from the
  number channel, faint nested outlines from the served hull or box, names sized by masked count.
- **Colour by cluster is exact only.** A point wears a cluster's colour only when the wire said it
  is a member. There is **no geometric guess** between — no nearest-centroid mapping, no
  hull-containment test on the client. The per-point membership column (D12: the deepest *served*
  artifact per named layer, per response, null otherwise) is therefore a **prerequisite** of colour
  by cluster, not an upgrade to it. Until it serves, points colour by column only and clusters are
  outlines and names; a geometric fallback is not built.
- Contours and colour derived from held marks are not drawn: they are the density of a
  per-tile-capped sample, and exact-only applies to shapes too.

## Why

A coloured point asserts membership. A nearest-centroid guess is a plausible-looking assertion the
wire never made, and on a masked map it can name an artifact the principal was not served. Neutral
means *not known here yet*, which is true; a guessed colour is not.

## Consequences

D12 needs a leak-register pass before it serves (the column reveals membership of served points in
served artifacts, which the served hull already bounds; deepest-served keeps finer structure out).
The client's data path for the column — the session artifact table, ordinals named on the main
thread, the lookup texture — is §5.10 of the design.
