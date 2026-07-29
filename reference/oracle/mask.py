"""Mask construction and change composition — from `pairs.parquet`, independent of postings.

`mask_of` is the union-vs-semi-join differential's other side: the Rust engine derives a viewer's
authorised set from `postings.arrow` (per-term entity lists, possibly Roaring-compressed); this
module derives the same set from `terms/pairs.parquet` (the flat `(entity_id, term_id)` relation)
by direct scan. Agreement between the two is what the differential test proves.

`ChangeSet` composes changes in entity space exactly per I1's formula:

    M_auth = (mask \\ L) ∪ direct_eval(L)

where `mask` is the base (pairs-derived) authorised set, `L` is the set of entities whose labels
have been overridden by a `predicate` change, and `direct_eval` re-evaluates a *current* label
set against the session's granted terms. `delete` and `suppress` remove an entity from the result
outright (both fail closed); `unsuppress` is the only thing that undoes a `suppress`.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import pyarrow.parquet as pq

from . import bundle as bundle_mod


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
    # entity_id -> current descriptor term-id set, for entities with a `predicate` change applied.
    overrides: dict[int, set[int]] = field(default_factory=dict)

    def apply(self, entity_id: int, op: str, term_ids: set[int] | None = None) -> None:
        if op == "delete":
            self.deleted.add(entity_id)
        elif op == "suppress":
            self.suppressed.add(entity_id)
        elif op == "unsuppress":
            self.suppressed.discard(entity_id)
        elif op == "predicate":
            if term_ids is None:
                raise ValueError("predicate change requires term_ids")
            self.overrides[entity_id] = set(term_ids)
        else:
            raise ValueError(f"unknown change op '{op}'")

    def resolve(self, base_mask: set[int], session_terms: set[int]) -> set[int]:
        """M_auth = (base_mask \\ L) ∪ direct_eval(L), then drop deleted/suppressed entities."""
        overridden = set(self.overrides)
        resolved = base_mask - overridden
        for entity_id, terms in self.overrides.items():
            if terms & session_terms:
                resolved.add(entity_id)
        resolved -= self.deleted
        resolved -= self.suppressed
        return resolved
