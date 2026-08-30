"""Known-answer tests for `oracle.text` — the `match` and `phrase` arithmetic, by hand.

**What is under test here is the set-and-sequence arithmetic, not the segmentation.** `tokenise`
and `identity` reach the shipped `tessera tokenise` verb (decision 0070), which is a declared
echo: asking the binary what a string segments into and then asserting the answer would be testing
the CLI, and re-implementing UAX #29 in Python would compare ICU4C against icu4x and turn every
disagreement into a research question. So those two functions are **not** covered below, and the
gap is the deliberate one the module doc argues for. `TextColumn` takes its token streams as data,
which is what makes the part this file does cover checkable against records §10 alone — and what
lets these cases run with no binary, no bundle and no corpus.

Every token list below is written out rather than tokenised, for the same reason: a fixture that
went through the analyser would make an assertion about the analyser.
"""

from __future__ import annotations

from oracle.text import TextColumn


#   entity  token stream
#        1  the quick brown fox
#        2  quick quick brown          (a repeat, which `match` collapses and `phrase` does not)
#        3  brown fox quick            (the same three words, a different order)
#        4  (nothing)                  (an empty stream — present, carrying no token)
COLUMN = TextColumn(
    tokens={
        1: ["the", "quick", "brown", "fox"],
        2: ["quick", "quick", "brown"],
        3: ["brown", "fox", "quick"],
        4: [],
    }
)


# -- match: m-of-n over the query's distinct tokens -----------------------------------------------


def test_match_with_no_minimum_is_the_plain_conjunction():
    """`minimum` of `None` is *all of them* (records §10).

    Working: ["quick", "fox"] needs both. Entity 1 holds both, entity 3 holds both, entity 2
    holds only "quick", entity 4 holds nothing.

    Kills: a default that behaves as `any` — a filter that silently widens.
    """
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, ["quick", "fox"], None)) == {1, 3}


def test_match_counts_distinct_query_tokens_not_occurrences():
    """`match` is over the query's **distinct** tokens, and position does not matter.

    Working: the query ["quick", "quick"] has one distinct token, so `minimum=1` is satisfied by
    every entity holding "quick" — 1, 2 and 3. Entity 2 holds "quick" twice and that buys it
    nothing.

    Kills: counting occurrences in the held stream, which would let entity 2 satisfy a
    `minimum` of 2 against a one-word query.
    """
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, ["quick", "quick"], 1)) == {1, 2, 3}
    assert COLUMN.matches(2, ["quick", "quick"], 2) is False


def test_a_minimum_above_the_distinct_count_is_unsatisfiable():
    """"Four of two words" matches nothing — and is **not** collapsed to the conjunction.

    Working: ["quick", "fox"] has two distinct tokens; a `minimum` of 3 can never be met, so the
    answer is empty. Collapsing to the conjunction would answer {1, 3} instead. This was a defect
    in this module until 2026-08-14, which is why it has a case of its own.

    Kills: `need = min(minimum, len(wanted))`; the guard removed altogether.
    """
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, ["quick", "fox"], 3)) == set()
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, ["quick", "fox"], 2)) == {1, 3}


def test_m_of_n_admits_the_partial_holder():
    """The m-of-n case proper: `minimum` below the query's size.

    Working: the query ["quick", "fox", "hare"] has three distinct tokens and no entity holds
    "hare". At `minimum=2`, entities 1 and 3 hold two of the three and match; entity 2 holds one
    and does not.

    Kills: a `>=` written `>` (entity 1 and 3 would drop out); a `minimum` compared against the
    held stream's length rather than against the number of query tokens found.
    """
    query = ["quick", "fox", "hare"]
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, query, 2)) == {1, 3}
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, query, 1)) == {1, 2, 3}
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, query, 3)) == set()


def test_an_empty_query_matches_nothing():
    """An empty query is the `any_of: []` reading — it matches nothing, never everything.

    Kills: an empty query falling through to `0 >= 0` and matching every entity, which is a
    filter that stops filtering when its needle is dropped.
    """
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, [], None)) == set()
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, [], 1)) == set()


def test_an_entity_with_no_value_matches_no_text_predicate():
    """An entity absent from `tokens` carries no value and matches nothing (records §10).

    Working: entity 9 is not in the column at all, which is different from entity 4, which is
    present with an empty stream. Neither holds "quick", so neither satisfies a query for it,
    under any `minimum` the wire can carry (`minimum_should_match` is refused below 1 by
    `filter_dto.rs`, so 0 is not a reachable input and is asserted nowhere here).

    Kills: an absent entity treated as an empty stream; a `held is None` test written as a
    truthiness test, which would fold entity 4's empty stream into the absent case.
    """
    assert COLUMN.matches(9, ["quick"], None) is False
    assert COLUMN.matches(9, ["quick"], 1) is False
    assert COLUMN.matches(4, ["quick"], 1) is False
    assert COLUMN.carriers(lambda e: True) == {1, 2, 3, 4}


# -- phrase: order and adjacency ------------------------------------------------------------------


def test_a_phrase_is_ordered_and_adjacent():
    """`phrase` needs the query's tokens in this order and next to each other (records §10).

    Working: ["quick", "brown"] appears adjacent in entity 1 ("the **quick brown** fox") and in
    entity 2 ("quick **quick brown**"). Entity 3 holds both words in the order "brown … quick",
    so it matches `match` and not `phrase` — which is the whole difference between the two
    operands.

    Kills: `phrase` implemented as a set test (entity 3 would join); an unordered window.
    """
    assert COLUMN.carriers(lambda e: COLUMN.has_phrase(e, ["quick", "brown"])) == {1, 2}
    assert COLUMN.carriers(lambda e: COLUMN.matches(e, ["quick", "brown"], None)) == {1, 2, 3}


def test_a_phrase_must_be_contiguous():
    """Tokens either side of a gap are not a phrase.

    Working: entity 1 is "the quick brown fox", so ["the", "brown"] is present in order with one
    token between them and is **not** a phrase; ["the", "quick"] is.

    Kills: a subsequence test in place of a window test.
    """
    assert COLUMN.has_phrase(1, ["the", "brown"]) is False
    assert COLUMN.has_phrase(1, ["the", "quick"]) is True


def test_a_repeated_word_is_significant_in_a_phrase():
    """Repetition counts on both sides, unlike `match`.

    Working: entity 2's stream is "quick quick brown", so the phrase ["quick", "quick"] is
    present there and nowhere else — entity 1 holds "quick" once.

    Kills: de-duplicating the query before the window walk, which would make this phrase match
    every entity holding "quick".
    """
    assert COLUMN.carriers(lambda e: COLUMN.has_phrase(e, ["quick", "quick"])) == {2}


def test_a_phrase_may_end_the_stream():
    """The window walk includes the last position — an off-by-one here loses every trailing hit.

    Working: entity 1's last two tokens are "brown fox", and its whole stream is a phrase of
    itself. Entity 3 ends "fox quick".

    Kills: `range(len(held) - n)` instead of `range(len(held) - n + 1)`.
    """
    assert COLUMN.has_phrase(1, ["brown", "fox"]) is True
    assert COLUMN.has_phrase(1, ["the", "quick", "brown", "fox"]) is True
    assert COLUMN.has_phrase(3, ["fox", "quick"]) is True


def test_a_phrase_longer_than_the_stream_matches_nothing():
    """A query with more tokens than the entity holds cannot appear in it.

    Working: entity 2 holds three tokens, so a four-token phrase is unsatisfiable there whatever
    the words are.

    Kills: a negative-range walk that silently returns `False` for the wrong reason, or an index
    error.
    """
    assert COLUMN.has_phrase(2, ["quick", "quick", "brown", "fox"]) is False
    assert COLUMN.has_phrase(4, ["quick"]) is False


def test_an_empty_phrase_matches_nothing():
    """An empty query is not a phrase every document contains.

    Kills: an empty query answering `True` for every entity — `held[i:i] == []` is true at every
    position, so this is what the explicit guard is for.
    """
    assert COLUMN.carriers(lambda e: COLUMN.has_phrase(e, [])) == set()
    assert COLUMN.has_phrase(9, []) is False


def test_carriers_is_the_brute_force_set_over_the_column():
    """`carriers` walks every entity the column holds — including the empty-stream one.

    Working: a predicate that is true of everything answers all four fixture entities, entity 4
    included; the engine's answer to the same predicate must equal this set.

    Kills: `carriers` skipping entities with an empty token list, which would quietly narrow
    every comparison the differential makes.
    """
    assert COLUMN.carriers(lambda e: True) == {1, 2, 3, 4}
