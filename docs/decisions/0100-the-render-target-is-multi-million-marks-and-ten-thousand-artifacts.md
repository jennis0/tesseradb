# 0100 — The render target is multi-million marks and 10⁴-plus artifacts a layer

**Date:** 2026-08-25 · **Status:** Settled (owner ruling; transcribed 2026-08-25 from
[`client-components.md`](../design/client-components.md) §5.10 and the handover's §1).

## The decision

The components are sized for **several million marks on screen** and **ten thousand or more
artifacts in a layer** (a layer may hold 10⁶–10⁷; the served set per view is bounded by the cut and
`artifact_budget`). The viewer's `DEFAULT_BUDGET` is only the input's default, not a ceiling.

## Consequences

Every colouring interaction must be **O(artifacts), never O(points)**: membership is a per-point
GPU attribute resolved through a lookup texture, and the session artifact table's ordinals are
refcounted per band so the table and the texture are bounded by resident marks. A per-point colour
rewrite at this scale is tens of milliseconds and a 12 MB upload per interaction, and is the
construction the design refuses. The design's §5.10 figures are modelled; step 2's harness measures
them and [`client-delivery.md`](../client-delivery.md) records what was measured.
