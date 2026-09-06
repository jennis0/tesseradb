# 0132 — A view gate's label is one label

**Date:** 2026-09-06 · **Status:** Settled (owner ruling) · ⊘ not yet built

## What this answers

Decision 0129 took the comma grammar off the ingest wire: an item's access labels travel as a
list and each element is one label, verbatim. The same split-on-comma survived in one place — a
view's `visibility` gate label, read by `Plugin::terms_of_label` from the declaration
(`views.md` §6, `[[view_group.view]] visibility`), from `PUT /control/views/{group}/{key}`, and
from `tessera-build`'s parse of the same field. A gate label containing a comma splits into
fragments the same way an access label did, and the wire track's referee found a test gating on
`"finance,legal"` and relying on it.

Three options were put: a gate is one label and a gate wanting several terms declares a list;
the comma grammar stays for gate labels only, documented as the operator's own; nothing.

## The decision

**A gate's `visibility` is one label, taken verbatim; a gate wanting several terms declares
them as a list.** `terms_of_labels(&[label])` at every reader of the field — the declaration, the
control route's body and the build's parse — and `Plugin::terms_of_label` goes, the wire having
already stopped using it. `views.md` §6 and the declaration's field type change with it; the
control route's body carries a list where it carried a string, under the no-compatibility rule
(decision 0048).

## Why

0129's reason, one place along: a label is a key the operator owns, and a grammar that reads
structure into a key's bytes will one day read structure that is not there. Keeping one grammar
for gates alone would leave the two label readers disagreeing about what a comma means, which is
the state 0129 ended.

## Consequences

- `views.md` §6 and `configuration.md`'s `visibility` row say list-or-one-label; the control
  route's contract row moves in the same commit; `api_version` does not move (0129's precedent).
- A gate test that relied on the split is rewritten to declare its terms.
- Nothing on the viewer wire changes: a gate's terms were never sent.
