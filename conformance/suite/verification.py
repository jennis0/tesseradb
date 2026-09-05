"""Total verification — correctness-suite §9 over the generated corpus, build-order row 6.

Every served value is checked against ground truth computed from `tessera-corpus`, at the identity
the served row itself names. Two halves, because they are different claims (§9): the **row half**
takes every row of every recorded response, reads its `fx_key`, evaluates the generator, and
compares the Morton code, both quantised coordinates and every other declared field exactly — no
sampling, no probes. The **census half** exists because the row half is structurally blind to a
*lost* row: a row that is not served is not compared, so per-tile masked counts at a fixed depth
are held against the count the generator produces for the same principal and tiling. Per tile
rather than one global total, because a single number passes any defect that moves rows between
tiles while preserving the sum (§9.2). Together they close §8.1's discrimination ladder: a count
is blind to every permutation-preserving corruption, an identity set is blind to values, a
row-indexed check is blind to an unpermuted carry-forward, and a sampled identity-keyed check is
blind to everything outside the sample. An identity-keyed value check over every row, plus a
per-tile census, is blind to none.

**Ground truth is computed, never stored** (§8). At 10⁹ items the expected answer cannot be held
in a table, so every property of item *e* is a pure function of `(seed, e)` — and the expectation
side of this module is therefore the `tessera corpus` CLI verbs, which evaluate the one Rust
generator. A Python restatement of the generator would put the fixture under test rather than the
system (§12.1), so this module computes no corpus property itself; the one Python-side quantity is
the Morton code, taken from `oracle.morton` — the conformance suite's existing independent
statement of quantisation, pinned bit-for-bit to the Rust by its own vectors. The join is total by
construction: a corrupted `fx_key` inverts to *some* item, whose every property then disagrees
with the served row, so corruption of the join column is caught by the same comparison as
corruption of any other column.

**Lookups are batched per response, never per row** (§12.1). One `tessera corpus items` call per
recorded response, ids on stdin; one `tessera corpus census` call per (principal, depth). The
granularity is the design's: the O(*n*) census pass and the per-id evaluation stay in Rust, and
the Python side compares vectors. A call per row would multiply process spawns by the corpus and
make the mechanism unaffordable at exactly the sizes it exists for.

## Materialisation — the shim, and why it exists

The corpus reaches the build as files — points, pairs, the artifact rosters and their
memberships, `config.toml` — written by the crate's own materialisers. `tessera corpus
materialise` writes that set, but it writes only that set: §12.1's "the corpus emits a batch and
the driver posts it" needs one `/control/ingest` body drawn from the same generator over an
arbitrary item range, and no verb takes a range. So [`materialise_corpus`] compiles a two-file
cargo shim against `crates/tessera-corpus` itself and runs it, calling the same materialisers the
verb calls and then `Corpus::ingest_batch` beside them. This is the same trust chain as invoking
the CLI — the one Rust generator, reached through a build — and deliberately not a Python
restatement of the materialisers, for §12.1's reason. Two writers of one input set is the cost:
the shim's calls and `Corpus::config_toml`'s declaration are one obligation held apart in two
crates, which `test_materialisation.py` exists to keep matched. The shim is cached at a fixed
path per machine and shares the workspace's target directory, so after the first run its cost is
a cargo fingerprint check.

## What the harness owns, and the two rules a naive harness breaks

The expected census is the generator's count **minus the denies this harness has had accepted**
(§9.2) — they are the harness's own, and the corpus cannot know them. Applying the subtraction
needs each denied item's terms, which no verb serves; they come from the harness's *own inputs* —
the materialised `pairs.parquet` for built items, the posted ingest batch's `access` column for
ingested ones ([`terms_of`]). That is the fixture's input relation, not a second generator: the
passthrough grant rule applied in Python over terms the Rust materialisers wrote.

And the census must **barrier on the background refresh** before recording, or an established
session serves its previous projection and every count reads short by exactly the last ingested
batch (decision 0044 D1 — the refresh exists to change answers). The barrier is the executor's
`refreshes` counter, which is sound only while one session is resident; principals beyond the
recording one are authorised *after* the barrier, so their sessions materialise against the
current state. Background maintenance a tick might dispatch mid-run (a merge, a coalesce, a fold)
is entitled to change no answer, so this mechanism — unlike stage invariance — needs no isolation
from it.

## Two served shapes the expectations must meet half-way

- **A rendered number's absence is served as the type's zero** — the hot column cannot express
  absence and decision 0064's wire half is deferred — so the expected side maps an absent render
  number to 0 on both the points tail and the drill-down. A category's absence is its reserved
  code 0 on the tail and an omitted field at drill-down, which the declaration's own key→code
  table decides ([`Declaration`], parsed from the materialised `config.toml` rather than restated
  here).
- **The points tail is read positionally, not by name.** The tail's buffers are the render columns
  in manifest order, but the wire currently labels them with the first *k* names of the **full**
  declaration — the engine hands the serialiser the whole compiled schema while the gather narrows
  to render columns, so this corpus's `bay` codes arrive under the name `seen_at`. Found by this
  mechanism's first run; pinned as a strict xfail in `test_total_verification.py` so the fix is
  noticed. Positional reading is correct both before and after that fix, because the buffer order
  is the render declaration's either way.

The drill-down surface (`/v1/items`) is where the non-rendered families are verified — it is the
only reader of all three homes (§3) — and its `404` is a real answer: the row half requires a
battery item's drill-down to answer 404 exactly when the harness denied it, and requires a denied
item to appear in no points row, which is the deny lane's fail-closed rule observed on the row
surface.
"""

from __future__ import annotations

import base64
import io
import os
import re
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence

import pyarrow as pa
import pyarrow.ipc as ipc

from oracle import morton
from oracle.harness import CLI_BIN, REPO_ROOT, build_env, ensure_cli_built, write_deployment

from .battery import Categories, Item, Meta, Recorded, Viewport
from .canonical import Json, Streamed

#: The grid's own coordinates — the extent the conformance fixtures build against (contracts
#: §2.5), and the one the corpus verbs default to. The **build** reads it from the generator's own
#: declaration, which states it as the view's `extent`; this is the oracle's copy, used to derive
#: expected geometry.
GRID_EXTENT = (0.0, 65536.0, 0.0, 65536.0)

#: A fixed identity key for the fixture bundle: the lineage decision is "a test fixture, minted
#: deterministically", stated per the build's own rule rather than circumvented. Nothing may
#: persist a `tessera_id` across builds regardless — the ids are a keyed permutation.
FIXTURE_ID_KEY_HEX = "000102030405060708090a0b0c0d0e0f"

#: Where the materialiser shim lives, per machine — a fixed path for the same reason as the
#: catalogue's work dir: the compile is cached across sessions. The corpus *files* are per run.
SHIM_DIR = Path("/tmp/tessera-corpus-suite/materialise-shim")


class TotalVerificationFailure(AssertionError):
    """A served value, count or membership disagrees with the computed corpus — or the comparison
    was vacuous, which this mechanism refuses to report as a pass."""


# ---------------------------------------------------------------------------------------------
# Materialisation: the corpus's files, written by the crate's own materialisers
# ---------------------------------------------------------------------------------------------

_SHIM_MAIN = """\
//! Materialise the correctness-suite corpus (written by conformance/suite/verification.py; the
//! generator and every value are `tessera-corpus`'s — this file only names output paths).
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: corpus-materialise <seed> <n> <ingest_lo> <ingest_hi> <out_dir>");
        return ExitCode::FAILURE;
    }
    let seed: u64 = args[1].parse().expect("seed");
    let n: u64 = args[2].parse().expect("n");
    let lo: u64 = args[3].parse().expect("ingest_lo");
    let hi: u64 = args[4].parse().expect("ingest_hi");
    let out = std::path::PathBuf::from(&args[5]);
    std::fs::create_dir_all(&out).expect("out dir");
    let extent = tessera_spatial::Bounds { x_min: 0.0, x_max: 65536.0, y_min: 0.0, y_max: 65536.0 };
    let corpus = tessera_corpus::Corpus::new(seed, n, extent).expect("corpus");
    corpus.write_points_parquet(&out.join("points.parquet")).expect("points");
    corpus.write_pairs_parquet(&out.join("pairs.parquet")).expect("pairs");
    // Every remaining source `config_toml` declares — the four artifact rosters and their three
    // membership relations — in one call. The declaration is a constant, so the file set it names
    // and the file set written here are one obligation, checked by
    // `test_materialisation.py::test_every_declared_source_is_a_file_the_shim_wrote`.
    corpus.write_artifact_fixtures(&out).expect("artifact fixtures");
    std::fs::write(out.join("config.toml"), corpus.config_toml()).expect("config");
    if hi > lo {
        let batch = corpus.ingest_batch(lo..hi);
        let file = std::fs::File::create(out.join("ingest.arrows")).expect("ingest file");
        let mut writer =
            arrow::ipc::writer::StreamWriter::try_new(file, &batch.schema()).expect("writer");
        writer.write(&batch).expect("write");
        writer.finish().expect("finish");
    }
    ExitCode::SUCCESS
}
"""

_SHIM_MANIFEST = """\
[package]
name = "corpus-materialise"
version = "0.0.0"
edition = "2021"

[dependencies]
tessera-corpus = {{ path = "{corpus_crate}" }}
tessera-spatial = {{ path = "{spatial_crate}" }}
# Pinned to the workspace lock's exact version: the shim's `arrow` must be type-identical to the
# one `tessera-corpus` compiled against, or `RecordBatch` is two types and nothing links.
arrow = {{ version = "={arrow_version}", default-features = false, features = ["ipc"] }}

[workspace]
"""


@dataclass(frozen=True)
class CorpusFiles:
    """The materialised corpus: the build's inputs, the declaration, and the ingest batch."""

    seed: int
    n: int
    #: Item range the ingest stream carries (empty when nothing beyond the build was asked for).
    ingest_lo: int
    ingest_hi: int
    points: Path
    pairs: Path
    schema: Path
    ingest: Path | None

    @property
    def n_total(self) -> int:
        """Every item the harness has fed the server — built prefix plus ingested range. The
        census's `--n`: prefix-stability makes the corpus at `n_total` exactly the built corpus
        plus the posted batch (§8's first property, spent here)."""
        return max(self.n, self.ingest_hi)


def _workspace_arrow_version() -> str:
    lock = (REPO_ROOT / "Cargo.lock").read_text()
    m = re.search(r'name = "arrow"\nversion = "([^"]+)"', lock)
    if m is None:
        raise RuntimeError("no `arrow` entry in the workspace Cargo.lock — cannot pin the shim")
    return m.group(1)


def materialise_corpus(
    seed: int, n: int, out_dir: Path, *, ingest: tuple[int, int] | None = None
) -> CorpusFiles:
    """Write the corpus's files into `out_dir` via the crate's own materialisers (module doc).

    `ingest`, when given, is the half-open item range for one `/control/ingest` body — usually
    `(n, n + k)`, the corpus extended past the built prefix from the same functions.
    """
    lo, hi = ingest if ingest is not None else (n, n)
    SHIM_DIR.joinpath("src").mkdir(parents=True, exist_ok=True)
    (SHIM_DIR / "Cargo.toml").write_text(
        _SHIM_MANIFEST.format(
            corpus_crate=REPO_ROOT / "crates" / "tessera-corpus",
            spatial_crate=REPO_ROOT / "crates" / "tessera-spatial",
            arrow_version=_workspace_arrow_version(),
        )
    )
    (SHIM_DIR / "src" / "main.rs").write_text(_SHIM_MAIN)
    out_dir.mkdir(parents=True, exist_ok=True)
    run = subprocess.run(
        [
            "cargo", "run", "--release", "--quiet",
            "--manifest-path", str(SHIM_DIR / "Cargo.toml"),
            "--", str(seed), str(n), str(lo), str(hi), str(out_dir),
        ],
        env={**os.environ, "CARGO_TARGET_DIR": str(REPO_ROOT / "target")},
        capture_output=True,
        text=True,
    )
    if run.returncode != 0:
        raise RuntimeError(f"corpus materialisation failed:\n{run.stdout}\n{run.stderr}")
    return CorpusFiles(
        seed=seed,
        n=n,
        ingest_lo=lo,
        ingest_hi=hi,
        points=out_dir / "points.parquet",
        pairs=out_dir / "pairs.parquet",
        schema=out_dir / "config.toml",
        ingest=(out_dir / "ingest.arrows") if hi > lo else None,
    )


def build_bundle(files: CorpusFiles, bundle_root: Path) -> None:
    """`tessera build` over the materialised inputs — the same invocation shape as the catalogue's
    (`oracle.catalogue._build_argv`): a deployment file naming the declaration and the output,
    external ids minted from the source entity id (the denies address items by exactly those
    bytes), and the identity-key decision stated through the environment.

    Nothing names a source or an extent here: the generator's own declaration sits beside the two
    parquet files it names, and carries the grid extent this corpus's expected answers are stated
    in (`configuration.md` §1, §3) — the view it declares included, which is why nothing here names
    one either: `tessera build` materialises every view the declaration carries and takes no
    `--view` (fixed 2026-08-31; this function had kept the flag after the catalogue's own
    invocation dropped it, so every caller of it died at `build` with exit 2).
    """
    ensure_cli_built()
    deployment = write_deployment(
        files.schema.parent / "tessera.toml", bundle=bundle_root, schema=files.schema
    )
    subprocess.run(
        [
            str(CLI_BIN), "build",
            "--deployment", str(deployment),
            "--out", str(bundle_root),
            "--mint-external-ids",
        ],
        cwd=REPO_ROOT,
        env=build_env(FIXTURE_ID_KEY_HEX),
        check=True,
        capture_output=True,
    )


# ---------------------------------------------------------------------------------------------
# The declaration — parsed from the materialised schema, never restated
# ---------------------------------------------------------------------------------------------


def _value_codes(vocabulary: Mapping[str, object]) -> Mapping[str, int] | None:
    """A vocabulary's `key -> code` map, in either spelling (configuration.md §1).

    `values` is a table when the caller pinned codes and a bare array when it left them to the
    build, which assigns from 1 in declaration order, skipping `reserved` and never reaching the
    absent sentinel. The oracle has to mirror that assignment rather than refuse the spelling: it is
    the second reader the conformance suite exists to differ against, and a reader that only speaks
    one half of the surface silently narrows what the suite can cover.
    """
    values = vocabulary.get("values")
    if values is None or isinstance(values, Mapping):
        return values
    reserved = set(vocabulary.get("reserved", ()))
    codes: dict[str, int] = {}
    code = 1
    for key in values:
        while code in reserved:
            code += 1
        codes[key] = code
        code += 1
    return codes


@dataclass(frozen=True)
class ColumnDecl:
    name: str
    type: str
    render: bool
    index: bool
    #: A category's declared key→code table; `None` for every other family.
    values: Mapping[str, int] | None


@dataclass(frozen=True)
class Declaration:
    """The corpus's declared columns, read from the `config.toml` the build compiled — so the
    expected shapes below and the bundle's manifest share one source."""

    columns: tuple[ColumnDecl, ...]

    @classmethod
    def load(cls, config_path: Path) -> "Declaration":
        # A category's value table lives on the `[[vocabulary]]` block it names, not on the column
        # — `width`, `value_set` and `visibility` belong to the code space rather than to any one
        # attribute (configuration.md §1). The column is resolved through the reference here so the
        # expected shapes go on reading one table per column.
        raw = tomllib.loads(config_path.read_text())
        vocabularies = {v["name"]: v for v in raw.get("vocabulary", ())}
        columns = tuple(
            ColumnDecl(
                name=a["name"],
                type=a["type"],
                render=bool(a.get("render", False)),
                index=bool(a.get("index", False)),
                values=_value_codes(vocabularies.get(a.get("vocabulary", ""), {})),
            )
            for a in raw["attribute"]
        )
        return cls(columns)

    def render_columns(self) -> tuple[ColumnDecl, ...]:
        return tuple(c for c in self.columns if c.render)

    def names(self) -> tuple[str, ...]:
        return tuple(c.name for c in self.columns)


# ---------------------------------------------------------------------------------------------
# Expectations — the CLI verbs, one call per response / per (principal, depth)
# ---------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Expected:
    """One item's computed ground truth, as `tessera corpus items` states it."""

    e: int
    fx_key: int
    x: float
    y: float
    #: Declared field values by column name; `None` is absence. `seen_at` is microseconds since
    #: the epoch (cast from the verb's timestamp column, matching the drill-down's own unit).
    fields: Mapping[str, object]

    def code(self, extent: tuple[float, float, float, float] = GRID_EXTENT) -> int:
        """The full 64-bit position code the points batch must carry for this item."""
        return morton.code_of(self.x, self.y, extent)

    def tile(self, depth: int, extent: tuple[float, float, float, float] = GRID_EXTENT) -> int:
        """The depth-`depth` Morton prefix containing this item — the census's tile key."""
        return self.code(extent) >> (64 - 2 * depth) if depth else 0


def _run_cli(args: Sequence[str], stdin: bytes | None = None) -> bytes:
    run = subprocess.run(
        [str(CLI_BIN), *args], input=stdin, capture_output=True, cwd=REPO_ROOT
    )
    if run.returncode != 0:
        raise RuntimeError(
            f"tessera {' '.join(args[:2])} failed: {run.stderr.decode(errors='replace')}"
        )
    return run.stdout


def expected_items(seed: int, fx_keys: Iterable[int]) -> dict[int, Expected]:
    """One `tessera corpus items` call for every id a response carried (§12.1's granularity).

    Total over `u64`: a key no item was ever given still answers, as the item it inverts to —
    whose properties then disagree with whatever row carried the key (module doc).
    """
    keys = list(fx_keys)
    if not keys:
        return {}
    out = _run_cli(
        ["corpus", "items", "--seed", str(seed), "--ids", "-"],
        stdin="\n".join(str(k) for k in keys).encode(),
    )
    with ipc.open_stream(io.BytesIO(out)) as reader:
        table = reader.read_all()
    field_names = [n for n in table.schema.names if n not in ("fx_key", "e", "x", "y")]
    columns = {
        name: (
            table.column(name).cast(pa.int64()).to_pylist()
            if pa.types.is_timestamp(table.schema.field(name).type)
            else table.column(name).to_pylist()
        )
        for name in table.schema.names
    }
    expected: dict[int, Expected] = {}
    for i, fx in enumerate(columns["fx_key"]):
        expected[fx] = Expected(
            e=columns["e"][i],
            fx_key=fx,
            x=columns["x"][i],
            y=columns["y"][i],
            fields={"fx_key": fx, **{name: columns[name][i] for name in field_names}},
        )
    if set(expected) != set(keys):
        raise TotalVerificationFailure(
            "`corpus items` did not echo the ids it was given — the expectation side is broken"
        )
    return expected


def expected_census(seed: int, n: int, depth: int, grant_terms: Iterable[int]) -> dict[int, int]:
    """One `tessera corpus census` call: the expected masked count per depth-`depth` tile for one
    principal, before the harness's own denies are subtracted."""
    out = _run_cli(
        [
            "corpus", "census",
            "--seed", str(seed),
            "--n", str(n),
            "--zoom", str(depth),
            "--grant", ",".join(str(t) for t in grant_terms),
        ]
    )
    with ipc.open_stream(io.BytesIO(out)) as reader:
        table = reader.read_all()
    return dict(zip(table.column("tile").to_pylist(), table.column("count").to_pylist()))


def terms_of(files: CorpusFiles, es: Iterable[int]) -> dict[int, frozenset[int]]:
    """Each item's terms, from the harness's own materialised inputs (module doc): the pairs
    relation for the built prefix, the posted ingest batch's `access` labels beyond it.

    The `access` column is a list, one label per element, and each element is one descriptor
    verbatim — `builtin:passthrough`'s rule at both entry points (decision 0129). Nothing here
    splits a label."""
    import pyarrow.parquet as pq

    wanted = set(es)
    terms: dict[int, set[int]] = {e: set() for e in wanted}
    pairs = pq.read_table(files.pairs, columns=["entity_id", "term_id"])
    for e, t in zip(pairs.column("entity_id").to_pylist(), pairs.column("term_id").to_pylist()):
        if e in wanted:
            terms[e].add(t)
    if files.ingest is not None:
        with ipc.open_stream(io.BytesIO(files.ingest.read_bytes())) as reader:
            batch = reader.read_all()
        ids = [int.from_bytes(v, "little") for v in batch.column("external_id").to_pylist()]
        for e, labels in zip(ids, batch.column("access").to_pylist()):
            if e in wanted:
                terms[e].update(int(label) for label in labels)
    missing = [e for e in wanted if not terms[e]]
    if missing:
        raise TotalVerificationFailure(
            f"no terms found for items {sorted(missing)[:4]} — every corpus item carries at "
            f"least one, so the harness is reading the wrong inputs"
        )
    return {e: frozenset(ts) for e, ts in terms.items()}


def subtract_denies(
    census: Mapping[int, int],
    denied: Iterable[Expected],
    denied_terms: Mapping[int, frozenset[int]],
    grant_terms: Iterable[int],
    depth: int,
) -> dict[int, int]:
    """The harness's accepted denies, taken off the generator's count (§9.2). A deny moves a
    principal's counts only where the denied item was visible to that principal — the passthrough
    rule over the item's own materialised terms."""
    grant = set(grant_terms)
    out = dict(census)
    for item in denied:
        if not (denied_terms[item.e] & grant):
            continue
        tile = item.tile(depth)
        out[tile] = out.get(tile, 0) - 1
        if out[tile] == 0:
            del out[tile]
    return out


# ---------------------------------------------------------------------------------------------
# Reading a canonical response's surfaces
# ---------------------------------------------------------------------------------------------


def _streams_table(concatenated: bytes) -> pa.Table | None:
    """Zero or more complete Arrow IPC streams, concatenated — the canonical points/tiles/underlay
    encoding (§12.2). Each stream is self-delimiting, so repeated `open_stream` walks them all."""
    if not concatenated:
        return None
    reader_src = pa.BufferReader(concatenated)
    tables: list[pa.Table] = []
    while reader_src.tell() < len(concatenated):
        with ipc.open_stream(reader_src) as reader:
            tables.append(reader.read_all())
    return pa.concat_tables(tables) if len(tables) > 1 else tables[0]


def tile_visible(canon: Streamed) -> dict[int, int]:
    """The tiles surface's masked count per tile — the served side of the census half."""
    table = _streams_table(canon.tiles)
    if table is None:
        return {}
    return dict(zip(table.column("tile").to_pylist(), table.column("visible").to_pylist()))


def underlay_counts(canon: Streamed) -> dict[int, int]:
    """The underlay's masked count per cell — the same claim as the tiles surface at depth
    `zoom + underlay_offset`, and the only other derived aggregate in the system (§3)."""
    table = _streams_table(canon.underlay)
    if table is None:
        return {}
    return dict(zip(table.column("cell").to_pylist(), table.column("count").to_pylist()))


# ---------------------------------------------------------------------------------------------
# The row half
# ---------------------------------------------------------------------------------------------


def check_points(
    label: str,
    canon: Streamed,
    expected: Mapping[int, Expected],
    *,
    declaration: Declaration,
    denied_fx: frozenset[int],
    reasons: list[str],
) -> int:
    """Every row of one points surface against its own item: the code, and the render tail
    positionally (module doc). Returns the number of rows verified."""
    table = _streams_table(canon.points)
    if table is None:
        return 0
    render = declaration.render_columns()
    if table.num_columns != 2 + len(render):
        reasons.append(
            f"{label}: the points tail carries {table.num_columns - 2} scalar columns where the "
            f"declaration renders {len(render)}"
        )
        return 0
    fx_column = table.column("fx_key").to_pylist()
    codes = table.column("code").to_pylist()
    # Positions 2.. are the render columns in manifest order; the name check is the strict
    # xfail's business, not a laxity here (module doc).
    tail = [table.column(2 + i).to_pylist() for i in range(len(render))]
    rows = 0
    for i, fx in enumerate(fx_column):
        if fx in denied_fx:
            reasons.append(
                f"{label}: row {i} serves item fx {fx:#x}, which this harness denied — the deny "
                f"lane is fail-open on the row surface"
            )
            continue
        item = expected.get(fx)
        if item is None:
            reasons.append(
                f"{label}: row {i} carries fx {fx:#x}, for which the caller supplied no "
                f"expectation — the batching is broken"
            )
            continue
        if codes[i] != item.code():
            qx, qy = morton.deinterleave64(codes[i])
            reasons.append(
                f"{label}: row {i} (fx {fx:#x}, item {item.e}) carries code {codes[i]:#x} "
                f"(axes {qx:#x}, {qy:#x}) where the corpus places it at {item.code():#x}"
            )
        for col, served_column in zip(render, tail):
            served = served_column[i]
            value = item.fields[col.name]
            if col.values is not None:
                want = col.values[value] if value is not None else 0
            else:
                want = value if value is not None else 0
            if served != want:
                reasons.append(
                    f"{label}: row {i} (fx {fx:#x}, item {item.e}) serves {col.name!r} = "
                    f"{served!r} where the corpus says {want!r}"
                )
        rows += 1
    return rows


def check_item(
    label: str,
    canon: Json,
    item: Expected,
    *,
    declaration: Declaration,
    denied: bool,
    reasons: list[str],
) -> int:
    """One drill-down against its item: every declared field from whichever home holds it, the
    404 exactly at the harness's own denies, and the external-id join. Returns rows verified."""
    status = canon.payload["status"]
    if denied:
        if status != 404:
            reasons.append(
                f"{label}: item {item.e} (fx {item.fx_key:#x}) was denied and still answers "
                f"{status}"
            )
        return 1
    if status != 200:
        reasons.append(
            f"{label}: item {item.e} (fx {item.fx_key:#x}) answers {status} — a row this "
            f"harness never denied has vanished from the drill-down"
        )
        return 1
    body = canon.payload["body"]
    fields = body["fields"]
    for col in declaration.columns:
        value = item.fields[col.name]
        if col.render and col.values is None and value is None:
            want = 0  # a rendered number's absence is the stored zero (module doc)
        else:
            want = value
        served = fields.get(col.name)
        if served != want:
            reasons.append(
                f"{label}: item {item.e} serves {col.name!r} = {served!r} where the corpus "
                f"says {want!r}"
            )
    undeclared = set(fields) - set(declaration.names())
    if undeclared:
        reasons.append(f"{label}: item {item.e} serves undeclared fields {sorted(undeclared)}")
    want_external = base64.b64encode(item.e.to_bytes(8, "little")).decode()
    if body.get("external_id") != want_external:
        reasons.append(
            f"{label}: item {item.e} serves external_id {body.get('external_id')!r} where the "
            f"fixture's convention (the source id's little-endian bytes) says {want_external!r}"
        )
    return 1


def check_meta(canon: Json, declaration: Declaration, reasons: list[str]) -> None:
    """`/v1/meta` states exactly the declared columns, with the flags the schema compiled —
    §3's reason for the surface: a producer that dropped a column from the manifest."""
    served = canon.payload["declared_scalars"]
    served_shape = [(d["name"], d["render"], d["index"]) for d in served]
    declared_shape = [(c.name, c.render, c.index) for c in declaration.columns]
    if served_shape != declared_shape:
        reasons.append(
            f"/v1/meta declares {served_shape} where the schema compiled {declared_shape}"
        )
    # The frame rides with the view it belongs to (decision 0040): every view must state one,
    # and every one of them must be the fixture's.
    for view in canon.payload["views"]:
        extent = view.get("quantisation")
        if extent is None:
            reasons.append(f"/v1/meta view {view['id']!r} publishes no quantisation extent")
            continue
        if (extent["x_min"], extent["x_max"], extent["y_min"], extent["y_max"]) != GRID_EXTENT:
            reasons.append(
                f"/v1/meta view {view['id']!r} quantisation {extent} is not the fixture's extent"
            )
    check_roster(canon, reasons)


def check_roster(canon: Json, reasons: list[str]) -> None:
    """The roster and the views agree (`views.md` §3.2).

    The fixture declares plain views alone, so what this asserts on it is that *nothing* claims a
    group — which is the case the shape could get wrong in the quiet direction, a served view
    carrying a key no group lists. The other direction is checked too, and both bite the moment a
    grouped fixture exists: a view a client can see and cannot address, or a roster entry that
    resolves to nothing, is a picker that offers a view the server will 404.

    **A view is addressed by its key and by nothing else** (decision 0113), so there is no number
    to check here: what orders a group is the order its `views` list is in, and a served record
    carrying an `ordinal` field would be the removed machinery come back.
    """
    views = {view["id"]: view for view in canon.payload["views"]}
    rostered: set[str] = set()
    for group in canon.payload["groups"]:
        for view_id in group["views"]:
            rostered.add(view_id)
            view = views.get(view_id)
            if view is None:
                reasons.append(
                    f"/v1/meta group {group['name']!r} lists {view_id!r}, which it does not serve"
                )
                continue
            if view["group"] != group["name"]:
                reasons.append(
                    f"/v1/meta view {view_id!r} is listed by group {group['name']!r} and names "
                    f"group {view['group']!r}"
                )
            if view["id"] != f"{view['group']}:{view['key']}":
                reasons.append(
                    f"/v1/meta view {view_id!r} is not the join of its group and its key"
                )
    for view_id, view in views.items():
        if "ordinal" in view:
            reasons.append(
                f"/v1/meta view {view_id!r} carries an `ordinal`; a view is addressed by its key "
                f"alone and a group's order is its list's order (decision 0113)"
            )
        record = [view["group"], view["key"], view["metadata"]]
        if view_id in rostered:
            if any(field is None for field in record):
                reasons.append(f"/v1/meta view {view_id!r} is on a roster with a partial record")
        elif any(field is not None for field in record):
            reasons.append(
                f"/v1/meta view {view_id!r} carries a roster record and is on no group's roster"
            )


def check_categories(
    query: Categories, canon: Json, declaration: Declaration, reasons: list[str]
) -> None:
    """A category surface enumerates exactly the declared vocabulary, key and code alike."""
    decl = next(c for c in declaration.columns if c.name == query.column)
    served = [
        (v["code"], v["key"]) for page in canon.payload["pages"] for v in page["values"]
    ]
    want = sorted((code, key) for key, code in (decl.values or {}).items())
    if sorted(served) != want:
        reasons.append(
            f"/v1/categories/{query.column} serves {sorted(served)} where the declaration "
            f"says {want}"
        )


def verify_rows(
    recorded: Recorded,
    *,
    seed: int,
    declaration: Declaration,
    denied_fx: frozenset[int],
    fx_of_tessera: Mapping[int, int],
) -> int:
    """The row half over one recording: every row of every response, one expectation call per
    response (module doc). Returns the number of rows verified; raises on any disagreement, and
    on a vacuous run — a total verification that verified nothing is indistinguishable from a
    stub, so zero rows is a failure, not a pass."""
    reasons: list[str] = []
    rows = 0
    item_queries = [q for q in recorded if isinstance(q, Item)]
    drilldown_expected = expected_items(
        seed, [fx_of_tessera[q.tessera_id] for q in item_queries]
    )
    for query, canon in recorded.items():
        if isinstance(query, Viewport):
            assert isinstance(canon, Streamed)
            label = (
                f"viewport(zoom={query.zoom}, "
                f"filters={'yes' if query.filters else 'no'})"
            )
            table = _streams_table(canon.points)
            served_fx = table.column("fx_key").to_pylist() if table is not None else []
            expected = expected_items(seed, set(served_fx))
            rows += check_points(
                label,
                canon,
                expected,
                declaration=declaration,
                denied_fx=denied_fx,
                reasons=reasons,
            )
        elif isinstance(query, Item):
            fx = fx_of_tessera[query.tessera_id]
            rows += check_item(
                f"items/{query.tessera_id}",
                canon,
                drilldown_expected[fx],
                declaration=declaration,
                denied=fx in denied_fx,
                reasons=reasons,
            )
        elif isinstance(query, Meta):
            check_meta(canon, declaration, reasons)
        elif isinstance(query, Categories):
            check_categories(query, canon, declaration, reasons)
    if reasons:
        shown = "\n  - ".join(reasons[:20])
        more = f"\n  … and {len(reasons) - 20} more" if len(reasons) > 20 else ""
        raise TotalVerificationFailure(
            f"total verification: {len(reasons)} disagreement(s) over {rows} row(s):"
            f"\n  - {shown}{more}"
        )
    if rows == 0:
        raise TotalVerificationFailure(
            "total verification verified zero rows — the recording is empty or the harness "
            "recorded the wrong thing"
        )
    return rows


# ---------------------------------------------------------------------------------------------
# The census half
# ---------------------------------------------------------------------------------------------


def verify_census(
    served: Mapping[int, int], expected: Mapping[int, int], *, label: str
) -> int:
    """Per-tile equality, absent-as-zero on both sides — a tile in either map is compared, so a
    lost tile and an invented one both surface, and the failure names the tile (§9.2's
    localisation argument). Returns the number of tiles compared."""
    reasons = []
    tiles = sorted(set(served) | set(expected))
    for tile in tiles:
        s, x = served.get(tile, 0), expected.get(tile, 0)
        if s != x:
            reasons.append(f"tile {tile:#x}: served {s}, corpus says {x}")
    if reasons:
        shown = "\n  - ".join(reasons[:20])
        more = f"\n  … and {len(reasons) - 20} more" if len(reasons) > 20 else ""
        raise TotalVerificationFailure(
            f"census ({label}): {len(reasons)} tile(s) disagree:\n  - {shown}{more}"
        )
    return len(tiles)


__all__ = [
    "ColumnDecl",
    "CorpusFiles",
    "Declaration",
    "Expected",
    "FIXTURE_ID_KEY_HEX",
    "GRID_EXTENT",
    "TotalVerificationFailure",
    "build_bundle",
    "check_categories",
    "check_item",
    "check_meta",
    "check_points",
    "expected_census",
    "expected_items",
    "materialise_corpus",
    "subtract_denies",
    "terms_of",
    "tile_visible",
    "underlay_counts",
    "verify_census",
    "verify_rows",
]
