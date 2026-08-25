# 0098 — The status strip is the default; the expanded card is optional

**Date:** 2026-08-24 · **Status:** Settled (owner ruling on the design canvas; transcribed 2026-08-25
from [`client-components.md`](../design/client-components.md) §5.3 and §11).

## The decision

`<tessera-status>` is by default **one line** on the map — the state as a badge, the three counts
in the order *shown · matched · visible*, the refresh control when stale — and always in view
(bottom-left in both explorer layouts). The detail behind the numbers (the drawn region,
provisional marks, replica holdings) is a hover. An `expanded` attribute renders the card for a
host that wants it.

## Why

On the mock-up boards the expanded view-info card added nothing the strip and a hover did not, and
a panel that is not always in view cannot do the one job the state display has: making a refused,
expired or stale view impossible to mistake for a current one.
