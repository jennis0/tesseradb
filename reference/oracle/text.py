"""The `text` family's oracle: `match`, m-of-n and `phrase` from the fixture's own prose.

**This holds no dictionary, no postings and no ordinals**, exactly as `oracle.filters.KeywordColumn`
holds no dictionary — and the absence is the whole of what makes the text differential a
differential (`records-and-search.md` §10). The engine stores a per-layer sorted token dictionary
and one posting per term, resolves each query token inside each layer, intersects the postings with
the candidate as it reads them, and unions the layers; this module holds the *strings the fixture
planted*, tokenises them, and walks. So the two sides agree only if the build segmented, sorted,
front-coded, encoded and resolved correctly at every layer. An implementation that wrote one
dictionary and then answered consistently by its own wrong ordinals disagrees here rather than
being agreed with.

**One analyser, reached over the CLI, and that is the load-bearing design decision.**
Reimplementing UAX #29 segmentation in Python would compare PyICU's ICU4C against the engine's
icu4x — two implementations of one standard, which disagree at the margins, so every disagreement
would be a research question rather than a defect. `tessera tokenise` exists for exactly this
(decision 0070): the oracle asks the shipped binary for the token stream and then does its own
*set and sequence* arithmetic over it, which is the part under test.

The subprocess is called **once per corpus**, not once per string: the verb takes one input per
line on stdin and answers one tab-separated token line per input, so 150,000 documents cost one
process. A per-string call would make a whole-corpus sweep take hours.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path


def tokenise(texts: list[str], binary: Path, analyser: str = "unicode") -> list[list[str]]:
    """Every input's token list, in order, through the shipped analyser.

    **Two transports, chosen by what the inputs contain, and both are the same verb.** Stdin carries
    one input per line, which is what makes a 150,000-document corpus one subprocess instead of
    150,000 — and which cannot carry an input containing a newline or a tab, the framing being
    exactly those two characters. The argument form (`--text`, repeatable) has no framing at all
    and is used whenever an input needs it; it costs one argv entry per input, so it is the
    fallback rather than the default.

    The **output** framing is safe either way: a token can contain neither character, the segmenter
    breaking on both, so the tab-joined line is unambiguous whatever the input was.
    """
    if not texts:
        return []
    framed = any("\n" in t or "\t" in t for t in texts)
    if framed:
        argv = [str(binary), "tokenise", "--analyser", analyser]
        for text in texts:
            argv += ["--text", text]
        proc = subprocess.run(argv, capture_output=True, text=True, check=True)
    else:
        proc = subprocess.run(
            [str(binary), "tokenise", "--analyser", analyser],
            input="\n".join(texts) + "\n",
            capture_output=True,
            text=True,
            check=True,
        )
    lines = proc.stdout.split("\n")
    # The verb writes one line per input and a trailing newline, so the split leaves one empty
    # tail element. Anything else means the two sides have stopped agreeing about the framing.
    if lines and lines[-1] == "":
        lines.pop()
    if len(lines) != len(texts):
        raise AssertionError(
            f"`tessera tokenise` answered {len(lines)} lines for {len(texts)} inputs — the "
            "oracle's token stream is no longer aligned with its documents"
        )
    # An input analysing to no tokens at all — punctuation, the empty string — is an empty line,
    # which splits to `[""]` and must become `[]` rather than a one-token list of the empty string.
    return [ln.split("\t") if ln else [] for ln in lines]


def identity(binary: Path, analyser: str = "unicode") -> str:
    """The analyser's full `<name>/<version>` identity — what a column records in the manifest."""
    proc = subprocess.run(
        [str(binary), "tokenise", "--analyser", analyser, "--identity"],
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


@dataclass
class TextColumn:
    """One `text` column as the fixture planted it, with its documents already tokenised.

    `tokens[entity]` is that entity's token list, in document order with duplicates intact — the
    engine's own `Analyser::tokens` contract, and both properties are needed here: order for
    `phrase`, duplicates because a phrase may repeat a word.

    An entity absent from `tokens` **carries no value**, which is absence from the layer's presence
    and matches no predicate — the same rule every other family's oracle keeps.
    """

    tokens: dict[int, list[str]] = field(default_factory=dict)

    @classmethod
    def from_prose(
        cls, prose: dict[int, str], binary: Path, analyser: str = "unicode"
    ) -> TextColumn:
        """Tokenise a whole corpus in one subprocess. Entity order is preserved by the key list."""
        entities = sorted(prose)
        streams = tokenise([prose[e] for e in entities], binary, analyser)
        return cls(tokens=dict(zip(entities, streams, strict=True)))

    def matches(self, entity: int, query_tokens: list[str], minimum: int | None) -> bool:
        """`match`: at least `minimum` of the query's **distinct** tokens appear anywhere.

        `minimum` of `None` is *all of them*, which is the plain conjunction. A `minimum` above the
        distinct count is unsatisfiable — nothing can carry four of two words — and is answered
        `False` rather than collapsed to the conjunction, which is the engine's rule and was a
        defect until 2026-08-14.
        """
        held = self.tokens.get(entity)
        if held is None:
            return False
        wanted = sorted(set(query_tokens))
        if not wanted:
            # An empty query matches nothing, which is the reading `any_of: []` takes.
            return False
        need = len(wanted) if minimum is None else minimum
        if need > len(wanted):
            return False
        present = set(held)
        return sum(1 for w in wanted if w in present) >= need

    def has_phrase(self, entity: int, query_tokens: list[str]) -> bool:
        """`phrase`: the query's tokens appear **in this order and adjacent**.

        Order and repetition are significant on both sides, unlike `match` — that is the whole
        difference between the two operands, and the reason this walks a window rather than a set.
        """
        held = self.tokens.get(entity)
        if held is None or not query_tokens or len(query_tokens) > len(held):
            return False
        n = len(query_tokens)
        return any(held[i : i + n] == query_tokens for i in range(len(held) - n + 1))

    def carriers(self, predicate) -> set[int]:
        """Every entity the predicate holds for — the brute-force set the engine must equal."""
        return {e for e in self.tokens if predicate(e)}
