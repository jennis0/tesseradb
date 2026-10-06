"""Access expressions: reading a label and deciding whether a set of terms satisfies it.

A label is an Accumulo visibility expression without negation, or `public`. A term is written bare
when it is made of ASCII letters, digits and `_ - . : /`, and otherwise in double quotes, where
`\\"` and `\\\\` are the only escapes. `&` is conjunction and `|` is disjunction, and one bracket may
not mix them. Whitespace outside quotes separates tokens and is otherwise ignored; two operands
with only whitespace between them are refused. A quoted term is the text between its quotes, inner
spaces included, with its escapes applied.

A label is refused when it is empty, when brackets nest more than 256 deep, when a term holds a
control character, when a quoted term starts or ends with whitespace, which no credential's
trimmed term can equal, and when a term equals `public` or `inherited` ignoring ASCII case.
`public` is accepted only as the whole label, and every principal satisfies it. `inherited` is
never a label.

This module evaluates the tree as written, by direct recursion. It does not normalise, share
subexpressions or number terms, so agreeing with the engine's label DAG is evidence about both.
The engine's limit on the size of a label holding a conjunction is not modelled. Two of the item
card's answers are not modelled either: one for a label written with a conjunction that
normalisation reduces to a disjunction of terms, such as `(a|b)&(a|b|c)`, and one for an item
where one operand of its disjunction implies another without holding the other's conjuncts, such
as `a&b` beside `a&(b|c)`.
"""

from __future__ import annotations

import unicodedata
from dataclasses import dataclass
from typing import Callable

PUBLIC = "public"
INHERITED = "inherited"
MAX_DEPTH = 256
# Unicode's White_Space property, which trimming removes. `str.isspace` also accepts U+001C to
# U+001F, which are control characters here.
WHITESPACE = "\t\n\v\f\r \x85\xa0\u1680" + "".join(map(chr, range(0x2000, 0x200B))) + (
    "\u2028\u2029\u202f\u205f\u3000"
)
BARE = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-.:/")


class Refused(ValueError):
    """The text is not a label."""


@dataclass(frozen=True)
class Term:
    name: str


@dataclass(frozen=True)
class And:
    operands: tuple


@dataclass(frozen=True)
class Or:
    operands: tuple


@dataclass(frozen=True)
class Public:
    pass


def parse(text: str):
    """The label `text` writes, as a tree of `Term`, `And` and `Or`, or `Public()`."""
    body = text.strip(WHITESPACE)
    if not body:
        raise Refused("empty")
    if body == PUBLIC:
        return Public()
    reader = _Reader(body)
    tree = reader.expression(0)
    if reader.at < len(body):
        raise Refused(f"unexpected {body[reader.at]!r} at {reader.at}")
    return tree


def satisfies(tree, held: Callable[[str], bool]) -> bool:
    """Whether a principal holding the terms `held` satisfies the label `tree`."""
    if isinstance(tree, Public):
        return True
    if isinstance(tree, Term):
        return held(tree.name)
    if isinstance(tree, And):
        return all(satisfies(operand, held) for operand in tree.operands)
    return any(satisfies(operand, held) for operand in tree.operands)


def admits(labels, held: Callable[[str], bool]) -> bool:
    """Whether a principal holding the terms `held` satisfies any one of the labels `labels`, as an
    item, a view or an artifact carrying a list of labels admits it."""
    return any(satisfies(parse(label), held) for label in labels)


def witness(tree, held: Callable[[str], bool]) -> list[str] | None:
    """Held terms whose conjunction satisfies `tree`, sorted and distinct, or `None` where `held`
    does not satisfy it. Of a disjunction, the satisfied operand with fewest terms, then the first
    in code point order, is taken."""
    if isinstance(tree, Public):
        return []
    if isinstance(tree, Term):
        return [tree.name] if held(tree.name) else None
    found = [witness(operand, held) for operand in tree.operands]
    if isinstance(tree, And):
        if any(w is None for w in found):
            return None
        return sorted(set().union(*found))
    satisfied = [w for w in found if w is not None]
    return min(satisfied, key=lambda w: (len(w), w)) if satisfied else None


def write_term(name: str) -> str:
    """A term as label text: bare where every character may stand bare, and quoted otherwise."""
    if all(c in BARE for c in name):
        return name
    return '"' + name.replace("\\", "\\\\").replace('"', '\\"') + '"'


def card_labels(labels, held: Callable[[str], bool]) -> list[str]:
    """What an item card serves for an item carrying `labels`, to a principal holding `held`.

    The labels other than `public` are read as one disjunction, and its operands are each a term or
    a conjunction (`operands`). An operand that implies another, and is not implied by it, adds
    nothing to what the item admits and is dropped. Of the rest, each held term is served, and of
    each conjunction that `held` satisfies, its witness with its terms joined by `&`. `public` is
    served where a label is `public`. Each entry is label text, and the list is sorted and
    distinct."""
    out = set()
    disjuncts = []
    for text in labels:
        tree = parse(text)
        if isinstance(tree, Public):
            out.add(PUBLIC)
        else:
            disjuncts.extend(operands(tree))
    for i, x in enumerate(disjuncts):
        if any(
            implies(x, y) and not implies(y, x) for j, y in enumerate(disjuncts) if j != i
        ):
            continue
        if (w := witness(x, held)) is not None:
            out.add("&".join(write_term(t) for t in w))
    return sorted(out)


def operands(tree) -> list:
    """The operands of `tree` read as a disjunction: a disjunction's operands, with any
    disjunction among them read the same way, or `tree` itself."""
    if isinstance(tree, Or):
        return [o for operand in tree.operands for o in operands(operand)]
    return [tree]


def implies(x, y) -> bool:
    """Whether every principal satisfying `x` satisfies `y`. The expressions have no negation, so
    it is enough that each least set of terms satisfying `x` satisfies `y`."""
    return all(satisfies(y, clause.__contains__) for clause in _clauses(x))


def _clauses(tree) -> list[frozenset]:
    """Sets of terms, each of which satisfies `tree`, such that every set satisfying `tree` holds
    one of them."""
    if isinstance(tree, Term):
        return [frozenset([tree.name])]
    if isinstance(tree, Or):
        return [c for operand in tree.operands for c in _clauses(operand)]
    out = [frozenset()]
    for operand in tree.operands:
        out = [a | b for a in out for b in _clauses(operand)]
    return out


def held_term(term: str) -> str | None:
    """A term a credential presents, as it is held: trimmed, and `None` where nothing is left,
    where it holds a control character, or where it is `public` in any case. Every principal holds
    `public` without presenting it."""
    name = term.strip(WHITESPACE)
    if not name or any(unicodedata.category(c) == "Cc" for c in name):
        return None
    if name.isascii() and name.lower() == PUBLIC:
        return None
    return name


def terms(tree) -> set[str]:
    """Every term the label names."""
    if isinstance(tree, Term):
        return {tree.name}
    if isinstance(tree, (And, Or)):
        return set().union(*(terms(operand) for operand in tree.operands))
    return set()


class _Reader:
    def __init__(self, text: str):
        self.text = text
        self.at = 0

    def peek(self) -> str | None:
        """The next character outside whitespace, moving past the whitespace before it."""
        while self.at < len(self.text) and self.text[self.at] in WHITESPACE:
            self.at += 1
        return self.peek_raw()

    def peek_raw(self) -> str | None:
        return self.text[self.at] if self.at < len(self.text) else None

    def expression(self, depth: int):
        if depth > MAX_DEPTH:
            raise Refused("brackets nest too deeply")
        operands = [self.operand(depth)]
        operator = None
        while True:
            c = self.peek()
            if c is None or c == ")":
                break
            if c not in "&|":
                raise Refused(f"expected & or | at {self.at}")
            if operator is not None and c != operator:
                raise Refused(f"& and | mixed at {self.at}")
            operator = c
            self.at += 1
            operands.append(self.operand(depth))
        if operator is None:
            return operands[0]
        return (And if operator == "&" else Or)(tuple(operands))

    def operand(self, depth: int):
        c = self.peek()
        if c == "(":
            self.at += 1
            tree = self.expression(depth + 1)
            if self.peek() != ")":
                raise Refused(f"unclosed bracket at {self.at}")
            self.at += 1
            return tree
        if c == '"':
            return _term(self.quoted())
        if c is not None and c in BARE:
            start = self.at
            while self.peek_raw() is not None and self.peek_raw() in BARE:
                self.at += 1
            return _term(self.text[start:self.at])
        raise Refused(f"expected a term at {self.at}")

    def quoted(self) -> str:
        self.at += 1
        name = []
        while (c := self.peek_raw()) != '"':
            if c is None:
                raise Refused("unclosed quote")
            if c == "\\":
                self.at += 1
                if self.peek_raw() not in ('"', "\\"):
                    raise Refused(f"bad escape at {self.at}")
                c = self.peek_raw()
            name.append(c)
            self.at += 1
        self.at += 1
        return "".join(name)


def _term(name: str) -> Term:
    if not name:
        raise Refused("empty term")
    if any(unicodedata.category(c) == "Cc" for c in name):
        raise Refused(f"control character in {name!r}")
    if name.strip(WHITESPACE) != name:
        raise Refused(f"whitespace at an end of {name!r}")
    if name.isascii() and name.lower() in (PUBLIC, INHERITED):
        raise Refused(f"{name!r} is reserved")
    return Term(name)
