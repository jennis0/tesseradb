"""Known-answer tests for `oracle.mask` — I1's composition and the pairs-derived mask, by hand.

`mask_of` and `ChangeSet` are checked everywhere else against the engine they exist to referee, so
a rule both got wrong the same way would be reported as conformance. These cases assert the two
against answers derived from the design — I1, and write-path §5.4's two removal rules — over a
five-row `pairs.parquet` this file writes, so nothing here needs the Phase 0 corpus or a server.

**Nothing below composes from a `predicate` op, because nothing does.** Decision 0047 withdrew
`predicate` and the server refuses it with a 422, so the `∪ direct_eval(L)` arm was unreachable
and has been deleted; `resolve` is now `base_mask − deleted − suppressed`. `apply` still
*accepts* the op — `oracle.journal` submits one to drive that refusal — and composes nothing from
it, which is what the case below asserts. `resolve`'s `session_terms` parameter is likewise
retained and inert, so every call here passes one and no answer depends on it.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from oracle.mask import ChangeSet, mask_of


# The fixture relation, small enough to read. `pairs.parquet` is the flat `(entity_id, term_id)`
# grant relation; an entity appears once per term it is granted.
#
#   entity 1 -> terms 10, 11
#   entity 2 -> term 11
#   entity 3 -> term 12
#   entity 4 -> term 10 (twice — a duplicate row, which a union must not double-count)
PAIRS = [
    (1, 10),
    (1, 11),
    (2, 11),
    (3, 12),
    (4, 10),
    (4, 10),
]


@pytest.fixture
def pairs_path(tmp_path):
    table = pa.table(
        {
            "entity_id": pa.array([e for e, _ in PAIRS], type=pa.uint64()),
            "term_id": pa.array([t for _, t in PAIRS], type=pa.uint32()),
        }
    )
    path = tmp_path / "pairs.parquet"
    pq.write_table(table, path)
    return path


def test_mask_of_is_the_union_over_the_granted_terms(pairs_path):
    """A viewer sees an entity granted **at least one** of its terms (§6.3's union, I1's base).

    Working, straight off the fixture table: term 10 alone reaches entities 1 and 4; term 11
    alone reaches 1 and 2; the two together reach 1, 2 and 4 — the union, not the intersection,
    which would be {1}. Entity 3 is reached by neither.

    Kills: a semi-join written as an intersection (the {1} answer); an `and` where the scan needs
    an `or`.
    """
    assert mask_of({10}, pairs_path) == {1, 4}
    assert mask_of({11}, pairs_path) == {1, 2}
    assert mask_of({10, 11}, pairs_path) == {1, 2, 4}
    assert mask_of({12}, pairs_path) == {3}


def test_mask_of_counts_a_repeated_grant_once(pairs_path):
    """The mask is a *set* of entities, so a duplicated pair adds nothing.

    Working: entity 4 holds two rows for term 10 and appears once in the result, which is what
    makes `|mask_of(...)|` a cardinality the θ anchor can use.

    Kills: a list or a multiset in place of the set — invisible to a membership test and visible
    to every count derived from it.
    """
    assert mask_of({10}, pairs_path) == {1, 4}
    assert len(mask_of({10}, pairs_path)) == 2


def test_an_unheld_term_grants_nothing(pairs_path):
    """A term no entity is granted names an empty operand, not an error and not everything."""
    assert mask_of({99}, pairs_path) == set()


def test_no_granted_terms_is_the_empty_mask(pairs_path):
    """A session granted nothing sees nothing — fail-closed (I1; §4).

    The empty case is the one an implementation is most likely to get backwards, because "no
    filter on terms" reads as "no restriction" in every other query language.

    Kills: returning every entity in the relation for an empty term set.
    """
    assert mask_of(set(), pairs_path) == set()


# -- ChangeSet: composition in entity space ------------------------------------------------------


def test_no_changes_resolves_to_the_base_mask():
    """`M_auth` is the base projection when nothing has been denied (I1)."""
    assert ChangeSet().resolve({1, 2, 4}, {10, 11}) == {1, 2, 4}


def test_a_deletion_and_a_suppression_both_remove_the_entity():
    """Both denials fail closed: the entity is absent from `M_auth` (I1; write-path §5.4).

    Working: base {1, 2, 4}; delete 1 and suppress 2 leaves {4}. Neither op consults the
    session's terms — a denial is not a re-authorisation.

    Kills: either op silently doing nothing; either op being applied before rather than after the
    base, which would let a later grant resurrect the entity.
    """
    changes = ChangeSet()
    changes.apply(1, "delete")
    changes.apply(2, "suppress")
    assert changes.resolve({1, 2, 4}, {10, 11}) == {4}


def test_unsuppress_retires_a_suppression_and_nothing_else():
    """Rule S: a suppression retires **only** on unsuppress (write-path §5.4).

    Working: suppress 2 then unsuppress 2 restores it, because that is the one retirement route
    a suppression has. Delete 1 then unsuppress 1 leaves 1 deleted — a deletion retires only at
    the compaction fold that executes it (Rule F), and conflating the two is the fail-open
    mistake §5.4 names.

    Kills: `unsuppress` clearing the deleted set as well; `unsuppress` implemented as "remove
    from every denial set".
    """
    changes = ChangeSet()
    changes.apply(2, "suppress")
    changes.apply(2, "unsuppress")
    assert changes.resolve({1, 2, 4}, {10, 11}) == {1, 2, 4}

    changes.apply(1, "delete")
    changes.apply(1, "unsuppress")
    assert changes.resolve({1, 2, 4}, {10, 11}) == {2, 4}


def test_unsuppressing_something_never_suppressed_is_inert():
    """An unsuppress with no suppression to retire adds nobody to `M_auth`.

    Working: entity 9 is not in the base mask, and unsuppressing it must not put it there — an
    unsuppress retires a denial, it does not grant.

    Kills: `unsuppress` adding to a set rather than discarding from one; a `KeyError` on the
    discard, which would make a duplicate control-plane message a 500.
    """
    changes = ChangeSet()
    changes.apply(9, "unsuppress")
    assert changes.resolve({1, 2, 4}, {10, 11}) == {1, 2, 4}


def test_denials_apply_to_entities_outside_the_base_without_effect():
    """Denying something the viewer could not see anyway changes nothing it can see.

    A deny is broadcast in entity space, so it routinely names entities outside a given session's
    base mask; the composition must be a set difference rather than a removal that assumes
    membership.

    Kills: a `set.remove` in place of a difference, which would raise on the common case.
    """
    changes = ChangeSet()
    changes.apply(7, "delete")
    changes.apply(8, "suppress")
    assert changes.resolve({1, 2}, {10, 11}) == {1, 2}


def test_resolve_does_not_mutate_the_base_mask():
    """`resolve` returns a value; the base projection it composes over is unchanged.

    The base is a per-session artefact reused across requests, and a composition that mutated it
    would make a denial permanent for that session and invisible to the next `resolve` — which
    also means the second call would answer a question it was not asked.

    Kills: `base_mask -= ...` in place of `resolved = base_mask - ...`.
    """
    base = {1, 2, 4}
    changes = ChangeSet()
    changes.apply(1, "delete")
    assert changes.resolve(base, {10, 11}) == {2, 4}
    assert base == {1, 2, 4}
    assert changes.resolve(base, {10, 11}) == {2, 4}


def test_a_predicate_op_is_accepted_and_composes_nothing():
    """`predicate` is withdrawn (decision 0047) and the composition no longer has an arm for it.

    Working: the op is accepted — `oracle.journal` submits one to drive the server's 422, so
    rejecting it here would break that path — and `M_auth` is unchanged by it, whatever terms it
    names and whatever the session holds. Entity 5 is outside the base mask and stays outside;
    entity 1 is inside and stays inside.

    Kills: a re-introduced `∪ direct_eval(L)` arm that admits an entity on the strength of a
    label change no acked op can carry; an `apply` that raises on `predicate` and takes
    `test_journal.py`'s refusal path with it.
    """
    changes = ChangeSet()
    changes.apply(5, "predicate", {10})
    changes.apply(1, "predicate", {99})
    assert changes.resolve({1, 2, 4}, {10, 11}) == {1, 2, 4}


def test_session_terms_do_not_move_the_composition():
    """`resolve`'s `session_terms` is retained and **inert** — the base mask is already composed.

    The parameter survives because `journal.py` and `conformance/` pass it; nothing reads it. A
    composition that started consulting it would be re-authorising in the wrong place, below the
    base projection rather than above it.

    Kills: a `resolve` that intersects or widens the base against the session's terms.
    """
    changes = ChangeSet()
    changes.apply(2, "suppress")
    assert changes.resolve({1, 2, 4}, set()) == {1, 4}
    assert changes.resolve({1, 2, 4}, {10, 11, 12, 99}) == {1, 4}


def test_an_unknown_op_is_refused_rather_than_ignored():
    """An op the composition does not implement raises — it is never a no-op.

    A silently ignored change is a denial that was accepted and never enforced, which is the
    fail-open shape `CLAUDE.md` puts on the disclosure side of the line.

    Kills: an `else: pass` arm.
    """
    with pytest.raises(ValueError, match="unknown change op"):
        ChangeSet().apply(1, "retract")
