"""Access expressions: reading a label and deciding whether a set of terms satisfies it.

A label is an Accumulo visibility expression without negation, or `public`. A term is written bare
when it is made of ASCII letters, digits and `_ - . : /`, and otherwise in double quotes, where
`\\"` and `\\\\` are the only escapes. `&` is conjunction and `|` is disjunction, and one bracket may
not mix them. Whitespace around a term, an operator or a bracket is ignored, and a quoted term is
trimmed.

A label is refused when it is empty, when brackets nest more than 256 deep, when a term holds a
control character, and when a term equals `public` ignoring ASCII case. `public` is accepted only as
the whole label, and every principal satisfies it.

This module evaluates the tree as written, by direct recursion. It does not normalise, share
subexpressions or number terms, so agreeing with the engine's label DAG is evidence about both.
The engine's limit on the size of a label is defined on its normalised form, and is not modelled.
"""

from __future__ import annotations

import unicodedata
from dataclasses import dataclass
from typing import Callable

PUBLIC = "public"
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
    if not text.strip(WHITESPACE):
        raise Refused("empty")
    if text.strip(WHITESPACE) == PUBLIC:
        return Public()
    reader = _Reader(text)
    tree = reader.expression(0)
    reader.skip_space()
    if reader.at < len(text):
        raise Refused(f"unexpected {text[reader.at]!r} at {reader.at}")
    return tree


def satisfies(tree, held: Callable[[str], bool]) -> bool:
    """Whether a principal holding the terms `held` accepts satisfies the label `tree`."""
    if isinstance(tree, Public):
        return True
    if isinstance(tree, Term):
        return held(tree.name)
    if isinstance(tree, And):
        return all(satisfies(operand, held) for operand in tree.operands)
    return any(satisfies(operand, held) for operand in tree.operands)


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
        return self.text[self.at] if self.at < len(self.text) else None

    def skip_space(self) -> None:
        while self.peek() is not None and self.peek() in WHITESPACE:
            self.at += 1

    def expression(self, depth: int):
        if depth > MAX_DEPTH:
            raise Refused("brackets nest too deeply")
        operands = [self.operand(depth)]
        operator = None
        while True:
            self.skip_space()
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
        self.skip_space()
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
            while self.peek() is not None and self.peek() in BARE:
                self.at += 1
            return _term(self.text[start:self.at])
        raise Refused(f"expected a term at {self.at}")

    def quoted(self) -> str:
        self.at += 1
        name = []
        while (c := self.peek()) != '"':
            if c is None:
                raise Refused("unclosed quote")
            if c == "\\":
                self.at += 1
                if self.peek() not in ('"', "\\"):
                    raise Refused(f"bad escape at {self.at}")
                c = self.peek()
            name.append(c)
            self.at += 1
        self.at += 1
        return "".join(name).strip(WHITESPACE)


def _term(name: str) -> Term:
    if not name:
        raise Refused("empty term")
    if any(unicodedata.category(c) == "Cc" for c in name):
        raise Refused(f"control character in {name!r}")
    if name.lower() == PUBLIC and name.isascii():
        raise Refused(f"{name!r} is reserved")
    return Term(name)
