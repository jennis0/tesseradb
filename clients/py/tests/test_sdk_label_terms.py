"""The terms a database's default reader holds: every term its access labels name."""

from tesseradb._database import _label_terms


def test_the_default_reader_holds_every_term_a_label_names():
    assert _label_terms(['secret&(team_a|"team b")']) == ["secret", "team_a", "team b"]
    assert _label_terms(['"a\\"b"|c']) == ['a"b', "c"]
    assert _label_terms([" red ", "", None]) == ["red"]
    assert _label_terms("cs.LG") == ["cs.LG"]


def test_public_and_a_word_that_is_not_an_expression_are_kept_whole():
    assert _label_terms(["public"]) == ["public"]
    assert _label_terms(["inherited"]) == ["inherited"]
    assert _label_terms(["a b"]) == ["a b"]
