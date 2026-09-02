"""The `GET /v1/categories/{column}/suggest` fixture and its Python fold oracle.

`docs/design/value-suggestion.md` §4 declares the fold (NFKC, then full case folding, then
whitespace collapse) and the entry construction (key, title, word starts after the first) as a
fixed rule, factored in Rust as `tessera_analyse::{Fold, SuggestionFold}` and applied identically
to a typed query and to every indexed string. This module is the independent second
implementation §4 calls for: it re-derives the same rule from the design text, in Python, over a
small dedicated corpus built backwards from the rule's own edge cases — mixed-case, extra
whitespace, full-width forms (NFKC), a leading non-word run, and an underscore-joined key — rather
than the mask catalogue, whose planted vocabulary values (`alpha`, `beta`, …) are single words with
no titles and so cannot exercise the title or word-start entries at all.

**Deliberately not the mask catalogue** — reused for the same reason `oracle.label_fixture` is
not: the catalogue's `department`/`archive` columns are the right shape for the filter
differential and the wrong shape for this one. A dedicated corpus is also what makes "this string
folds to that entry" a fact a reader can check by reading, which is `label_fixture`'s argument
applied to matching rather than to containment.

**No bundle join is needed.** Every other differential in this suite keys its oracle by *entity*,
because it has to compare *served items* against the fixture's own identity (`fx_key`). A
suggestion response carries no item — only value codes, keys, titles and match spans — so
"is this value visible to this principal" is a fact about the corpus's own **source**-id space,
invariant under whatever permutation the build applies to reach entity space. The oracle here
works entirely in source-id space and never opens the built bundle.

**The fold: Python versus Rust, and where they may legitimately diverge.** `unicodedata.normalize
("NFKC", …)` and `str.casefold()` are CPython's own tables (Unicode Character Database /
`CaseFolding.txt`, full mappings), where the Rust side is `icu4x`'s pinned data
(`tessera_analyse::UNICODE_VERSION`). Full case folding is locale-independent by construction, so
the two are expected to agree on every code point both Unicode Character Database revisions assign
— the one legitimate divergence is a code point one Unicode version knows and the other does not
(a very recent script), which this fixture does not touch. `str.casefold()` is Python's own
implementation of the same *algorithm* `CaseMapper::fold_string` runs, not a reimplementation
prone to disagree on ordinary text; a test that found the two disagreeing on anything in this
fixture would be finding a real bug, not this documented gap.
"""

from __future__ import annotations

import subprocess
import unicodedata
from dataclasses import dataclass
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from oracle.harness import CLI_BIN, REPO_ROOT, build_env, ensure_cli_built, write_deployment

# ---------------------------------------------------------------------------------------------
# The fold — the Python oracle's own implementation of value-suggestion.md §4
# ---------------------------------------------------------------------------------------------


def nfkc_casefold(text: str) -> str:
    """NFKC, then full case folding — `Fold::fold`'s own two stages, in that order."""
    return unicodedata.normalize("NFKC", text).casefold()


def fold_entry(text: str) -> str:
    """`SuggestionFold::entry`: the fold, then whitespace runs collapsed to one space and
    trimmed."""
    return " ".join(nfkc_casefold(text).split())


def _wordish(c: str) -> bool:
    """`SuggestionFold::wordish`: `Alphabetic ∪ Number ∪ Mark`, approximated by the Unicode
    general-category groups `L*` (letter), `N*` (number) and `M*` (mark) — the same union the
    design names, read off `unicodedata.category` rather than icu4x's property tables."""
    return unicodedata.category(c)[0] in ("L", "N", "M")


def _word_starts(text: str) -> list[int]:
    """Character offsets of every word start **after the first** — the first word is the
    whole-string entry, not a word-start entry (`SuggestionFold::word_starts` /
    `::served_word_starts`, unified here: Python indexes by code point throughout, so the
    byte-offset/character-offset split the Rust side carries has nothing to mirror)."""
    out: list[int] = []
    previous_wordish = False
    leading = True
    for i, c in enumerate(text):
        wordish = _wordish(c)
        if wordish and not previous_wordish:
            if leading:
                leading = False
            else:
                out.append(i)
        previous_wordish = wordish
    return out


def _served_start(text: str) -> int:
    """The character index the whole-string entry starts at — leading whitespace, which the fold
    trims (`SuggestionFold::served_start`)."""
    for i, c in enumerate(text):
        if not c.isspace():
            return i
    return 0


def match_len(served: str, start: int, q_folded: str) -> int:
    """`match_len` (`crates/tessera-engine/src/suggest.rs`): re-fold `served` forward from `start`,
    one character at a time, until the fold has `q_folded` as a prefix — the number of characters
    consumed is `match.len`. An empty query consumes nothing. Mirrors the Rust function exactly;
    Python indexes strings by code point throughout, so there is no byte/character split to carry
    across the port."""
    if not q_folded:
        return 0
    if start >= len(served):
        return 0
    tail = served[start:]
    characters = 0
    for end in range(1, len(tail) + 1):
        characters = end
        if fold_entry(tail[:end]).startswith(q_folded):
            return characters
    return characters


KIND_KEY = 0
KIND_TITLE = 1
KIND_WORD_START = 2


@dataclass(frozen=True)
class Entry:
    entry: str
    kind: int
    field: str  # "key" or "title"
    start: int


def entries_of(key: str, title: str | None) -> list[Entry]:
    """`SuggestionFold::entries_of` (`value-suggestion.md` §4): the whole key, the whole title
    where one exists, and every word start after the first of the title — or of the key, where
    there is no title.

    An entry that folds to the empty string is dropped; word starts are dropped whole for a value
    whose folded and served boundary counts disagree (not reachable by this fixture's planted
    strings, so the branch is here for parity with the Rust rule rather than because a case
    exercises it).
    """
    out: list[Entry] = []

    key_entry = fold_entry(key)
    if key_entry:
        out.append(Entry(key_entry, KIND_KEY, "key", _served_start(key)))

    if title is not None:
        title_entry = fold_entry(title)
        if title_entry:
            out.append(Entry(title_entry, KIND_TITLE, "title", _served_start(title)))
        word_source, field = title, "title"
    else:
        word_source, field = key, "key"

    folded = fold_entry(word_source)
    folded_starts = _word_starts(folded)
    served_starts = _word_starts(word_source)
    if len(folded_starts) == len(served_starts):
        for at, served in zip(folded_starts, served_starts):
            out.append(Entry(folded[at:], KIND_WORD_START, field, served))

    return out


# ---------------------------------------------------------------------------------------------
# The corpus
# ---------------------------------------------------------------------------------------------

N_ITEMS = 240
VIEW_ID = "s0"
EXTENT_MAX = 65536.0
SEED = 20260902

#: The one entity carrying `solo` — the positive control that finds a single visible member.
SOLO_ID = 0
#: The range carrying `omega` — visible only to a principal holding `OMEGA_TERM`, so the value is
#: hidden to any principal that does not (C11: a hidden value must answer as if absent).
OMEGA_LO, OMEGA_HI = 200, 210
OMEGA_TERM = 42

#: `topic` — the `derived` vocabulary, planted to exercise every entry kind and every fold rule
#: §4 names: extra internal whitespace, mixed case, full-width forms (NFKC), a leading
#: non-word run (the "first word" exclusion applies from the first *wordish* transition, not from
#: offset 0), and an underscore-joined key with no title (word starts still fire on the
#: word/non-word boundary the underscore makes).
TOPIC_VALUES: dict[str, tuple[int, str | None]] = {
    "ml": (1, "Machine   Learning"),
    "stat": (2, "Statistical  Machine Learning"),
    "fw": (3, "ＦULL Ｗidth"),  # fullwidth F, W — NFKC folds to ASCII "full width"
    "paren": (4, "(draft) Machine Learning"),
    "multi_word": (5, None),
    "solo": (6, "Solo Entry"),
    "omega": (7, None),
    # `void` is declared and planted on no entity anywhere — the hollow value, C11's other control.
    "void": (8, None),
}

#: `archive` — the `public` counterpart: served as authored to every principal, gated on nothing.
ARCHIVE_VALUES: dict[str, tuple[int, str | None]] = {
    "red": (11, "Red Team"),
    "blue": (12, None),
    # Declared, planted nowhere — public and hollow both, unlike `topic`'s `void`.
    "unused": (13, None),
}

_TOPIC_CYCLE = ["ml", "stat", "fw", "paren", "multi_word"]


def topic_of(source_id: int) -> str | None:
    """`topic` as planted — a pure function of the source id, exactly as the mask catalogue's own
    generation functions are (`oracle/catalogue.py`'s `department_of` docstring: what the entity
    was *given*, upstream of anything the build derives from it)."""
    if source_id == SOLO_ID:
        return "solo"
    if OMEGA_LO <= source_id < OMEGA_HI:
        return "omega"
    if source_id % 6 == 0:
        return None
    return _TOPIC_CYCLE[source_id % len(_TOPIC_CYCLE)]


def archive_of(source_id: int) -> str | None:
    """`archive` as planted — decorrelated from `topic_of`'s own cycle (period 6) by a period-7
    stride, on `oracle/catalogue.py`'s argument: a correlated planting would make every
    cross-column assertion pass for the wrong reason."""
    if source_id % 7 == 0:
        return None
    return ["red", "blue"][source_id % 2]


def omega_visible_source_ids() -> set[int]:
    return set(range(OMEGA_LO, OMEGA_HI))


def terms_of(source_id: int) -> list[int]:
    """Which `point_visibility` term a source carries — exactly one each, so there is no
    conjunctive/disjunctive question to get wrong across a multi-term row. **Every** source carries
    a term (`default = "public"` names an actual term string in this relation's own space rather
    than an unconditional "no gate": a principal holding no terms at all sees nothing at all,
    exactly `oracle/catalogue.py`'s `MaskCase(name="empty", grants=())` — "the zero-visibility
    principal", not the widest one). `omega`'s range carries `OMEGA_TERM` in place of `BASE_TERM`,
    so a principal short of `OMEGA_TERM` loses exactly `omega`'s members and nothing else."""
    if OMEGA_LO <= source_id < OMEGA_HI:
        return [OMEGA_TERM]
    return [BASE_TERM]


def visible_sources(grants: list[str]) -> set[int]:
    """The source ids a principal holding `grants` (decimal term strings) can see — computed from
    the planting rule, never from the built bundle. A point is visible iff the principal holds
    every term the point's own row lists (conjunctive point visibility, as `pairs` rows compose)."""
    held = {int(t) for t in grants}
    return {s for s in range(N_ITEMS) if set(terms_of(s)) <= held}


#: Principals used across the differential. Both hold `BASE_TERM`, so everything but `omega` is
#: visible to each; `NARROW` never holds `OMEGA_TERM`, so `omega` is hidden to it, and `WIDE`
#: holds it, so `omega` is visible. That is the pair's whole difference, which is what makes it a
#: clean C11 control rather than two principals differing by more than one value.
BASE_TERM = 1
NARROW_GRANTS: list[str] = [str(BASE_TERM)]
WIDE_GRANTS: list[str] = [str(BASE_TERM), str(OMEGA_TERM)]


def members_of(planted: dict[int, str], key: str) -> set[int]:
    return {s for s, v in planted.items() if v == key}


def planted_topic() -> dict[int, str]:
    return {s: v for s in range(N_ITEMS) if (v := topic_of(s)) is not None}


def planted_archive() -> dict[int, str]:
    return {s: v for s in range(N_ITEMS) if (v := archive_of(s)) is not None}


# ---------------------------------------------------------------------------------------------
# The oracle sweep: the served set §7's order gives, over a visible candidate
# ---------------------------------------------------------------------------------------------


def suggest_oracle(
    values: dict[str, tuple[int, str | None]],
    planted: dict[int, str],
    candidate: set[int] | None,
    q: str,
    limit: int,
    walk_budget: int | None = None,
) -> tuple[list[dict], bool]:
    """The oracle's own answer to one `suggest` request.

    `candidate` is `None` for a `public` column (every value is served as authored, §3) and a set
    of visible source ids for a `derived` one — `members(v) ∩ candidate ≠ ∅` decides visibility,
    exactly `Engine::suggest`'s predicate. Ordering is §7's: ascending by folded entry string, ties
    by entry kind (key, title, word start), then by key; a value is served once, at its first
    matching entry in that order.

    `walk_budget`, where given, bounds how many **distinct values** the walk examines (§6.2's
    `admits`: one unit per gate test, a value already on the page costs nothing) — `more` is then
    `true` either because the page filled or because the budget ran out first, and this fixture is
    far too small to fill a page under any prefix without exhausting it, so a `limit` and a
    `walk_budget` chosen to interact are how the two causes are told apart in the test that wants
    to.
    """
    q_folded = fold_entry(q)

    rows: list[tuple[str, int, str, dict]] = []
    for key, (code, title) in values.items():
        for entry in entries_of(key, title):
            served_string = title if entry.field == "title" else key
            row = {
                "code": code,
                "key": key,
                "title": title,
                "match": {
                    "field": entry.field,
                    "start": entry.start,
                    "len": match_len(served_string, entry.start, q_folded),
                },
            }
            rows.append((entry.entry, entry.kind, key, row))
    rows.sort(key=lambda r: (r[0], r[1], r[2]))

    # Mirrors `WalkState::stop`/`::admits` exactly: `stop()` (page full, or the budget spent) is
    # checked before an entry is looked at; a value already emitted is then free to skip
    # (`admits`'s dedup, before its `examined` increment); only a value not yet decided is
    # actually probed and counted against the budget.
    served: list[dict] = []
    served_keys: set[str] = set()
    examined = 0
    more = False
    for entry_str, _kind, key, row in rows:
        if not entry_str.startswith(q_folded):
            continue
        if len(served) >= limit or (walk_budget is not None and examined >= walk_budget):
            more = True
            break
        if key in served_keys:
            continue
        examined += 1
        if candidate is not None:
            member_sources = members_of(planted, key)
            if not (member_sources & candidate):
                continue
        served_keys.add(key)
        served.append(row)
    return served, more


def counts_for(values: dict[str, tuple[int, str | None]], planted: dict[int, str], candidate: set[int]) -> dict[str, int]:
    return {key: len(members_of(planted, key) & candidate) for key in values}


# ---------------------------------------------------------------------------------------------
# Building the bundle
# ---------------------------------------------------------------------------------------------

CONFIG_TOML = f"""
[sources]
points        = "points.parquet"
pairs         = "pairs.parquet"
topicvalues   = "topicvalues.parquet"
archivevalues = "archivevalues.parquet"

[defaults]
source = "points"

[[view]]
name             = "{VIEW_ID}"
extent           = {{ x = [0.0, {EXTENT_MAX}], y = [0.0, {EXTENT_MAX}] }}
source           = "points"
point_visibility = {{ source = "pairs", default = "public" }}

[[vocabulary]]
name       = "topic"
width      = "u8"
value_set  = "closed"
visibility = "derived"
source     = "topicvalues"

[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
source     = "archivevalues"

[[attribute]]
name       = "topic"
type       = "category"
index      = true
vocabulary = "topic"

[[attribute]]
name       = "archive"
type       = "category"
index      = true
vocabulary = "archive"
"""


def _write_points(path: Path) -> None:
    import random

    rng = random.Random(SEED)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array(
                    [rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], type=pa.float32()
                ),
                "y": pa.array(
                    [rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], type=pa.float32()
                ),
                "topic": pa.array([topic_of(s) for s in range(N_ITEMS)], type=pa.string()),
                "archive": pa.array([archive_of(s) for s in range(N_ITEMS)], type=pa.string()),
            }
        ),
        path,
    )


def _write_pairs(path: Path) -> None:
    rows = [(s, t) for s in range(N_ITEMS) for t in terms_of(s)]
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([r[0] for r in rows], type=pa.uint64()),
                "term_id": pa.array([r[1] for r in rows], type=pa.uint32()),
            }
        ),
        path,
    )


def _write_vocabulary(path: Path, values: dict[str, tuple[int, str | None]]) -> None:
    keys = list(values)
    pq.write_table(
        pa.table(
            {
                "key": pa.array(keys, type=pa.string()),
                "code": pa.array([values[k][0] for k in keys], type=pa.uint32()),
                "title": pa.array([values[k][1] for k in keys], type=pa.string()),
            }
        ),
        path,
    )


def build_suggest_bundle(work_dir: Path) -> Path:
    """Write the corpus and its declaration under `work_dir`, build the bundle via the CLI, and
    hand back its root. Built fresh per test session — the corpus is 240 rows and two eight/three
    value vocabularies, so a build costs a fraction of a second and there is no reuse machinery
    to get wrong (contrast `oracle.harness.ensure_fixture_bundle`'s stamped-recipe machinery,
    which exists because that fixture is 150,000 rows)."""
    ensure_cli_built()
    work_dir.mkdir(parents=True, exist_ok=True)
    _write_points(work_dir / "points.parquet")
    _write_pairs(work_dir / "pairs.parquet")
    _write_vocabulary(work_dir / "topicvalues.parquet", TOPIC_VALUES)
    _write_vocabulary(work_dir / "archivevalues.parquet", ARCHIVE_VALUES)
    config = work_dir / "suggest.toml"
    config.write_text(CONFIG_TOML)

    bundle = work_dir / "bundle"
    deployment = write_deployment(work_dir / "tessera.toml", bundle=bundle, schema=config)
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle), "--mint-id-key"],
        cwd=REPO_ROOT,
        env=build_env(),
        check=True,
    )
    return bundle
