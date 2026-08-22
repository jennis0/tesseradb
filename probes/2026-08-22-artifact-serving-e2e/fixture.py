"""Stage one of a tier: materialise, author the boundary geometry, build, verify, delete the inputs.

**One tier on disk at a time.** The materialised inputs for 10⁹ points are ~136 GB and the box has
149 GB free, so the inputs go the moment the bundle opens — and every step checks the floor first
and stops rather than filling the disk (`campaign.require_disk`).

Run: `python3 fixture.py --work DIR --n N [--seed S] [--keep-inputs]`
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import time
from pathlib import Path

import campaign as C

SEED = 20_260_822
TERMS_PER_LEVEL = 65_536


#: The design-ceiling layer: an attribute predicate over `weight`.
#:
#: **This is how the campaign reaches 10⁷ artifacts, and it is not a trick.** The generator scales
#: every closed-form arm at one artifact per hundred points, so 10⁷ artifacts would want 10⁹ points
#: and that tier does not fit on this box (the README's disk arithmetic). `weight` is a `u32` drawn
#: from a keyed mix, so at 10⁷ rows it carries very nearly 10⁷ distinct values — and an attribute
#: layer mints one artifact per distinct value. One layer, ten million artifacts, over a corpus a
#: hundred times smaller than the one that would otherwise be needed.
#:
#: It has **no census**: nothing in `tessera-corpus` states this relation, so the ceiling probe
#: reports latency and residency and claims nothing about correctness. That is why it is a separate
#: build rather than a sixth layer on the tier bundles — a layer with no oracle should not sit
#: beside five that have one.
CEILING_LAYER_TOML = """
# The design-ceiling probe (campaign README §7): an attribute predicate over a keyed `u32`, which
# at 10^7 rows has very nearly 10^7 distinct values and therefore very nearly 10^7 artifacts.
[[layer]]
name                      = "campaign/ceiling"
views                     = ["s0"]
membership                = { attribute = "weight" }
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
"""


def assemble_config(fixture: Path, ceiling: bool = False) -> Path:
    """The campaign's declaration: the generator's own, plus the spatial layer whose boxes
    `artifact_campaign_fixture` authored, and optionally the design-ceiling layer.

    Appended rather than rewritten — the generator's five `[[layer]]` blocks are the fixture, and
    the sixth exists only because the spatial arm's roster carries prefixes and no geometry.
    """
    base = (fixture / "corpus-config.toml").read_text()
    extra = (fixture / "boundary-layer.toml").read_text()
    out = fixture / "campaign-config.toml"
    out.write_text(base + "\n" + extra + (CEILING_LAYER_TOML if ceiling else ""))
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--n", required=True, type=int)
    ap.add_argument("--seed", type=int, default=SEED)
    ap.add_argument("--terms-per-level", type=int, default=TERMS_PER_LEVEL)
    ap.add_argument("--keep-inputs", action="store_true")
    ap.add_argument(
        "--ceiling",
        action="store_true",
        help="add the design-ceiling layer — an attribute predicate over `weight`, ~10^7 artifacts",
    )
    ap.add_argument(
        "--no-measure",
        action="store_true",
        help="skip the ladder's O(n) measuring pass (the 10^9 tier, where it costs minutes)",
    )
    args = ap.parse_args()

    work: Path = args.work
    work.mkdir(parents=True, exist_ok=True)
    C.require_disk(work)
    report: dict = {"seed": args.seed, "n": args.n, "terms_per_level": args.terms_per_level}

    started = time.monotonic()
    proc = subprocess.run(
        ["/usr/bin/time", "-f", "%e %M", str(C.CLI), "corpus", "materialise",
         "--seed", str(args.seed), "--n", str(args.n), "--out", str(work / "fixture"),
         "--terms-per-level", str(args.terms_per_level)],
        capture_output=True, text=True, env={**__import__("os").environ},
    )
    if proc.returncode != 0:
        raise SystemExit(f"materialise failed:\n{proc.stdout}\n{proc.stderr}")
    wall, rss = proc.stderr.strip().splitlines()[-1].split()
    report["materialise"] = {
        "seconds": float(wall),
        "peak_rss_bytes": int(rss) * 1024,
        "report": proc.stdout.strip(),
        "input_bytes": sum(p.stat().st_size for p in (work / "fixture").iterdir()),
    }
    print(f"materialised in {wall}s, {report['materialise']['input_bytes'] / 1024**3:.2f} GB of inputs")

    fixture_bin = C.REPO_ROOT / "target" / "release" / "artifact_campaign_fixture"
    argv = [str(fixture_bin), "--seed", str(args.seed), "--n", str(args.n),
            "--terms-per-level", str(args.terms_per_level), "--out", str(work / "fixture")]
    if args.no_measure:
        argv.append("--no-measure")
    C.run(argv)
    report["fixture"] = json.loads((work / "fixture" / "fixture.json").read_text())
    report["fixture"].pop("grants", None)  # the grants live in their own file; they are large

    assemble_config(work / "fixture", ceiling=args.ceiling)
    C.require_disk(work)
    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)

    proc = subprocess.run(
        ["/usr/bin/time", "-f", "%e %M", str(C.CLI), "build", "--deployment", str(work / "tessera.toml")],
        cwd=work, capture_output=True, text=True,
        env={**__import__("os").environ, "TESSERA_IDENTITY_KEY": C.IDENTITY_KEY},
    )
    if proc.returncode != 0:
        raise SystemExit(f"build failed:\n{proc.stdout}\n{proc.stderr[-4000:]}")
    wall, rss = proc.stderr.strip().splitlines()[-1].split()
    bundle_bytes = sum(p.stat().st_size for p in (work / "bundle").rglob("*") if p.is_file())
    report["build"] = {
        "seconds": float(wall),
        "peak_rss_bytes": int(rss) * 1024,
        "bundle_bytes": bundle_bytes,
        "report": proc.stdout.strip(),
    }
    print(f"built in {wall}s, peak RSS {int(rss) / 1024**2:.1f} GB, bundle {bundle_bytes / 1024**3:.2f} GB")

    # The bundle verifies by being served: a server that boots and answers `/v1/meta` with the six
    # layers has opened every one of them. Cheaper than a second pass over the store and it is the
    # property that matters — the inputs are about to be deleted.
    server = C.Server(work, *ports)
    server.spawn()
    try:
        token, _ = server.authorise(["0"])
        layers = [layer["name"] for layer in server.meta(token).get("layers", [])]
        report["layers_served"] = layers
        print("layers:", layers)
    finally:
        server.stop()

    if not args.keep_inputs:
        shutil.rmtree(work / "fixture" / "points.parquet", ignore_errors=True)
        for name in ("points.parquet", "pairs.parquet", "flat_members.parquet",
                     "partition_members.parquet", "treed_members.parquet"):
            (work / "fixture" / name).unlink(missing_ok=True)
        report["inputs_deleted"] = True
        print(f"inputs deleted; {C.free_bytes(work) / 1024**3:.1f} GB free")

    report["elapsed_seconds"] = time.monotonic() - started
    (work / "fixture-report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items() if k != "fixture"}, indent=2)[:2000])


if __name__ == "__main__":
    main()
