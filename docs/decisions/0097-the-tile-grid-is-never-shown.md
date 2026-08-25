# 0097 — The tile grid is never shown; a selection's highlight is the shape drawn

**Date:** 2026-08-24 · **Status:** Settled (owner ruling on the design canvas; transcribed 2026-08-25
from [`client-components.md`](../design/client-components.md) §5.10–§5.11 and §11).

## The decision

The storage's cells — the Morton tiles a request is addressed by and a region is counted over —
are **never drawn**, anywhere in the components. A box or lasso selection highlights **the shape
the user drew**; the cells it was counted over are a fact about storage. Where a counted cell
exceeds a screen pixel, the region's counts are typed **not exact** and render as such — the
inexactness is stated in the number, not drawn as a grid.

## Why

The grid is an implementation coordinate, not a fact about the corpus. Showing it invites reading
tile boundaries as structure and reading a cell cover as the selection. The honest presentation is
the user's shape plus a typed statement of how exact the count under it is.
