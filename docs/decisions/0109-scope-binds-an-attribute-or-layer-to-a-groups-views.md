# 0109 — `scope` binds an attribute or a layer to a group's views, inside the gate

**Date:** 2026-08-30 · **Status:** Settled (owner rulings, 2026-08-30)

## The decision

An attribute is entity-space and view-invariant by default, and declares nothing about views. The
one case needing declaration is a value that differs by view of a group:
`scope = { group = "quarter" }` — a family of entity-space columns, one per view, presence
bitmaps beside them, postings per column for a category. Evaluation stays in entity space, so
I2's filter argument is unchanged. A layer takes the same key with the same meaning: scoped, its
artifact rows carry `fields.view` and an artifact belongs to one view; unscoped, one artifact set
is drawn on every view the layer names.

Under a view of the group the request decides which column a leaf reads; elsewhere the leaf
**pins** (`sentiment@2026-Q3`, `sentiment@#3`) or is a 422. **The whole scoped surface sits
inside the group's gate**: `filter_operands` omits a scoped attribute where the gate fails, a
gate-failed pin is indistinguishable from an attribute never declared, and the unpinned 422
names the group only where the principal reaches it. Found in review as a fail-open — a
gate-failed principal filtering their visible entities by a gated view's column reads that
view's membership — and closed by decision 0090's argument: a gate at some surfaces and not
others is fail-open.

View metadata is not an attribute: one value per view, on the roster, filters nothing.

`views.md` §5 (r6) is the design.
