# Polygon membership — requirements

**Status:** **Requirements only.** This document states what must be true. It contains no design,
no mechanism and no proposed implementation, and it does not decide anything the design will have
to. Nothing here is built: today `[layer.shape]` accepts `{ kind = "bbox", depth }` and a polygon
is refused (`configuration.md` §1).

**Required by** [`../evidence/memos/2026-08-27-ingest-campaign-plan.md`](../evidence/memos/2026-08-27-ingest-campaign-plan.md) §9.
**Depends on** [`projections.md`](projections.md): which spaces exist, and what each one does to a
shape, is that document's question. R9–R12 are the part that belongs here.

---

## 1. Why this exists

Three consumers in current work need the same operation — selecting the points inside a polygon,
efficiently enough to answer inside a request:

1. **Published boundary sets.** Administrative geographies with many polygons at several nested
   levels (ONS output areas through to local authorities; Overture's twelve division subtypes).
2. **Regions defined at request time.** A viewer drawing an area and asking what is in it.
3. **The join the writer is currently obliged to do offline**, stamping each point with the code of
   every region containing it, because the server cannot answer the question.

## 2. Requirements

**R1.** An artifact's membership may be declared as a polygon.

**R2.** Membership is **exact**: the members are the points inside the polygon and no others. A
membership wider than the declared shape is not acceptable at any tolerance.

**R3.** Membership does **not go stale**. A point ingested inside the polygon is a member on the
next request, with no refresh, republication or rebuild — the guarantee `bbox` spatial membership
gives today.

**R4.** Every quantity served from such a membership is computed from inside the viewer's own
visible set alone (§4's **I2**), and the shape is not a route by which a count, extent or shape
reveals a point outside it.

**R5.** Per-request cost is **bounded and declared**. A layer carrying many polygons, or a viewport
intersecting many of them, must not be able to produce an unbounded response or an unbounded
evaluation.

**R6.** A polygon may be **replaced or edited without re-ingesting the points it contains.**
Published boundary sets are revised, and the cost of a revision must fall on the boundary rather
than on the corpus.

**R7.** A layer may carry **many polygons across several levels**, with the containment between
levels expressible.

**R8.** Polygons are declared in the **view's own coordinate system**, on the same frame as the
points they select.

**R9.** A polygon is declared in **either WGS84 or the view's projected space**, and which of the
two is part of the declaration. A caller is not obliged to convert in order to submit.

**R10.** The space a polygon is declared in **defines the plane its edges are straight in**. The two
readings are not equivalent — an edge that is straight in one is a curve in the other — so this is
what the caller is choosing when they choose a space, and not an encoding detail.

**R11.** Transforming a polygon from the space it was declared in **does not change which points it
contains**, beyond the resolution of the grid the points are stored on.

**R12.** A declaration that does not match the coordinates supplied does not silently produce a
wrong membership.

**R13.** All three consumers in §1 are served. Request-time regions and published boundaries are the
same question asked at two times, and a capability that answers only one of them does not meet this
requirement.

## 3. Out of scope

- Drawing the polygon. Supplied shape as *content* already exists and is unaffected.
- Any shape other than a polygon. Whether circles, radii, multipolygons and holes are in or out is
  a question for the design, and this document does not answer it.

## 4. Open questions for the design

Recorded so they are not mistaken for settled, and not answered here.

- What bounds R5 — the existing artifact budget, or a bound of its own.
- Whether a request-time region (§1.2) is an artifact, a filter operand, or both.
- Whether multipolygons and holes are required, and by which consumer.
- What `depth` means, if anything, once a shape is not a box: on a `bbox` it **is** the membership
  (`configuration.md` §1), and that reading cannot carry over unchanged to a shape whose membership
  is the shape.

## Appendix R — review trail

**r1 (2026-08-27).** Written from the owner's ruling that three consumers needing one operation
makes it a first-class capability rather than a per-consumer workaround. Requirements only, by owner
direction: assumptions and candidate mechanisms deliberately excluded. R9–R12 added later the same
day on the owner's ruling that a polygon may arrive in either coordinate space rather than the
caller being forced to pick one. **Not reviewed.**
