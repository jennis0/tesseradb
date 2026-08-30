# 0111 — A shape spans projected views through wgs84, and no geometry spans both kinds of space

**Date:** 2026-08-30 · **Status:** Settled (owner rulings, 2026-08-30)

## The decision

A shape layer may span views with **different projections and different extents**: a `wgs84`
shape is densified, projected, clipped and decomposed through each view's own declared
transform — the function that placed that view's points — run once per view, membership resolved
per view against that view's stored positions. This supersedes the 2026-08-29 refusal of
mixed-projection shape layers, which asserted agreement the per-view pipeline never needed.

Three rules bound it:

- **`view`-space geometry spans only views sharing projection and frame** — its coordinates are
  one frame's, and `wgs84` is the spelling that spans. A view group's views share both by
  construction, so a shape layer scoped to a group carries `view`-space geometry safely.
- **A layer's views are all projected or all `none`.** `wgs84` means nothing in an embedding;
  no geometry spans the two kinds of space, refused at the layer declaration.
- **Warn, never block, on a shape wholly outside a view's extent** — empty membership there,
  the count reported beside the clip counts. The two-`none`-views warning is removed: spanning
  is opt-in, and a caller who declared it needs no second-guessing.

Owned consequence: two projected views of one geography can disagree about a boundary point,
each testing its own quantised position — exact per view is the semantics, and the divergence
is bounded by quantisation.

`polygon-membership.md` §4.3 (r-bumped this date) is the normative text; `views.md` §3.5 carries
the group note. `canonical_shapes` becomes per-view at the shape stage of the views delivery.
