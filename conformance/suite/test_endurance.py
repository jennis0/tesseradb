"""The endurance tier — correctness-suite §6's backstop, build-order row 9.

**The axis here is the operation count, not the corpus size**, and it is the one axis nothing
else in the tree exercises: the soak reaches forty flushes, the stage-invariance plan eight
writes and one fold, and every number they check is small enough to be right by accident. The
defects this tier exists for need a long life — a `seg_id` allocator that wraps, a reclamation
pass that takes back slightly less than each fold orphans, a maintenance width that holds for
five cycles and drifts over five hundred — and each of them is invisible at any corpus size run
briefly. So this module composes the driver's existing stages (§12.3) into one long plan:
thousands of writes, a hundred folds, denies of all three retirement stories, WAL rotations,
reloads and one kill at a sampled fold, with the read battery recorded around every stage as
everywhere else.

**Every assertion is a bound, never a figure.** The exact counts are functions of the policy
widths (tier width 4, coalesce width 8, the merge cap), and asserting them would make this a
test of the constants; what matters is that the numbers stop growing with the number of
operations. The ceilings are generous on purpose: they are what a *stopped* maintenance pass
fails — a dead coalesce reaches the write count within one ladder phase, a dead reclaim
accumulates a prefix per fold — while a working one sits far beneath them whatever the widths
are tuned to.

What only accumulation shows, and where this module looks for it:

- **Identifier monotonicity under pressure.** `seg_id`s are never reused, across merges or
  prefixes (contracts §2.1); the side-manifest sequence `SEGMENTS-<n>.json` is monotone, unpadded
  and never replaced. Checked from the bundle's own files at every observation, over hundreds of
  publications; a persisting side-manifest whose bytes change, or a retired id that reappears, is
  named directly.
- **Reclamation keeping pace.** On-disc bytes only grow until a fold reclaims, and a fold that
  reclaims slightly less than it orphans is indistinguishable from a working one over four folds
  — this is the single most valuable measurement in the tier. After every fold (and the reclaim
  tick that follows it) the module measures bytes outside the live prefix and unreferenced bytes
  inside it, against ceilings that do not grow with the fold count.
- **Prefix accumulation.** Each fold writes a new prefix and flips `CURRENT`; the prefix count
  must return to one and the live prefix's sequence number must only rise.
- **The merge ladder's saturation** (decision 0049). With `max_merged_segment_bytes` set, the
  live segment count settles at flushed bytes over the saturation size rather than collapsing to
  one — confirming the settle needs enough unfolded merges to reach it, which is what the
  fold-free ladder phase at the head of the plan provides.
- **The six axes the coalesce bounds** — delta tiers, dictionary extents, attribute extents,
  record extents, text extents, and external-id runs (with their locators, bounded by the merge)
  — over hundreds of cycles rather than the soak's five.
- **The allocator floor and the entity high-water** — monotone across every reload, fold and the
  kill, never re-minting — and the **WAL's reclaim bound**: rotation's steady state is two
  members, held while accepted deletions are still pending their fold.
- **Overlay growth**: deletions retire exactly at the fold that executes them (Rule F) and
  suppressions never retire at all (Rule S — write-path §5.4's two removal rules, kept apart
  here across many folds, including a suppression that outlives several folds before its
  unsuppress).

**Verification cadence is what makes a long run affordable** (§6). The read battery rides every
stage, through `run_plan` exactly as the short tiers use it. Total verification (§9 — every
served row at its own identity, plus the per-tile census) runs on a sampled subset of fold
cycles and totally at the end. The deep structural verifier (`tessera verify <bundle> --deep`, a
full re-hash) runs every *n*th fold and at the end, never every stage — at a thousand writes the
re-hash would be the whole run.

**This is a backstop, never a gate** (§6, §13). It is skipped unless `TESSERA_SUITE_ENDURANCE=1`
is set, so a plain `pytest conformance/suite` — including CI's — never runs it; wiring it into a
gate and then disabling it is the failure that sentence in the design exists to prevent. It runs
on demand and before a release. The knobs (defaults are the real thing, §16's tier table):

    TESSERA_SUITE_ENDURANCE=1   opt in (required; everything below is optional)
    TESSERA_SUITE_BASE=1000000  built corpus items
    TESSERA_SUITE_ROUNDS=2000   ingest-and-flush writes
    TESSERA_SUITE_FOLDS=100     compaction folds
    TESSERA_SUITE_ROWS=256      rows per write
    TESSERA_SUITE_SEED=20260816 corpus seed
    TESSERA_SUITE_VERIFY=every:10  deep-verify and total-verification cadence, in folds
    TESSERA_SUITE_KILL=1        kill one sampled fold at its publication seam (§10.1)

## Composition, not modification

The driver is used as built: `run_plan` walks chunks of the one plan (chunked so recordings are
dropped as they are checked — a two-thousand-stage result list would hold every recording at
once), and each stage is judged by `check` against its own entitlement. One deliberate seam: a
write's `Rows` entitlement here is the batch's *visible-to-the-recording-principal* subset, not
the whole batch — the corpus prices grants by construction, so most ingested rows are invisible
to the battery principal and the wire cannot show them (the same masked-entitlement argument as
`Entity`'s doc). `check(result, claimed=...)` exists for exactly this, so the stage still
asserts acceptance of the full batch while the recording is held to the visible rows.

Maintenance is deliberately *not* isolated per stage, unlike the stage-invariance plan: merges
and coalesces dispatch on whatever write tick finds them eligible, which is the steady state a
deployment actually runs in. They are entitled to change nothing, so a write's diff still equals
its visible rows — and a maintenance pass that did corrupt an answer surfaces as that write's
`Unexplained`.

Observations sample the bundle between chunks (a few writes apart, and after every fold), not
after every publication; an identifier minted and retired wholly between two observations is
outside their sight. The sampled cadence is the affordability trade §6 names, made explicit.

## A defect this tier found on its first run, pinned rather than tolerated silently

A merge planned on the same tick as a flush used to publish its side-manifest with the
watermark it captured at *plan* time (`tessera-engine::merge`'s `rebase_into` overwrote the
cloned manifest's), one batch behind the flush that shared its tick — every later publication
cloned the stale value forward and a reboot under-reported by exactly one batch, with total
verification green across it (the boot rebuilds the buffer by `row_of`, not the watermark).
Fixed 2026-08-15: the rebase now keeps the cloned live manifest's values, and the engine's
manifest-commit guard (`check_manifest_publishable`) refuses any side-manifest that would
regress a durable scalar, so the same shape on the next field is a loud refusal rather than a
silent regression. The one-batch allowance the assertions below carried while the defect was
pinned is deleted: the watermark is held strictly monotone, live and durable alike, and the
durable value may never trail the served one. The interleaving itself is pinned
deterministically in `crates/tessera-engine/tests/merge.rs`.

A second defect fell out of running long before any assertion could: the driver's spawn pipes
the server's stdout/stderr and nothing reads it, so after roughly one pipe buffer of logging the
write executor blocks inside a log write and never consumes another tick — the tier wedged at
the same operation count three runs straight, and `/proc/<pid>/task/*/wchan` showed the
executor's thread in `anon_pipe_write`. [`PipeDrainer`] is this module's own remedy and keeps
the tail as failure evidence; the durable fix belongs in the driver's spawn.

The third was the dictionary-extent axis, §6's predicted drift found in the flesh and since
fixed. Under sustained ingest alone the coalesce bounded it — this plan's fold-free ladder holds
it oscillating under ten entries across 128 promoting flushes — but under a fold cadence it
ratcheted 6 to 58 across 24 cycles, linear in the operation count and never draining.

The cause was not the one this paragraph first gave. Blaming the coalesce's eight-consecutive
window and stranded tiers described the observed *shape*; what actually froze the entries was
eligibility. A fold digest-names every file it carries into the new prefix's manifest, for the
durability of its hard links, and the coalesce read "digest-named in MANIFEST" as "the build's
own artefact, never take" — so every extent alive at a flip became permanently ineligible. The
dictionary is the one guarded axis a fold does not rebuild into its base, which is why it alone
ratcheted. Eligibility there is positional now: only the single extent a builder writes is
spared. The axis asserts a flat ceiling below, like the other five.

Recorded rather than quietly corrected, because a wrong diagnosis that matched the data is worth
more to the next reader than a tidy one.
"""

from __future__ import annotations

import base64
import collections
import hashlib
import io
import json
import os
import re
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

import pyarrow.ipc as ipc
import pytest

from oracle.harness import CLI_BIN

from .battery import Viewport
from .driver import (
    Build,
    Deny,
    Fold,
    Killed,
    Load,
    Rotate,
    Stage,
    SuiteHarness,
    Write,
    check,
    run_plan,
)
from .entitlement import Delta, Rows
from .verification import (
    Declaration,
    build_bundle,
    expected_census,
    expected_items,
    materialise_corpus,
    subtract_denies,
    terms_of,
    tile_visible,
    underlay_counts,
    verify_census,
    verify_rows,
)

# -- parameters ---------------------------------------------------------------------------------

#: The recording principal: eight level-1 terms, about 1/16 of the corpus visible. Narrow enough
#: that two thousand battery recordings stay affordable, wide enough that every write lands a few
#: visible rows and every surface has content.
GRANT_TERMS = tuple(range(64, 72))
GRANTS = tuple(str(t) for t in GRANT_TERMS)
VIEW_ID = "s0"
BBOX = (0.0, 0.0, 65536.0, 65536.0)
#: The per-tile cap (contracts §3.2's cap clause, not a per-request ceiling).
#:
#: **At the small tiers this sits above every visible total the run can reach**, so no tile
#: truncates, a removed row admits nothing behind it, and a write's delta is the corpus's delta.
#: Past roughly three million items at this grant's 1/16 it no longer can, and the zoom-0
#: viewport — one tile over the whole extent — caps first while the deeper ones, whose visible
#: set is spread over 64 and 1024 tiles, stay complete for far longer. A capped tile is not a
#: skipped check: the diff falls to its capped arm, which holds membership over the tiles that
#: stayed complete and net movement everywhere, and reports a recording whose every tile capped
#: as uncheckable rather than passing it.
#:
#: Raising this to chase exactness at scale is the wrong lever twice over — it cannot outrun a
#: corpus that grows, and the deep viewport's served set is what the harness must then compare.
K = 200_000
#: The suite config's merge cap (`driver._suite_config`): the ladder's saturation size.
SATURATION_BYTES = 1_048_576

#: Generous ceilings for the coalesce-bounded axes — with width 8 and tiered outputs, a working
#: pass sits under ~8·log8(ops); a stopped one reaches the ladder phase's write count (hundreds).
AXIS_CEILING = 32
#: `attr_extents` is one list over four per-flush columns (weight, seen_at, bay, tag), so its
#: transient peak sits near four windows' worth per column; measured oscillating 24–102 through
#: the ladder with the coalesce's drops clearly visible. A stopped pass reaches four entries per
#: flush — several times this line within one ladder phase.
ATTR_AXIS_CEILING = 192
#: Non-base segments allowed beyond the flushed-bytes ÷ saturation term: up to three unmerged
#: segments per active ladder size class, a few classes deep.
LADDER_SLACK = 9
#: WAL members: rotation's steady state is two (`tessera-lifecycle::wal`), plus transients.
WAL_MEMBER_CEILING = 4
#: Bytes outside the live prefix after the post-fold reclaim: `CURRENT` aside, there should be
#: none; slack for a straggling temp file.
CROSS_PREFIX_DEAD_CEILING = 65_536

_PREFIX_RE = re.compile(r"v\d{5,}")
_SIDE_RE = re.compile(r"SEGMENTS-(\d+)\.json$")


def parse_verify_cadence(text: str) -> int:
    """§16's `TESSERA_SUITE_VERIFY` grammar, the slice of it this tier uses: `every:<n>` (deep
    verification and sampled total verification every n folds), `each` meaning every fold, or a
    bare integer."""
    text = text.strip()
    if text == "each":
        return 1
    if text.startswith("every:"):
        text = text[len("every:"):]
    n = int(text)
    if n < 1:
        raise ValueError(f"a verification cadence must be at least 1, not {n}")
    return n


@dataclass(frozen=True)
class Params:
    """One run's shape, from the `TESSERA_SUITE_*` environment; defaults are §16's endurance row."""

    base: int
    rounds: int
    folds: int
    rows: int
    seed: int
    verify_every: int
    kill: bool

    @classmethod
    def from_env(cls) -> "Params":
        env = os.environ.get
        return cls(
            base=int(env("TESSERA_SUITE_BASE", "1000000")),
            rounds=int(env("TESSERA_SUITE_ROUNDS", "2000")),
            folds=int(env("TESSERA_SUITE_FOLDS", "100")),
            rows=int(env("TESSERA_SUITE_ROWS", "256")),
            seed=int(env("TESSERA_SUITE_SEED", "20260816")),
            verify_every=parse_verify_cadence(env("TESSERA_SUITE_VERIFY", "every:10")),
            kill=env("TESSERA_SUITE_KILL", "1") == "1",
        )

    @property
    def writes_per_cycle(self) -> int:
        """Writes inside each fold cycle: three quarters of the writes ride the fold cadence."""
        return max(1, (self.rounds * 3 // 4) // self.folds)

    @property
    def ladder_writes(self) -> int:
        """The fold-free stretch at the head of the plan — the remaining quarter (or whatever the
        integer split leaves), long enough for the merge ladder to reach its settle."""
        return self.rounds - self.writes_per_cycle * self.folds

    @property
    def ingest_range(self) -> tuple[int, int]:
        return (self.base, self.base + self.rounds * self.rows)


# -- bundle observation -------------------------------------------------------------------------


def _du(path: Path) -> int:
    total = 0
    for root, _dirs, names in os.walk(path):
        for name in names:
            total += (Path(root) / name).stat().st_size
    return total


def _strings(node) -> set[str]:
    """Every key and string value in a parsed manifest — the file-reference sweep. Local rather
    than imported from the driver, whose helper is private to its crash-discard rules;
    over-collection is safe on this side too (it can only make a file count as referenced)."""
    out: set[str] = set()
    if isinstance(node, dict):
        for key, value in node.items():
            out.add(key)
            out |= _strings(value)
    elif isinstance(node, list):
        for value in node:
            out |= _strings(value)
    elif isinstance(node, str):
        out.add(node)
    return out


@dataclass
class BundleView:
    """One observation's read of the bundle on disc: the live prefix's latest side-manifests and
    the byte accounting the reclamation assertions run on."""

    prefixes: tuple[str, ...]
    live_prefix: str
    #: (partition, n) -> sha256 of every side-manifest file present in the live prefix.
    side_digests: dict[tuple[str, int], str]
    #: partition -> the latest side-manifest, parsed.
    latest: dict[str, dict]
    total_bytes: int
    live_bytes: int
    cross_prefix_dead: int
    unreferenced_bytes: int
    #: seg_id -> on-disc bytes of its segment directory, from the latest side-manifests.
    segment_bytes: dict[str, int]


def read_bundle(bundle_root: Path) -> BundleView:
    live = json.loads((bundle_root / "CURRENT").read_text())["prefix"]
    prefixes = tuple(
        sorted(
            d.name
            for d in bundle_root.iterdir()
            if d.is_dir() and _PREFIX_RE.fullmatch(d.name)
        )
    )
    prefix_dir = bundle_root / live

    side_digests: dict[tuple[str, int], str] = {}
    latest_paths: dict[str, tuple[int, Path]] = {}
    for manifest_path in sorted(prefix_dir.glob("partitions/*/SEGMENTS-*.json")):
        match = _SIDE_RE.search(manifest_path.name)
        n_text = match.group(1)
        if n_text != str(int(n_text)):
            raise AssertionError(
                f"side-manifest sequence is padded: {manifest_path} — the sequence is written "
                f"unpadded (`SEGMENTS-{{n}}.json`) and a padded member is a foreign writer"
            )
        n = int(n_text)
        partition = manifest_path.parent.name
        side_digests[(partition, n)] = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
        if partition not in latest_paths or n > latest_paths[partition][0]:
            latest_paths[partition] = (n, manifest_path)

    latest = {
        partition: json.loads(path.read_text())
        for partition, (_n, path) in latest_paths.items()
    }

    referenced: set[str] = set()
    for manifest_path in [prefix_dir / "MANIFEST.json", *prefix_dir.glob("partitions/*/SEGMENTS-*.json")]:
        referenced |= _strings(json.loads(manifest_path.read_text()))

    total_bytes = _du(bundle_root)
    live_bytes = _du(prefix_dir)
    current_bytes = (bundle_root / "CURRENT").stat().st_size
    unreferenced = 0
    for file_path in prefix_dir.rglob("*"):
        if not file_path.is_file():
            continue
        rel = str(file_path.relative_to(prefix_dir))
        name = file_path.name
        if name in ("MANIFEST.json",) or _SIDE_RE.search(name):
            continue
        if rel in referenced:
            continue
        unreferenced += file_path.stat().st_size

    segment_bytes: dict[str, int] = {}
    for partition, manifest in latest.items():
        for seg in manifest["segments"]:
            seg_dir = (
                prefix_dir
                / "partitions"
                / partition
                / "views"
                / seg["view"]
                / "segments"
                / seg["seg_id"]
            )
            segment_bytes[seg["seg_id"]] = _du(seg_dir) if seg_dir.exists() else 0

    return BundleView(
        prefixes=prefixes,
        live_prefix=live,
        side_digests=side_digests,
        latest=latest,
        total_bytes=total_bytes,
        live_bytes=live_bytes,
        cross_prefix_dead=total_bytes - live_bytes - current_bytes,
        unreferenced_bytes=unreferenced,
        segment_bytes=segment_bytes,
    )


def _wal_members(wal_path: Path) -> list[Path]:
    return sorted(wal_path.parent.glob(f"{wal_path.stem}-[0-9]*{wal_path.suffix}"))


@dataclass
class Tracker:
    """Cross-observation state: everything whose assertion is "…and it never went backward"."""

    ever_seg_ids: set[str] = field(default_factory=set)
    prev_live_seg_ids: set[str] = field(default_factory=set)
    #: (prefix, partition, n) -> sha256 for every side-manifest ever observed.
    side_seen: dict[tuple[str, str, int], str] = field(default_factory=dict)
    #: The side-manifest sequence is one global counter — a fold's first manifest continues the
    #: old prefix's numbering rather than restarting it (observed, then asserted).
    side_global_max: int = 0
    entity_high_water: int = 0
    watermark: int = 0
    live_prefix_version: int = 0
    findings: list[str] = field(default_factory=list)
    #: Live (unretired) suppressions, oldest first, and deletions pending their fold.
    suppressed: list["DenyRecord"] = field(default_factory=list)
    deleted: list["DenyRecord"] = field(default_factory=list)
    pending_deletes: int = 0
    folds_landed: int = 0
    observations: list[dict] = field(default_factory=list)

    def denied_fx(self) -> frozenset[int]:
        return frozenset(r.fx for r in (*self.suppressed, *self.deleted))


@dataclass(frozen=True)
class DenyRecord:
    fx: int
    e: int
    terms: frozenset[int]


def observe(h: SuiteHarness, t: Tracker, p: Params, label: str, *, quiescent: bool) -> dict:
    """One observation: read the wire's gauges and the bundle's files, assert every bound that
    must hold at this point in the run, and append the numbers to the report.

    `quiescent` marks observations taken after maintenance has drained (post-fold, post-ladder):
    only there are wire and disc asserted to agree, since between drains a merge may be mid-swap.
    """
    status = h.status()
    executor = status["write_executor"]
    view = read_bundle(h.bundle_root)

    # -- identifier monotonicity ----------------------------------------------------------------
    live_ids = set(view.segment_bytes)
    new_ids = live_ids - t.prev_live_seg_ids
    reused = new_ids & t.ever_seg_ids
    assert not reused, (
        f"{label}: seg_id(s) {sorted(reused)[:4]} appeared again after retiring — ids are never "
        f"reused, across merges or prefixes (contracts §2.1), and a wrapped allocator is exactly "
        f"the defect only a long run can see"
    )
    t.ever_seg_ids |= live_ids
    t.prev_live_seg_ids = live_ids

    fresh_ns = []
    for (partition, n), digest in sorted(view.side_digests.items()):
        key = (view.live_prefix, partition, n)
        earlier = t.side_seen.get(key)
        assert earlier is None or earlier == digest, (
            f"{label}: {view.live_prefix}/partitions/{partition}/SEGMENTS-{n}.json changed bytes "
            f"under observation — the sequence is append-only and a member is never replaced"
        )
        if earlier is None:
            fresh_ns.append(n)
        t.side_seen[key] = digest
    # One global counter: every manifest not seen before must sit above everything ever seen —
    # across flushes, merges, denies, coalesces and the fold's new prefix alike.
    for n in fresh_ns:
        assert n > t.side_global_max or not t.side_global_max, (
            f"{label}: SEGMENTS-{n}.json appeared below the sequence's high-water "
            f"{t.side_global_max} — the side-manifest counter went backwards or forked"
        )
    if fresh_ns:
        t.side_global_max = max(t.side_global_max, *fresh_ns)

    # -- prefixes and reclamation ---------------------------------------------------------------
    version = int(view.live_prefix[1:])
    assert version >= t.live_prefix_version, (
        f"{label}: the live prefix went backwards ({t.live_prefix_version} -> {version})"
    )
    t.live_prefix_version = version
    if quiescent:
        assert len(view.prefixes) == 1, (
            f"{label}: {len(view.prefixes)} prefixes on disc after reclamation had its tick "
            f"({view.prefixes}) — superseded prefixes are accumulating"
        )
        assert view.cross_prefix_dead <= CROSS_PREFIX_DEAD_CEILING, (
            f"{label}: {view.cross_prefix_dead} bytes live outside the live prefix — "
            f"reclamation is not keeping pace with what the folds orphan"
        )
        unref_ceiling = max(SATURATION_BYTES, view.live_bytes // 10)
        assert view.unreferenced_bytes <= unref_ceiling, (
            f"{label}: {view.unreferenced_bytes} unreferenced bytes inside the live prefix "
            f"(ceiling {unref_ceiling}) — publications are orphaning files no manifest names"
        )

    # -- the coalesce-bounded axes and the merge ladder -----------------------------------------
    seg_count = 0
    for partition, manifest in view.latest.items():
        axes = {
            "deltas": len(manifest["deltas"]),
            "record_extents": len(manifest["record_extents"]),
            "text_extents": len(manifest["text_extents"]),
        }
        for name, count in axes.items():
            assert count <= AXIS_CEILING, (
                f"{label}: `{name}` reached {count} entries (ceiling {AXIS_CEILING}) — this axis "
                f"grows one per flush and only the entity-space coalesce bounds it; a working "
                f"pass stays under ~8·log8(flushes)"
            )
        # The dictionary axis is bounded like the other five. It ratcheted 6 -> 58 across 24 fold
        # cycles when this tier first ran, and the cause was not the tiering the first reading
        # blamed: a fold digest-names every file it carries into the new prefix's MANIFEST, and
        # the coalesce read "digest-named" as "the build's own artefact, never take", so each fold
        # froze every extent alive at its flip. Eligibility on this axis is positional now — only
        # the one extent a builder writes is spared — so a flat ceiling is the honest bound again.
        dicts = len(manifest["dict_extents"])
        assert dicts <= AXIS_CEILING, (
            f"{label}: `dict_extents` reached {dicts} entries (ceiling {AXIS_CEILING}) — the "
            f"coalesce has stopped bounding this axis, or a publication has frozen it again"
        )
        attrs = len(manifest["attr_extents"])
        assert attrs <= ATTR_AXIS_CEILING, (
            f"{label}: `attr_extents` reached {attrs} entries (ceiling {ATTR_AXIS_CEILING})"
        )
        segments = manifest["segments"]
        seg_count += len(segments)
        runs = len(manifest["external_id_runs"])
        locators = len(manifest["locator_extents"])
        assert runs <= len(segments) + LADDER_SLACK, (
            f"{label}: {runs} external-id runs against {len(segments)} segments — the merge "
            f"coalesces its inputs' runs, so runs must track the segment count"
        )
        assert locators <= len(segments) + LADDER_SLACK, (
            f"{label}: {locators} locator extents against {len(segments)} segments"
        )

    non_base = dict(view.segment_bytes)
    if non_base:
        base_id = max(non_base, key=non_base.get)
        del non_base[base_id]
    non_base_bytes = sum(non_base.values())
    ladder_allowance = non_base_bytes // SATURATION_BYTES + LADDER_SLACK
    assert len(non_base) <= ladder_allowance, (
        f"{label}: {len(non_base)} non-base segments hold {non_base_bytes} bytes — decision "
        f"0049's settle is flushed bytes over the {SATURATION_BYTES}-byte saturation size (plus "
        f"ladder slack {LADDER_SLACK}), and a count past it means the merge has stopped"
    )
    if quiescent:
        wire_segments = sum(s["count"] for s in status["segments"])
        assert wire_segments == seg_count, (
            f"{label}: /control/status counts {wire_segments} live segments where the "
            f"side-manifests name {seg_count}"
        )

    # -- allocator floor, watermark, WAL --------------------------------------------------------
    high_water = status["entity_id_high_water"]
    watermark = status["partitions"][0]["watermark"]
    assert high_water >= t.entity_high_water, (
        f"{label}: entity high-water went backwards ({t.entity_high_water} -> {high_water}) — "
        f"the allocator floor did not survive a rotation, and re-minted ids alias old rows"
    )
    assert watermark >= t.watermark, (
        f"{label}: the watermark went backwards ({t.watermark} -> {watermark}) — a publication "
        f"wrote a stale value into a side-manifest (the fixed merge-publication defect's shape, "
        f"module doc), and entity coverage between the two values goes invisible on the next boot"
    )
    t.entity_high_water = high_water
    t.watermark = watermark
    # The durable record may never trail the served one: every publication commits its manifest
    # *before* it swaps, so at any instant disc >= wire — a shortfall here is a manifest written
    # from plan-time state rather than live state.
    disc_watermark = min(m["watermark"] for m in view.latest.values())
    assert disc_watermark >= watermark, (
        f"{label}: the side-manifest watermark {disc_watermark} trails the live {watermark} — "
        f"the durable record under-reports, and a reboot serves it"
    )

    members = _wal_members(h.wal_path)
    wal_bytes = sum(m.stat().st_size for m in members)
    if quiescent:
        assert len(members) <= WAL_MEMBER_CEILING, (
            f"{label}: {len(members)} WAL members on disc (steady state is two) — rotation is "
            f"rotating without reclaiming, even though every disposition is restated at each "
            f"member's snapshot"
        )

    # -- overlay: the two removal rules ---------------------------------------------------------
    overlay = status["overlay"]
    expected_depth = len(t.suppressed) + t.pending_deletes
    assert overlay["depth"] == expected_depth, (
        f"{label}: overlay depth is {overlay['depth']} where the harness's own ledger says "
        f"{expected_depth} ({len(t.suppressed)} live suppressions + {t.pending_deletes} deletions "
        f"pending their fold) — a suppression retiring anywhere but unsuppress, or a deletion "
        f"anywhere but its fold, is a conflation of write-path §5.4's two removal rules"
    )
    assert overlay["retirable"] == t.pending_deletes, (
        f"{label}: {overlay['retirable']} retirable deletions where the ledger says "
        f"{t.pending_deletes}"
    )

    # -- nothing failed its way to a low number -------------------------------------------------
    flush = executor["flush"]
    assert flush["flush_failures"] == 0, f"{label}: flush failures: {flush}"
    assert executor["wal_recoveries"] == 0, (
        f"{label}: the WAL recovered {executor['wal_recoveries']} time(s) mid-run — denies were "
        f"answered 500 somewhere in this plan"
    )
    assert status["compaction"]["fold_failures"] == 0, (
        f"{label}: fold failures: {status['compaction']}"
    )

    row = {
        "label": label,
        "prefix": view.live_prefix,
        "prefixes": len(view.prefixes),
        "total_kb": view.total_bytes // 1024,
        "live_kb": view.live_bytes // 1024,
        "dead_b": view.cross_prefix_dead,
        "unref_kb": view.unreferenced_bytes // 1024,
        "segments": seg_count,
        "non_base": len(non_base),
        "non_base_kb": non_base_bytes // 1024,
        "deltas": sum(len(m["deltas"]) for m in view.latest.values()),
        "dicts": sum(len(m["dict_extents"]) for m in view.latest.values()),
        "attrs": sum(len(m["attr_extents"]) for m in view.latest.values()),
        "records": sum(len(m["record_extents"]) for m in view.latest.values()),
        "texts": sum(len(m["text_extents"]) for m in view.latest.values()),
        "runs": sum(len(m["external_id_runs"]) for m in view.latest.values()),
        "side_n": max((n for (_pfx, _p, n) in t.side_seen if _pfx == view.live_prefix), default=0),
        "overlay": f"{overlay['depth']}/{overlay['retirable']}",
        "alarms": overlay["soft_limit_alarms"],
        "wal": f"{len(members)}:{wal_bytes // 1024}k",
        "high_water": high_water,
        "flushes": flush["flushes"],
        "merges": executor["merges"],
        "coalesces": executor["coalesces"],
        "folds": status["compaction"]["folds"],
    }
    t.observations.append(row)
    return row


# -- driving ------------------------------------------------------------------------------------


class PipeDrainer:
    """Reads each spawned server's stdout/stderr pipe so the write executor cannot block on it.

    The driver pipes the server's output and never reads it, which is invisible to every short
    tier — their servers say less than one pipe buffer (64 KiB) in a lifetime. A long run does
    not: this tier's first three runs each wedged at the same operation count, the executor's
    thread (`tessera-lifecyc`) parked in `anon_pipe_write` mid-log, ticks never consumed again —
    found by reading `/proc/<pid>/task/*/wchan` of the wedged process. Draining here keeps the
    executor's logging non-blocking and keeps the tail as the failure report's witness; the
    driver-level fix (its spawn owning the draining, or logging to a file) is `driver.py`'s
    owner's to make. The faults build a [`Killed`] stage boots mid-stage lives and dies inside
    that stage and is not attached — one fold's logging does not reach the buffer's size.
    """

    def __init__(self) -> None:
        self._seen: set[int] = set()
        self._tails: dict[int, collections.deque] = {}
        self._current: int | None = None

    def attach(self, h: SuiteHarness) -> None:
        proc = h.proc
        if proc is None or proc.stdout is None:
            return
        self._current = proc.pid
        if proc.pid in self._seen:
            return
        self._seen.add(proc.pid)
        tail: collections.deque = collections.deque(maxlen=400)
        self._tails[proc.pid] = tail
        stream = proc.stdout

        def pump() -> None:
            for line in iter(stream.readline, b""):
                tail.append(line)

        threading.Thread(target=pump, daemon=True, name=f"server-drain-{proc.pid}").start()

    def tail_text(self) -> str:
        if self._current is None or self._current not in self._tails:
            return ""
        return b"".join(self._tails[self._current]).decode(errors="replace")


def pull_tick_patiently(h: SuiteHarness, attempts: int = 4) -> None:
    """`SuiteHarness.pull_tick`, retried a bounded number of times.

    Its single 30-second window is right for the short tiers; an endurance run shares its machine
    for hours, and one slow fsync under someone else's IO (a WAL rotation rides these ticks) must
    not fail the tier — that would be a resource result reported as a correctness result, the
    §7 mistake in miniature. Retrying re-reads the counter baseline, and an extra consumed tick
    is harmless (an empty-buffer tick is a pure maintenance dispatch). The bound keeps a
    genuinely wedged executor a loud failure rather than an infinite wait.
    """
    for attempt in range(attempts):
        try:
            h.pull_tick()
            return
        except TimeoutError:
            if attempt == attempts - 1:
                raise


def drain_maintenance(h: SuiteHarness, max_ticks: int = 60) -> None:
    """Pull ticks until the merge and coalesce counters stop moving — the soak's settle loop,
    over the wire. Empty-buffer ticks are pure maintenance dispatches (driver module doc)."""
    quiet = 0
    for _ in range(max_ticks):
        before = h.executor()
        pull_tick_patiently(h)
        time.sleep(0.25)
        after = h.executor()
        if (after["merges"], after["coalesces"]) == (before["merges"], before["coalesces"]):
            quiet += 1
            if quiet >= 3:
                return
        else:
            quiet = 0
    raise RuntimeError(f"maintenance did not settle within {max_ticks} ticks")


def await_reclaim(h: SuiteHarness, timeout: float = 60.0) -> None:
    """Reclamation runs at the executor's tick, once no request holds the superseded generation;
    pull ticks until one prefix remains."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        prefixes = [
            d
            for d in h.bundle_root.iterdir()
            if d.is_dir() and _PREFIX_RE.fullmatch(d.name)
        ]
        if len(prefixes) == 1:
            return
        pull_tick_patiently(h)
        time.sleep(0.3)
    raise TimeoutError(
        "the superseded prefix was never reclaimed — a fold's orphan is accumulating"
    )


def run_and_check(
    h: SuiteHarness, stages: list[Stage], claims: dict[str, Delta], drainer: PipeDrainer
):
    """One `run_plan` chunk, every stage judged as it lands; returns the final stage's result so
    a sampled cycle can verify the freshest recording without recording again. The drainer is
    re-attached after every chunk — a chunk ending in a reload or a kill leaves a new server
    process whose pipe must be read before it can fill (its class doc carries the incident)."""
    last = None
    for result in run_plan(h, stages):
        check(result, claimed=claims.get(result.label))
        last = result
    drainer.attach(h)
    return last


def deep_verify(bundle_root: Path) -> str:
    """`tessera verify <bundle> --deep` — §12.4's full re-hash, run between stages while the plan
    is quiescent (nothing publishes concurrently, which is the reader-safety §12.4 asks for)."""
    run = subprocess.run(
        [str(CLI_BIN), "verify", str(bundle_root), "--deep"],
        capture_output=True,
        text=True,
    )
    assert run.returncode == 0, (
        f"deep verification refused the live bundle:\n{run.stdout}\n{run.stderr}"
    )
    return run.stdout.strip().splitlines()[-1]


def total_verify(
    recorded,
    *,
    p: Params,
    declaration: Declaration,
    t: Tracker,
    h: SuiteHarness,
    n_total: int,
    label: str,
) -> tuple[int, int]:
    """§9's two halves over one recording: every served row at its own identity, and every
    recorded count surface against the generator's census minus the harness's accepted denies."""
    rows = verify_rows(
        recorded,
        seed=p.seed,
        declaration=declaration,
        denied_fx=t.denied_fx(),
        fx_of_tessera=h.fx_by_tessera,
    )
    denied = [*t.suppressed, *t.deleted]
    denied_expected = expected_items(p.seed, [r.fx for r in denied]) if denied else {}
    denied_terms = {r.e: r.terms for r in denied}
    census_cache: dict[int, dict[int, int]] = {}

    def expected_at(depth: int) -> dict[int, int]:
        if depth not in census_cache:
            census = expected_census(p.seed, n_total, depth, GRANT_TERMS)
            census_cache[depth] = subtract_denies(
                census,
                [denied_expected[r.fx] for r in denied],
                denied_terms,
                GRANT_TERMS,
                depth,
            )
        return census_cache[depth]

    tiles = 0
    for query, canon in recorded.items():
        if not isinstance(query, Viewport):
            continue
        name = f"{label} zoom={query.zoom} filters={'y' if query.filters else 'n'}"
        tiles += verify_census(tile_visible(canon), expected_at(query.zoom), label=name)
        if query.underlay_offset:
            depth = query.zoom + query.underlay_offset
            tiles += verify_census(
                underlay_counts(canon), expected_at(depth), label=f"{name} underlay@{depth}"
            )
    return rows, tiles


# -- the run ------------------------------------------------------------------------------------

ENDURANCE_OPTED_IN = os.environ.get("TESSERA_SUITE_ENDURANCE") == "1"


@pytest.mark.skipif(
    not ENDURANCE_OPTED_IN,
    reason=(
        "the endurance tier is a backstop, never a gate (correctness-suite §6/§13): "
        "opt in with TESSERA_SUITE_ENDURANCE=1"
    ),
)
def test_endurance_long_life(tmp_path_factory):
    p = Params.from_env()
    assert p.base >= 60_000, (
        f"TESSERA_SUITE_BASE={p.base}: the suite config's merge cap ({SATURATION_BYTES} bytes) "
        f"must sit below the base segment, and a base this small builds one under it — the "
        f"server would refuse the config at boot, so refuse here with the reason instead"
    )
    assert p.ladder_writes >= 8, (
        f"rounds={p.rounds} folds={p.folds} leaves a {p.ladder_writes}-write ladder phase — too "
        f"short for the merge ladder to exercise at all; raise rounds or lower folds"
    )

    work = tmp_path_factory.mktemp("endurance")
    files = materialise_corpus(p.seed, p.base, work / "corpus", ingest=p.ingest_range)
    declaration = Declaration.load(files.schema)
    bundle_root = work / "bundle"
    build_bundle(files, bundle_root)

    with ipc.open_stream(io.BytesIO(files.ingest.read_bytes())) as reader:
        ingest_table = reader.read_all().combine_chunks()
    assert ingest_table.num_rows == p.rounds * p.rows

    grant = set(GRANT_TERMS)
    claims: dict[str, Delta] = {}
    #: (fx, e, terms) per ingested row visible to the recording principal, in write order —
    #: the pool the delete-an-ingested-item stages draw from.
    ingested_visible: list[DenyRecord] = []

    def write_stage(i: int) -> Write:
        window = ingest_table.slice(i * p.rows, p.rows).combine_chunks()
        sink = io.BytesIO()
        with ipc.new_stream(sink, window.schema) as writer:
            for batch in window.to_batches():
                writer.write_batch(batch)
        fx_keys = window.column("fx_key").to_pylist()
        access = window.column("access").to_pylist()
        visible = []
        for offset, (fx, labels) in enumerate(zip(fx_keys, access)):
            # One label per list element, verbatim (decision 0129).
            terms = frozenset(int(label) for label in labels)
            if terms & grant:
                visible.append(fx)
                ingested_visible.append(
                    DenyRecord(fx=fx, e=p.base + i * p.rows + offset, terms=terms)
                )
        label = f"w{i}"
        claims[label] = Rows(visible)
        return Write(label, sink.getvalue(), f"endurance-{i}", fx_keys)

    h = SuiteHarness(
        bundle_root=bundle_root,
        run_dir=tmp_path_factory.mktemp("endurance-run"),
        grants=GRANTS,
        view_id=VIEW_ID,
        bbox=BBOX,
        k=K,
        filters={"bay": {"eq": "cedar"}},
    )
    t = Tracker()
    drainer = PipeDrainer()
    deep_reports: list[str] = []
    sample_reports: list[str] = []
    write_cursor = 0
    ingested_cursor = 0

    def next_writes(count: int) -> list[Write]:
        nonlocal write_cursor
        stages = [write_stage(i) for i in range(write_cursor, write_cursor + count)]
        write_cursor += count
        return stages

    started = time.monotonic()
    try:
        run_and_check(h, [Build()], claims, drainer)

        # Deny planning needs the establishment recording: the built pool is every item the
        # principal was served, minus the battery's own drill-down items (which must stay alive
        # for the whole run so their surface keeps answering 200).
        battery_fx = {h.fx_by_tessera[i] for i in h.item_ids}
        pool = sorted(set(h.fx_by_tessera.values()) - battery_fx)
        need = 2 * p.folds + 4
        assert len(pool) >= need, (
            f"only {len(pool)} visible non-battery items for {need} planned deny targets — "
            f"widen the grant or grow the base"
        )
        pool_expected = expected_items(p.seed, pool[:need])
        pool_terms = terms_of(files, [pool_expected[fx].e for fx in pool[:need]])
        built_targets = iter(
            DenyRecord(fx=fx, e=pool_expected[fx].e, terms=pool_terms[pool_expected[fx].e])
            for fx in pool[:need]
        )

        def deny(op: str, record: DenyRecord, label: str) -> Deny:
            b64 = base64.b64encode(record.e.to_bytes(8, "little")).decode()
            return Deny(op, lambda _h, b64=b64, fx=record.fx: (b64, fx), label=label)

        # -- the ladder phase: writes only, no fold, the merge left to find its settle ----------
        chunk_size = 8
        while write_cursor < p.ladder_writes:
            count = min(chunk_size, p.ladder_writes - write_cursor)
            run_and_check(h, next_writes(count), claims, drainer)
            observe(h, t, p, f"ladder-{write_cursor}", quiescent=False)
        drain_maintenance(h)
        settle = observe(h, t, p, "ladder-settle", quiescent=True)
        if settle["non_base_kb"] * 1024 >= 2 * SATURATION_BYTES:
            assert settle["non_base"] >= 2, (
                f"the ladder holds {settle['non_base_kb']} KiB in {settle['non_base']} non-base "
                f"segment(s) — over the saturation size everything may not collapse into one "
                f"segment (decision 0049: the cap is what keeps a merge from re-copying the "
                f"corpus, and the settle is the evidence it is honoured)"
            )

        # -- the fold cycles --------------------------------------------------------------------
        # The kill lands mid-run, on a cycle that carries a deletion (`| 1` forces an odd cycle),
        # so an accepted deletion demonstrably survives the killed fold still pending and is
        # retired by the next one — Rule F held across a crash.
        kill_cycle = min(p.folds - 1, (p.folds // 2) | 1) if p.kill else -1
        load_cycles = {p.folds // 3, (2 * p.folds) // 3}
        for c in range(p.folds):
            stages: list[Stage] = next_writes(p.writes_per_cycle)

            suppression = next(built_targets)
            stages.append(deny("suppress", suppression, f"suppress-{c}"))
            deletion = None
            if c % 2 == 1:
                if c % 4 == 1 and ingested_cursor < len(ingested_visible):
                    deletion = ingested_visible[ingested_cursor]
                    ingested_cursor += 1
                else:
                    deletion = next(built_targets)
                stages.append(deny("delete", deletion, f"delete-{c}"))
            unsuppression = None
            if c % 4 == 3 and t.suppressed:
                unsuppression = t.suppressed[0]
                stages.append(deny("unsuppress", unsuppression, f"unsuppress-{c}"))
            if c % 4 == 2:
                stages.append(Rotate())
            if c in load_cycles:
                stages.append(Load(f"reload-{c}"))

            run_and_check(h, stages, claims, drainer)
            t.suppressed.append(suppression)
            if deletion is not None:
                t.deleted.append(deletion)
                t.pending_deletes += 1
            if unsuppression is not None:
                t.suppressed.remove(unsuppression)

            drain_maintenance(h)
            version_before = t.live_prefix_version
            if c == kill_cycle:
                # §10.1's modifier at the fold's own seam: park before the `CURRENT` flip,
                # SIGKILL, discard what a power cut would have taken, restart. The fold does not
                # land; its deletions stay pending for the next cycle's fold, which is Rule F
                # observed across a crash.
                last = run_and_check(
                    h, [Killed(Fold(f"fold-{c}"), "before_current_flip")], claims, drainer
                )
            else:
                last = run_and_check(h, [Fold(f"fold-{c}")], claims, drainer)
                t.folds_landed += 1
                t.pending_deletes = 0
            await_reclaim(h)
            landed = "killed" if c == kill_cycle else "landed"
            row = observe(h, t, p, f"fold-{c} ({landed})", quiescent=True)
            if c == kill_cycle:
                assert t.live_prefix_version == version_before, (
                    f"the killed fold flipped CURRENT anyway ({row['prefix']}) — the kill did "
                    f"not land before the flip"
                )
            else:
                assert t.live_prefix_version > version_before, (
                    f"fold-{c} landed but the live prefix did not advance ({row['prefix']})"
                )

            if c % p.verify_every == p.verify_every - 1 or c == kill_cycle:
                rows, tiles = total_verify(
                    last.after,
                    p=p,
                    declaration=declaration,
                    t=t,
                    h=h,
                    n_total=p.base + write_cursor * p.rows,
                    label=f"fold-{c}",
                )
                sample_reports.append(f"fold-{c}: {rows} rows, {tiles} tiles verified")
                deep_reports.append(f"fold-{c}: {deep_verify(h.bundle_root)}")

        # -- the end: reopen, verify totally, re-hash -------------------------------------------
        assert write_cursor == p.rounds
        final = run_and_check(h, [Load("final-reload")], claims, drainer)
        rows, tiles = total_verify(
            final.after,
            p=p,
            declaration=declaration,
            t=t,
            h=h,
            n_total=p.base + p.rounds * p.rows,
            label="final",
        )
        sample_reports.append(f"final: {rows} rows, {tiles} tiles verified")
        deep_reports.append(f"final: {deep_verify(h.bundle_root)}")
        observe(h, t, p, "final", quiescent=True)

        # The wire's fold counter is process-local and the final reload reset it; the harness's
        # own ledger is what says every requested fold landed.
        assert t.folds_landed == p.folds - (1 if p.kill else 0)
        assert t.entity_high_water >= p.base + p.rounds * p.rows, (
            f"entity high-water {t.entity_high_water} is below the {p.base + p.rounds * p.rows} "
            f"items this run fed the server — entities were lost or the floor slid back"
        )
        assert t.side_global_max >= p.rounds, (
            f"the side-manifest sequence reached only {t.side_global_max} across {p.rounds} "
            f"writes — every write publishes at least one manifest, so publications were lost"
        )

    except BaseException:
        # For a liveness failure (an executor that stops consuming ticks) the server's own
        # output is the only witness — a panic in a background thread lands there and nowhere
        # else. The drainer has been reading it all along; print its tail before teardown.
        said = drainer.tail_text()
        if said:
            print(f"\n--- server output at failure (tail) ---\n{said[-8000:]}")
        else:
            print("\n--- server output at failure: nothing said ---")
        raise
    finally:
        elapsed = time.monotonic() - started
        h.stop()
        _print_report(p, t, deep_reports, sample_reports, elapsed)


def _print_report(
    p: Params, t: Tracker, deep_reports: list[str], sample_reports: list[str], elapsed: float
) -> None:
    print(
        f"\nendurance: base={p.base} rounds={p.rounds} rows={p.rows} folds={p.folds} "
        f"seed={p.seed} verify=every:{p.verify_every} kill={p.kill} "
        f"({elapsed:.0f}s, {len(t.observations)} observations)"
    )
    columns = [
        "label", "prefix", "total_kb", "dead_b", "unref_kb", "segments", "non_base",
        "non_base_kb", "deltas", "dicts", "attrs", "records", "texts", "runs", "side_n",
        "overlay", "wal", "high_water", "flushes", "merges", "coalesces", "folds",
    ]
    if t.observations:
        widths = {
            c: max(len(c), *(len(str(row.get(c, ""))) for row in t.observations))
            for c in columns
        }
        print("  ".join(c.rjust(widths[c]) for c in columns))
        for row in t.observations:
            print("  ".join(str(row.get(c, "")).rjust(widths[c]) for c in columns))
    for line in sample_reports:
        print(f"total verification — {line}")
    for line in deep_reports:
        print(f"deep verify — {line}")
    if t.findings:
        shown = t.findings[:12]
        print(f"findings ({len(t.findings)}):")
        for line in shown:
            print(f"  - {line}")
        if len(t.findings) > len(shown):
            print(f"  … and {len(t.findings) - len(shown)} more of the same shape")


# -- always-on checks (no server, no corpus) ----------------------------------------------------


def test_the_defaults_are_the_design_tier():
    """§16's endurance row, transcribed: thousands of writes, 100+ folds, sampled verification —
    so an untouched environment runs the real thing, and a developing run must shrink it
    explicitly. Runs in every suite invocation (it costs nothing) so drift between this module
    and the design's tier table is caught without opting in."""
    saved = {
        k: os.environ.pop(k)
        for k in list(os.environ)
        if k.startswith("TESSERA_SUITE_") and k != "TESSERA_SUITE_ENDURANCE"
    }
    try:
        p = Params.from_env()
    finally:
        os.environ.update(saved)
    assert p.rounds >= 1000 and p.folds >= 100
    assert p.verify_every > 1, "deep verification every fold would be the whole run (§6)"
    assert p.kill, "row 9 carries a kill at a sampled fold by default"
    assert p.rounds == p.ladder_writes + p.writes_per_cycle * p.folds


def test_the_verify_cadence_grammar():
    assert parse_verify_cadence("every:25") == 25
    assert parse_verify_cadence("each") == 1
    assert parse_verify_cadence("10") == 10
    with pytest.raises(ValueError):
        parse_verify_cadence("every:0")
