# 0022 — deepscatter is rejected, on licence and on architecture

**Date:** 2026-07-28 · **Status:** Settled

## Context

deepscatter is the closest existing thing to this system's rendering layer, and its tiling
approach looks architecturally adjacent. It will be proposed again by anyone who finds it.

## Decision

Not adopted as code. Two independent reasons, either of which is sufficient.

## Licence

deepscatter is **CC-BY-NC-SA**.

**NonCommercial** turns on the character of the use rather than on whether anything is sold, so it
is not discharged by not charging for the service. **ShareAlike** would force any release to carry
a licence that is not open source by the OSI definition.

## Architecture

Its quadfeather tiler assigns points to tiles **in fill order, with no per-point priority** — which
is exactly the mechanism this design replaces. Priority-based level of detail is what makes the
selection nest across zoom and compose across partitions.

So the half that makes deepscatter architecturally close is the half that cannot be used.

## What to take instead

The **manifest-with-per-tile-ranges** shape and the **sidecar-column split**, as patterns rather
than as code.

## Evidence

Moved here from the retiring implementation plan §3. The prior-art review of visual analytics
covers deepscatter and quadfeather in full: [`../evidence/prior-art/prior-art-2-visual-analytics.md`](../evidence/prior-art/prior-art-2-visual-analytics.md).
