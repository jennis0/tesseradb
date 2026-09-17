# 0143 — Term images live in the bundle, and a session's row projection is built by whichever of three routes costs least

**Date:** 2026-09-17 (ruling 2, the keep rule, 2026-09-16) · **Status:** Settled (owner ruling)

## What this answers

A session's row projection is the image of its mask fragment under a view's permutation, built by
walking one permutation slot per held entity. That walk takes seconds at 10⁹ rows for a broad
principal, measured in [`2026-09-14-term-images.md`](../evidence/memos/2026-09-14-term-images.md).
That memo set out the design and the measurements. A **term image** is one authorisation term's
base posting projected into a view's row space, stored so a session reads it instead of walking
it; the memo left its placement open at §6(a), together with the route a session takes once images
exist. The owner ruled on both in the handover that followed. This record carries the ten rulings,
dated as the handover gives them, and the further decisions the controller took while building
stages 1 to 5.

## The rulings

1. **Images live in the bundle**, written by build and by fold, covered by the bundle's digest
   sweep. `BUNDLE_FORMAT` moved 11 to 12 to carry them ([decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md):
   no compatibility, stale bundles refused). It has since moved 12 to 13, for a table field this
   record's own controller work added (below); a 12 bundle is refused at 13, not read short.
2. **Keep rule: an image is kept only above 30 rows per Roaring container** (2026-09-16), a
   constant written into the file header and checked at open. Compared against 10, 100 and 300 at
   the memo's §2.
3. **Images are read mapped**, as frozen `BitmapView`s, never built in the serving process.
4. **One implementation writes images at build and at fold** ([decision 0091](0091-build-is-ingest-into-an-empty-database.md)
   and decision 0139, which the code cites by number; no file of that number exists in this tree at
   the time of this record, and that gap is not this record's to close).
5. **Extents get no images.** Rows a flush or a merge added since the last fold are walked; the
   served set is identical either way. An ingest-only deployment gets images at its first fold.
6. **The complement route is included, chosen by cost, not by a fixed coverage share.**
7. **Rung 6 is rebuilt for measurement**, with each row's access list
   `[country, "y:"+year, "s:"+specieskey]`, about 1.4×10⁶ terms and about 1.0×10¹⁰ pairs, modelled.
   The controller checks the postings build time and the free disk before committing to the build,
   and reports both.
8. **Register: Appendix C row C19 is widened with two further branches rather than given a new
   row.** (a) The route a session takes is a function of the principal's own grant, pre-overlay, the
   same shape as the walk's own time. (b) Page-cache warmth of the image file across principals
   sharing a term is a timing channel the owner ruled very minor.
9. **Filter postings (category, text) are out of scope.**
10. **View groups pay table plus payload per key's view.** Accepted, and the build report states
    image bytes per view.

## Alternatives declined

- **A derived cache beside the bundle**, built lazily or on a schedule, independent of bundle
  format (memo §6(a)'s second option). Avoids a format bump and lets an operator skip the disk cost
  for a view nobody queries by term, at the cost of its own identity, its own integrity check and a
  miss path the bundle route does not need. Declined for ruling 1.
- **A coverage-share rule for the route**: the complement past a fixed percentage of the domain,
  the split otherwise (memo §7). Two principals at close access shares, 64.6% and 89.8%, showed the
  cheaper route differs between them and is not predicted by share, so ruling 6 chooses by cost
  instead.
- **A keep rule at 10 rows per container** (memo §7). The broadest species principal's split cost
  24.7 s against a 23.2 s walk at that cut, below its own break-even. Ruling 2 sets it at 30.

## What the controller decided, building stages 1–5

- **The derivation holds one window of `threads` postings and images at a time and writes the
  table through as each window completes**, so nothing in `derive_term_images` scales with the
  dictionary beyond the table itself (`tessera_store::term_images::derive_term_images`). A wider
  window buys nothing once workers are saturated at one term each, and this bounds transient memory
  to the window rather than to the whole file.
- **The build derives at most four workers wide and the fold one wide.** The build competes with
  nothing else on the host and rayon's own width is already capped elsewhere; the fold runs on its
  one dedicated thread (decision 0043) and stays sequential so its memory is bounded to one image
  and one scratch. The 64-part rung measured twelve build workers at 6.6 s against the fold's single
  thread at 1.5 s for byte-identical output: the terms are small enough there that the mutex over
  the scratch pool and the window's serial read dominate over the width bought.
- **The complement route is valid wherever `dense_rows` is recorded**, and adds the extents above
  it exactly as the walk's own route does, so a view carrying extents is not withheld from the
  route: `RowSpace::project_complement_base` returns the base's contribution alone and the caller
  unions `project_extents_from` above it.
- **The chooser prices the residual per entity, from a count the table carries, not per row.** A
  term-image table entry now records its base posting's cardinality (`TermImageEntry::entities`).
  The residual walk reads one permutation slot per entity whether or not the slot holds a row, and
  in a `group:key` view the rows are fewer than the entities, so pricing by rows priced the walk
  below its real cost. `open` refuses an entry whose image holds more rows than its posting has
  entities. This is the change that moved `BUNDLE_FORMAT` 12 to 13 and the file's header version 1
  to 2: a 12 file has zeros where the count now sits, and a 13 reader would price every unkept term
  at no entities.
- **A view with no image table is still priced walk against complement.** Images are written by the
  build and by each fold, so a view can be served with none, or a session can hold no term that has
  one; either way there is no split to price, and the residual is not summed and the delta postings
  are not read for it.
- **An IO failure reading postings for the split falls back to the walk, with a warning.** The
  postings the split route prices and walks are the ones the session's own fragment was just built
  from, so a failure here is a host condition rather than a state a request can reach. The walk
  returns the identical rows more slowly, and refusing the session's map for a fault that costs it
  nothing is the wrong trade.

## What this record does not decide

- **The chooser's constants** (`tessera_store::term_images::ROUTE_COSTS`) are modelled from the
  eleven principals measured on one corpus at rung 6, before the projection walk's bucket fix.
  Stage 6 re-takes them once rung 6 is rebuilt under ruling 7.
- **The fold's thread count is one**, priced from a model rather than a rung 6 measurement of pass
  2b. Stage 6 measures pass 2b on a fold and models rung 6 from it.
