# 0110 — The ordinal gap is accepted as a register row

**Date:** 2026-08-30 · **Status:** Superseded by [0113](0113-ordinals-are-removed-and-the-key-is-the-only-address.md) (2026-08-31) — ordinals are removed entirely, so the gap this accepted cannot be observed and its register row C27 is deleted

## Context

A group's ordinals are monotone and served in roster order; a gate-failed view is omitted. A
principal seeing ordinals 0, 1, 3 learns *a* view exists at #2 that they may not see, and a
moving high-water counts hidden creations — found in the r6 security review as a defeat of
name-level indistinguishability.

## The decision

**Accepted, as a new Appendix C row**, on C15's argument: knowing something was created is not
knowing whose or what; a gap and a dropped key are indistinguishable; and a deployment whose
roster shape is sensitive gates the group, which hides the whole roster, gaps included.

Rejected: per-principal-dense ordinals and serving no ordinals — both re-open the per-session
handle machinery decision 0006 retired, to close a channel of one bit per creation.

`views.md` §9 (r6) carries the row's text; it enters architecture Appendix C at fold-in.
