# 0090 — A vocabulary has one visibility axis, and keeps one key

**Date:** 2026-08-19 · **Status:** Settled (owner ruling)

## Context

[Decision 0088](0088-visibility-is-two-axes-and-the-membership-test-is-one.md) ruled that everything
about who may see an object answers one of two questions — `visibility`, which access label the
viewer must hold, and `require_member_visibility`, how much of the object's own membership the
viewer must already see — and gave each one key. In passing it declared the word `derived` retired,
because it meant *any member* on a vocabulary and *every member* on supplied content: one word,
two quantifiers, in adjacent controls.

`configuration.md` kept `derived` as a vocabulary's live setting regardless, and the two documents
have read as contradicting each other since. The question this settles is which of them was right.

## The decision

**A vocabulary keeps one key with two words:**

```toml
visibility = "public"   # or "derived"
```

`public` — every value name is served to every principal as authored. `derived` — a value is served
where the viewer can see at least one item carrying it, evaluated per request from inside the mask.

**A vocabulary was never merging two axes.** `public` is not the label axis at its widest; a
vocabulary has no label axis at all. `public` means *no membership requirement* and `derived` means
*any member* — one axis, two settings, which is what one key is for. The two-axis shape belongs to
objects that have two axes.

**`derived` is unambiguous now.** 0088's objection was the collision with supplied content, and that
collision has since disappeared on its own: content's settings are `all` and `inherited`, so the
word appears in one place only. The reason to retire it expired; the word stays.

## What was tried and reverted

The two-key form was written and then discarded before it landed, and the reason it failed is the
reason to record it here rather than leave it to be re-attempted.

Splitting the key gives a vocabulary `visibility` and `require_member_visibility` like everything
else, and appears to buy a capability the merged key cannot express: gating a vocabulary behind an
access label — *only an analyst may know this category set exists*. **That capability is not
available at this size.** A vocabulary hangs off a *column*, and a column reaches a principal
through six surfaces: `/v1/meta`'s schema list, `/v1/categories`, the item drill-down's resolved
value key, `/v1/meta`'s filter-operand list, the filter parser — which is built from a session-free
schema snapshot, so a gated column stays filterable and its member counts readable — and the render
column that ships codes in every viewport frame under the column's name. A gate at two of the six is
fail-open, and a disclosure control that is accepted and not enforced is the failure this corpus
exists to prevent.

So the split produced `visibility` with exactly one legal value, the label spelling refused, and the
capability absent: two keys where one sufficed, one of them inert. Worse than either alternative,
and reverted.

**If the six surfaces are ever gated, the split earns itself and can be made then.** The work is a
column-level gate, not a vocabulary-level one, and that is the shape a future attempt should take.

## Consequences

- The contradiction between 0088 and `configuration.md` is closed in the design's favour. 0088's
  two-axis rule stands for every object that has two axes; a vocabulary has one, and is not an
  exception to the rule but an instance of a different case.
- The leak register is unchanged: `derived` is the setting C11's row already names.
- ⊘ **A vocabulary cannot be gated by an access label**, and the parser takes no label in that
  slot — refused rather than accepted and ignored. What is absent is a column-level gate across
  six surfaces, not a spelling.
