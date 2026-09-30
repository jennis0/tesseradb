"""The access-expression oracle (`oracle.access`), pinned case by case."""

from __future__ import annotations

import pytest

from oracle.access import And, Or, Public, Refused, Term, parse, satisfies, terms


def holding(*names):
    return lambda name: name in names


@pytest.mark.parametrize(
    "text, tree",
    [
        ("user:a/b-c.d_E9", Term("user:a/b-c.d_E9")),
        ('"a\\"b\\\\c"', Term('a"b\\c')),
        ('" team a "', Term(" team a ")),
        ('"  "', Term("  ")),
        ('"a "', Term("a ")),
        (" \t(a) ", Term("a")),
        ("a&(b|c)", And((Term("a"), Or((Term("b"), Term("c")))))),
        (" public ", Public()),
        ("publicly", Term("publicly")),
        ("inheritance", Term("inheritance")),
    ],
)
def test_what_a_label_reads_as(text, tree):
    assert parse(text) == tree


@pytest.mark.parametrize(
    "text",
    [
        "", "  ", "a&b|c", "a|b&c", "a&", "&a", "a b", "(a", "a)", "()", '""', '"a',
        '"a\\n"', "!a", "a&!b", "é", "a,b", '"a\tb"', '"\x07"', "a\x1c", "public&a", "PUBLIC",
        '"public"', "(public)", "a|Public", "inherited", " Inherited ", "INHERITED",
        '"inherited"', "(inherited)", "a&inherited", "a|(b&InHeRiTeD)", "a & b", "a |b",
        "( a|b)", "(a|b )", "a&( b)", '"a" &b',
    ],
)
def test_what_is_refused(text):
    with pytest.raises(Refused):
        parse(text)


def test_nesting_is_bounded_at_256():
    parse("(" * 256 + "a" + ")" * 256)
    with pytest.raises(Refused):
        parse("(" * 257 + "a" + ")" * 257)


@pytest.mark.parametrize(
    "text, held, expected",
    [
        ("secret&(team_a|team_b)", ("secret", "team_b"), True),
        ("secret&(team_a|team_b)", ("secret",), False),
        ("(s&e)|(a&b)", ("a", "b"), True),
        ("(s&e)|(a&b)", ("s", "b"), False),
        ('"team a"|x', ("team a",), True),
        ("public", (), True),
    ],
)
def test_satisfies(text, held, expected):
    assert satisfies(parse(text), holding(*held)) is expected


def test_terms_are_those_named():
    assert terms(parse('a&(b|"c d")&a')) == {"a", "b", "c d"}
    assert terms(parse("public")) == set()


def test_a_space_inside_quotes_makes_another_term():
    assert parse('"a "') != parse("a")
    assert not satisfies(parse('"a "'), holding("a"))
