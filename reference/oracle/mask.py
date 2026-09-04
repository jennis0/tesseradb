"""Mask construction and change composition — from `pairs.parquet`, independent of postings.

`mask_of` is the union-vs-semi-join differential's other side: the Rust engine derives a viewer's
authorised set from `postings.arrow` (per-term entity lists, possibly Roaring-compressed); this
module derives the same set from `terms/pairs.parquet` (the flat `(entity_id, term_id)` relation)
by direct scan. Agreement between the two is what the differential test proves.

`ChangeSet` composes changes in entity space. I1's formula is

    M_auth = (mask \\ L) ∪ direct_eval(L)

where `L` is the set of entities whose labels a `predicate` change has overridden — and **`L` is
empty here, permanently.** [Decision 0047](../../docs/decisions/0047-edit-is-delete-plus-reingest.md)
withdrew `predicate`: an edit is a delete followed by a re-ingest, the server refuses the op with a
422, and the WAL variant is deleted. No acked predicate change can exist, so the override arm could
never be entered, and this module carried it for a state no running system can produce —
`CLAUDE.md`'s pre-release rule points at deleting such a shape rather than documenting it. What
composes is therefore

    M_auth = mask \\ (deleted ∪ suppressed)

`delete` and `suppress` remove an entity from the result outright (both fail closed); `unsuppress`
is the only thing that undoes a `suppress`, per write-path §5.4's Rule S. `apply` still *accepts*
`predicate` — `journal` submits one to drive the refusal, which is a live test — and composes
nothing from it.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import pyarrow.parquet as pq


def mask_of(term_ids: set[int], pairs_path) -> set[int]:
    """Entity ids granted access to at least one of `term_ids`, scanned from `pairs.parquet`.

    Independent derivation from postings: this reads the flat (entity_id, term_id) relation, not
    the per-term postings arrays the server serves queries from.
    """
    if not term_ids:
        return set()
    table = pq.read_table(pairs_path, columns=["entity_id", "term_id"])
    entities = table.column("entity_id").to_pylist()
    terms = table.column("term_id").to_pylist()
    granted = set()
    for entity, term in zip(entities, terms):
        if term in term_ids:
            granted.add(entity)
    return granted


@dataclass
class ChangeSet:
    """Accumulated `/control/changes` history, applied in entity space (I1's composition rule)."""

    deleted: set[int] = field(default_factory=set)
    suppressed: set[int] = field(default_factory=set)

    def apply(self, entity_id: int, op: str, term_ids: set[int] | None = None) -> None:
        if op == "delete":
            self.deleted.add(entity_id)
        elif op == "suppress":
            self.suppressed.add(entity_id)
        elif op == "unsuppress":
            self.suppressed.discard(entity_id)
        elif op == "predicate":
            # Withdrawn by decision 0047 and refused with a 422, so a real deployment cannot ack
            # one. Accepted rather than rejected because a journal that submits one to *prove* the
            # refusal is a test worth having, and it composes nothing either way: `term_ids` names
            # a label set no session is evaluated against.
            if term_ids is None:
                raise ValueError("predicate change requires term_ids")
        else:
            raise ValueError(f"unknown change op '{op}'")

    def resolve(self, base_mask: set[int], session_terms: set[int]) -> set[int]:
        """`M_auth` = base_mask with deleted and suppressed entities dropped.

        `session_terms` is what `direct_eval` would have been evaluated against; with `predicate`
        withdrawn there is nothing left to evaluate, and it is accepted only because `journal` and
        both suites pass it. Narrowing the signature is a change that has to own those call sites.
        """
        resolved = base_mask - self.deleted
        resolved -= self.suppressed
        return resolved
