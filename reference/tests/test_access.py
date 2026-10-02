"""The access-expression oracle (`oracle.access`), pinned case by case."""

from __future__ import annotations

import pytest

from oracle.access import (
    And,
    Or,
    Public,
    Refused,
    Term,
    admits,
    card_labels,
    implies,
    held_term,
    parse,
    satisfies,
    terms,
    witness,
)


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


def test_a_list_of_labels_admits_a_principal_satisfying_any_one():
    assert admits(["x", "a&b"], holding("a", "b"))
    assert admits(["x", "a&b"], holding("x"))
    assert not admits(["x", "a&b"], holding("a"))
    assert not admits([], holding("a"))


@pytest.mark.parametrize(
    "term, held",
    [
        (" red ", "red"),
        ("team a", "team a"),
        ("", None),
        (" ", None),
        ("public", None),
        (" PuBlic ", None),
        ("a\x01b", None),
        ("\x00a&b", None),
    ],
)
def test_a_credentials_term_is_held_trimmed_and_never_public_or_a_control_character(term, held):
    assert held_term(term) == held


def everything(_term):
    return True


def test_a_witness_takes_the_satisfied_operand_with_fewest_terms_then_the_first():
    assert witness(parse("(t&c)|(s&(b|a))"), everything) == ["a", "s"]
    assert witness(parse("(a&b&c)|(d&e)"), everything) == ["d", "e"]
    assert witness(parse("(t&c)|(s&(b|a))"), holding("s", "b", "c")) == ["b", "s"]
    assert witness(parse("(t&c)|(s&(b|a))"), holding("s")) is None
    assert witness(parse("public"), holding()) == []


def test_a_card_serves_each_held_term_and_one_clause_of_each_satisfied_conjunction():
    labels = ["eu&(ir:legal|ir:new)", "ir:new|ir:secret", "x&y"]
    assert card_labels(labels, holding("eu", "ir:new", "ir:secret")) == [
        "eu&ir:new",
        "ir:new",
        "ir:secret",
    ]
    assert card_labels(labels, holding("ir:secret")) == ["ir:secret"]
    assert card_labels(labels, holding("x")) == []
    assert card_labels(["public", "a"], holding("a")) == ["a", "public"]
    assert card_labels(['s&"team b"', '"team b"'], holding("s", "team b")) == [
        '"team b"',
        's&"team b"',
    ]


def test_a_card_serves_a_clause_of_each_operand_of_the_labels_read_as_one_disjunction():
    everyone = holding("a", "b", "c", "d", "e", "s", "t")
    assert card_labels(["a|(b&c)"], everyone) == ["a", "b&c"]
    assert card_labels(["a", "b&c"], everyone) == ["a", "b&c"]
    assert card_labels(["(a&b&c)|(d&e)"], everyone) == ["a&b&c", "d&e"]
    assert card_labels(["(t&c)|(s&(b|a))"], everyone) == ["a&s", "c&t"]
    assert card_labels(["a", "a&b"], everyone) == ["a"]
    assert card_labels(["a|(a&b)"], everyone) == ["a"]
    assert card_labels(["(a|b)|(c&d)"], holding("b", "c")) == ["b"]


def test_implication_between_operands():
    assert implies(parse("a&b"), parse("a"))
    assert implies(parse("a&b"), parse("a&(b|c)"))
    assert not implies(parse("a&(b|c)"), parse("a&b"))
    assert implies(parse("(a|b)&c"), parse("c"))


def test_a_space_inside_quotes_makes_another_term():
    assert parse('"a "') != parse("a")
    assert not satisfies(parse('"a "'), holding("a"))
