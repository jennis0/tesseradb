"""One rung's whole workload: build it, verify it, serve it, ingest into it, and say what happened.

Assembly, and nothing else. Every figure is the figure the driver that measured it wrote — the
build's own stage timings, `serve_battery`'s result, `ingest_cycle`'s result — kept whole in the run
file under its own key. What this module decides is the order, the ports, the cap, and the two
sections a rerun is read for: whether the run held, and what it cost beside the last run of the same
rung.

    python3 -m test_corpora.common.workload --rung arxiv --work <scratch> [--quick]

`--quick` is the shape for a change under review: three zooms, three deciles, ten samples a cell,
and a 2% hold-out. Full mode is every driver's own default and a 10% hold-out. The two are compared
only against runs of the same shape.

The exit code is the correctness section's: a refused check, a failed verify, an OOM kill, a dead
server, a failed request, an unequal census or a rejected batch each make it non-zero. The cost
section never touches it, because a timing on this machine depends on what else is running.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Sequence

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.10 on this box
    import tomli as tomllib

from . import serve_battery
from .deployment import minted_credentials, read_env_file
from .ingest_cycle import build_bundle, ranks_file
from .ingest_cycle import main as ingest_cycle_main
from .paths import ladder
from .timing import Steps

#: `MemoryMax` on every deployment this run serves.
CAP_BYTES = 16 * 1024**3

#: The checkout this module lives in, which is where the binary is built and the bench is run.
CHECKOUT = Path(__file__).resolve().parents[2]


def git(*args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(CHECKOUT), *args], capture_output=True, text=True
    ).stdout.strip()


def run(command: Sequence[str], cwd: Path, env: dict[str, str] | None = None) -> dict:
    """One child process, and what it said. Never raises: a refusal is a result here."""
    proc = subprocess.run(
        [str(part) for part in command], cwd=cwd, env=env, capture_output=True, text=True
    )
    if proc.returncode != 0:
        print(proc.stdout[-2000:] + proc.stderr[-2000:], flush=True)
    return {
        "returncode": proc.returncode,
        "stdout_tail": proc.stdout[-2000:],
        "stderr_tail": proc.stderr[-2000:],
    }


def text_column(rung_dir: Path) -> str | None:
    """The rung's first indexed text attribute — what the battery's `match` asks on.

    Read from the declaration rather than defaulted: arXiv indexes `title` and GeoNames `name`, and
    a `match` on a column a rung does not declare is a failed request in every cell.
    """
    declared = tomllib.loads((rung_dir / "corpus.toml").read_text())
    for attribute in declared.get("attribute", []):
        if attribute.get("type") == "text" and attribute.get("index"):
            return attribute["name"]
    return None


def dig(node, *path, default=None):
    """`node[path[0]][path[1]]…`, or `default` where any step is absent. An absent figure is
    reported as absent and never as a zero."""
    for key in path:
        if not isinstance(node, dict):
            return default
        node = node.get(key)
    return default if node is None else node


# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------


def cargo_release_binary() -> Path:
    """`cargo build --release -p tessera-cli`, and the binary it wrote."""
    env = dict(os.environ, CARGO_PROFILE_RELEASE_DEBUG="0")
    built = run(
        ["cargo", "build", "--release", "-p", "tessera-cli"], CHECKOUT, env=env
    )
    if built["returncode"] != 0:
        raise SystemExit(f"cargo build --release -p tessera-cli failed:\n{built['stderr_tail']}")
    return CHECKOUT / "target" / "release" / "tessera"


def battery_argv(args, rung_dir: Path, bundle: Path, work: Path, binary: Path) -> list[str]:
    argv = [
        "--boot-rung", str(rung_dir),
        "--boot-bundle", str(bundle),
        "--boot-scratch", str(work / "serve-scratch"),
        "--boot-binary", str(binary),
        "--boot-port0", str(args.port0),
        "--cap-bytes", str(args.cap_bytes),
        "--ranks", str(ranks_file(rung_dir)),
        "--out", str(work / "serve.json"),
    ]
    column = text_column(rung_dir)
    argv += ["--text-column", column] if column else ["--text-samples", "0"]
    if args.quick:
        argv += [
            "--zooms", "0,3,6",
            "--deciles", "0,5,9",
            "--cells-per-decile", "1",
            "--samples", "10",
        ]
    return argv


def cycle_argv(args, rung_dir: Path, bundle: Path, work: Path, binary: Path) -> list[str]:
    return [
        "--rung-dir", str(rung_dir),
        "--work", str(work / "cycle"),
        "--binary", str(binary),
        "--out", str(work / "cycle.json"),
        "--all-in-bundle", str(bundle),
        "--write-cycle",
        "--state-extent",
        # The battery holds `port0`..`port0 + 2`; a cycle serves the folded deployment and the
        # all-in one, so it takes six consecutive ports of its own.
        "--port0", str(args.port0 + 3),
        "--cap-bytes", str(args.cap_bytes),
        "--fraction", "0.02" if args.quick else "0.10",
        "--write-cycle-n", "200" if args.quick else "1000",
    ]


# ---------------------------------------------------------------------------------------------
# The report
# ---------------------------------------------------------------------------------------------


def per_view(views: dict, key: str) -> tuple[bool, str]:
    """Whether every view's `key` held, and each view's verdict named."""
    held = bool(views) and all(view.get(key) for view in views.values())
    said = ", ".join(
        f"{name} {'equal' if view.get(key) else 'DIFFERS'}" for name, view in views.items()
    )
    return held, said or "no view"


def correctness(result: dict) -> list[tuple[str, bool, str]]:
    """One `(check, held, the number that decides it)` per line of the correctness section."""
    rows = []
    for step, label in (("check", "tessera check"), ("build", "build"), ("verify", "verify --deep")):
        code = dig(result, step, "returncode", default=None)
        rows.append((f"{label} clean", code == 0, f"exit {'n/a' if code is None else code}"))

    serve = result.get("serve")
    if serve is None:
        rows.append(("serve battery ran", False, "no result"))
    else:
        rows += [
            ("battery no OOM kill", not serve.get("oom_kill_seen"), f"{bool(serve.get('oom_kill_seen'))}"),
            ("battery server alive", not serve.get("died"), dig(serve, "died", "during", default="alive")),
            ("battery no failed request", not serve.get("request_failures"), f"{serve.get('request_failures')} failed"),
        ]

    cycle = result.get("ingest")
    if cycle is None:
        rows.append(("ingest cycle ran", False, "no result"))
        return rows
    views = dig(cycle, "equivalence", "views", default={})
    written, expected = (
        dig(cycle, "write_cycle", "visible_after_cycle", default="n/a"),
        dig(cycle, "write_cycle", "expected_after_cycle", default="n/a"),
    )
    after, before = (
        dig(cycle, "restart", "visible", default="n/a"),
        dig(cycle, "restart", "visible_before", default="n/a"),
    )
    batches = cycle.get("ingest_by_view") or {}
    rows += [
        ("census equal per view", *per_view(views, "equal")),
        ("artifact parents equal per view", *per_view(views, "parents_equal")),
        ("write cycle counts", written == expected, f"{written} visible against {expected} expected"),
        (
            "restart equal",
            bool(dig(cycle, "restart", "census_equal")) and after == before,
            f"{after} visible after the restart against {before} before",
        ),
        (
            "every ingest batch accepted",
            bool(batches)
            and all(v.get("accepted") == v.get("rows_offered") for v in batches.values()),
            ", ".join(f"{name} {v.get('accepted')}/{v.get('rows_offered')}" for name, v in batches.items())
            or "no view",
        ),
        ("cycle reported no failure", not cycle.get("failures"), f"{len(cycle.get('failures') or [])} sentence(s)"),
    ]
    return rows


def cost(result: dict) -> dict[str, float | None]:
    """The run's cost figures, flat and named, in the order the section prints them."""
    serve, cycle = result.get("serve") or {}, result.get("ingest") or {}
    principal = next((r for r in serve.get("ladder") or [] if r.get("target", 0) >= 1.0), {})
    build_peak_kib = dig(result, "build", "peak_rss_kib")
    figures: dict[str, float | None] = {
        "build wall (s)": dig(result, "build", "wall_s"),
        "build peak RSS (MiB)": build_peak_kib / 1024 if build_peak_kib else None,
        "bundle bytes": dig(result, "build", "bundle_bytes"),
        "verify wall (s)": dig(result, "verify", "wall_s"),
        "server open (s)": serve.get("open_s"),
    }
    cells = principal.get("cells") or []
    for zoom in sorted({cell["zoom"] for cell in cells}):
        at_zoom = [cell["conditions"]["hot"]["wall_ms"] for cell in cells if cell["zoom"] == zoom]
        # The median of the cells' own p50s, and the worst cell's p99: a battery-level percentile is
        # over cells rather than over pooled samples (`serve_battery.battery_figures`).
        p50s = [hot["p50"] for hot in at_zoom if hot["p50"] is not None]
        p99s = [hot["p99"] for hot in at_zoom if hot["p99"] is not None]
        figures[f"hot p50 z{zoom} (ms)"] = statistics.median(p50s) if p50s else None
        figures[f"hot p99 z{zoom} (ms)"] = max(p99s) if p99s else None
    figures["first viewport (s)"] = principal.get("first_viewport_s")
    for view, block in (cycle.get("ingest_by_view") or {}).items():
        figures[f"ingest {view} (items/s)"] = block.get("items_per_s")
    fold_peak = dig(cycle, "fold", "fold_peak_rss_bytes")
    figures.update(
        {
            "publication (members/s)": dig(cycle, "publish", "totals", "members_per_s"),
            "flush to visible (s)": dig(cycle, "flush", "visibility_s"),
            "fold wall (s)": dig(cycle, "fold", "fold_s"),
            "fold peak RSS (MiB)": fold_peak / 1024**2 if fold_peak else None,
            "restart open (s)": dig(cycle, "restart", "open_s"),
            "total wall (s)": result.get("total_wall_s"),
        }
    )
    return figures


def figure(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value:,.0f}" if abs(value) >= 10_000 else f"{value:,.2f}"


def earlier_result(results_dir: Path, rung: str, quick: bool, this_file: Path) -> dict | None:
    """The most recent earlier run of the same rung and the same shape."""
    runs = []
    for path in sorted(results_dir.glob("*.json")):
        if path == this_file:
            continue
        try:
            earlier = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if earlier.get("rung") == rung and bool(earlier.get("quick")) == quick:
            runs.append((earlier.get("started_at") or "", earlier))
    return max(runs, key=lambda pair: pair[0])[1] if runs else None


def report(result: dict, results_dir: Path, this_file: Path) -> int:
    rows = correctness(result)
    print("\nCorrectness")
    for check, held, number in rows:
        print(f"  {'pass' if held else 'FAIL'}  {check}: {number}")
    for sentence in result["failures"]:
        print(f"  FAILED: {sentence}")

    earlier = earlier_result(results_dir, result["rung"], result["quick"], this_file)
    here, before = cost(result), cost(earlier) if earlier else {}
    print("\nCost (never affects the exit code: a timing here depends on what else the machine is doing)")
    if earlier is None:
        print("  this run is the first of this rung and shape in the results directory")
        for name, value in here.items():
            print(f"  {name:28} {figure(value):>14}")
    else:
        print(f"  against {earlier.get('started_at')} at {(earlier.get('commit') or '')[:12]}")
        print(f"  {'figure':28} {'this run':>14} {'earlier':>14} {'change':>9}")
        for name, value in here.items():
            was = before.get(name)
            change = "n/a"
            if value is not None and was:
                change = format((value - was) / was * 100, "+.1f") + "%"
            print(f"  {name:28} {figure(value):>14} {figure(was):>14} {change:>9}")
    return 1 if any(not held for _, held, _ in rows) else 0


# ---------------------------------------------------------------------------------------------
# The entry point
# ---------------------------------------------------------------------------------------------


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="workload", description=__doc__.splitlines()[0])
    ap.add_argument("--rung", required=True, help="a rung directory's name under $TESSERA_LADDER")
    ap.add_argument("--work", required=True, help="scratch for the bundle, the caches and the WALs")
    ap.add_argument("--quick", action="store_true", help="the shape for a change under review")
    ap.add_argument("--binary", help="a tessera binary; built from this checkout when absent")
    ap.add_argument("--cap-bytes", type=int, default=CAP_BYTES)
    ap.add_argument("--port0", type=int, default=8171)
    ap.add_argument("--results", help="default $TESSERA_LADDER/<rung>/workload-results/")
    args = ap.parse_args(argv)

    rung_dir = ladder(args.rung)
    work = Path(args.work).resolve() / args.rung
    work.mkdir(parents=True, exist_ok=True)
    results_dir = Path(args.results) if args.results else rung_dir / "workload-results"
    results_dir.mkdir(parents=True, exist_ok=True)
    # Ignored by git, or outside a repository (128). Tracked is the one answer that stops the run.
    ignored = subprocess.run(
        ["git", "-C", str(results_dir), "check-ignore", "-q", str(results_dir)],
        capture_output=True,
    ).returncode
    if ignored == 1:
        raise SystemExit(
            f"{results_dir} is inside a git repository and is not ignored, so a run would commit "
            f"a box-specific figure; pass --results pointing somewhere ignored"
        )
    # The rung's own `.env` for the identity key, and a value for any credential variable neither
    # it nor the environment carries. Both drivers boot from the environment of this process.
    os.environ.update(read_env_file(rung_dir / ".env"))
    minted = minted_credentials(rung_dir)
    os.environ.update(minted)

    steps = Steps()
    failures: list[str] = []
    result: dict = {
        "rung": args.rung,
        "quick": bool(args.quick),
        "started_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "host": socket.gethostname(),
        "minted_credentials": sorted(minted),
        "steps": steps,
    }

    with steps.step("binary"):
        binary = Path(args.binary).resolve() if args.binary else cargo_release_binary()
    result["commit"] = git("rev-parse", "HEAD")
    result["dirty"] = bool(git("status", "--porcelain"))
    result["binary"] = str(binary)
    print(f"{binary} at {result['commit'][:12]}{' (dirty tree)' if result['dirty'] else ''}")

    with steps.step("check"):
        result["check"] = run([binary, "check"], rung_dir, dict(os.environ))
    if result["check"]["returncode"] != 0:
        failures.append(f"tessera check refused {args.rung}: {result['check']['stderr_tail'][-300:]}")

    bundle = work / "bundle"
    with steps.step("build"):
        if bundle.exists():
            shutil.rmtree(bundle)
        result["build"] = build_bundle(
            binary, rung_dir, bundle, work / "stage-timings.json", env=dict(os.environ)
        )
    if result["build"]["returncode"] != 0:
        failures.append(
            f"the build of {args.rung} failed, so nothing was served: "
            f"{result['build']['stderr_tail'][-300:]}"
        )
    else:
        with steps.step("verify"):
            result["verify"] = run([binary, "verify", "--deep", bundle], rung_dir, dict(os.environ))
        result["verify"]["wall_s"] = steps["verify"]
        if result["verify"]["returncode"] != 0:
            failures.append(f"tessera verify --deep refused the bundle: {result['verify']['stderr_tail'][-300:]}")

        with steps.step("serve"):
            try:
                serve_battery.main(battery_argv(args, rung_dir, bundle, work, binary))
            except Exception as e:  # noqa: BLE001 — a battery that could not run is a failure, not a stop
                failures.append(f"the serve battery raised {type(e).__name__}: {e}")
        serve_out = work / "serve.json"
        result["serve"] = json.loads(serve_out.read_text()) if serve_out.exists() else None

        with steps.step("ingest"):
            try:
                ingest_cycle_main(cycle_argv(args, rung_dir, bundle, work, binary))
            except Exception as e:  # noqa: BLE001 — as for the battery
                failures.append(f"the ingest cycle raised {type(e).__name__}: {e}")
        cycle_out = work / "cycle.json"
        result["ingest"] = json.loads(cycle_out.read_text()) if cycle_out.exists() else None
        failures += list(dig(result, "ingest", "failures", default=[]))

        # `viewport_latency` opens a bundle with the engine directly, on the view `s0` and the
        # identity Morton extent its own fixture is built with. A rung's views are named and framed
        # otherwise, so this says so and carries on; it never gates.
        if not args.quick:
            with steps.step("viewport_latency"):
                result["viewport_latency"] = run(
                    ["cargo", "run", "--release", "-p", "tessera-bench", "--bin",
                     "viewport_latency", "--", "--bundle", bundle],
                    CHECKOUT,
                    dict(os.environ, CARGO_PROFILE_RELEASE_DEBUG="0"),
                )
            if result["viewport_latency"]["returncode"] != 0:
                print(
                    "viewport_latency could not measure this bundle and is not a gate: "
                    f"{result['viewport_latency']['stderr_tail'].strip().splitlines()[-1:]}"
                )

    result["failures"] = failures
    result["total_wall_s"] = steps.total()
    out = results_dir / f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{result['commit'][:12]}.json"
    out.write_text(json.dumps(result, indent=2, default=str))
    print(f"\nwrote {out} in {result['total_wall_s']} s")
    return report(result, results_dir, out)


if __name__ == "__main__":
    sys.exit(main())
