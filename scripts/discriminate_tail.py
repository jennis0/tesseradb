#!/usr/bin/env python3
"""Task 2: discriminate the constant ~42-47 ms viewport tail: cold page faults, or not?

Four arms over the same workload, same bundle, same principal, same viewports:

  A  cold        server just booted, no pre-fault, external-ID index loaded
  B  prefaulted  as A, but every hot mapping read end-to-end before measuring
  C  no-index    server built with `--features skip-id-index` (18.9 GB of
                 external-ID extents never mapped by the engine's own loader),
                 no pre-fault
  D  no-index+prefaulted   C plus the pre-fault pass

The hypothesis predicts: the constant tail is large in A, small in B, small in C,
smallest in D -- i.e. it tracks residency, not work. If the tail survives in D,
the hypothesis is REFUTED and the residency story is not what is costing the tail.

Major faults are the direct evidence: read field 12 (majflt) of /proc/<pid>/stat
before and after each arm's serving phase. A tail caused by page faults must show
a majflt delta that falls with the arms; a tail that persists at ~zero major
faults is something else (allocator, GC of the frozen cache, tokio scheduling,
NUMA, or the swap the box entered at high k).

Caveat this script cannot remove on its own: `tessera-store::read::open_bundle`'s
`verify_files` reads every manifest-listed file's bytes in full at boot -- via
buffered `File::read`, not mmap -- for digest verification, *unconditionally*,
regardless of `skip-id-index`. That read already exercises the OS page cache for
every byte of the bundle, including the external-ID extents, before the engine
ever reaches (or, under the feature, skips) `ExternalIdIndex::load`'s own mmap
pass. `skip-id-index` therefore only guarantees this *process* never maps or
page-touches the extents through its own address space (so its own RSS/mapped
footprint drops), not that the kernel never reads those bytes at all during this
boot. The `/proc/<pid>/stat` `majflt` counter is unaffected by this: a major
fault is counted only when the kernel actually has to fetch a page from backing
storage for *this* access, wherever the previous state of the page cache came
from, so it remains the correct direct-evidence metric regardless.

Reuses `scripts/bench_k_sweep.py`'s viewport generator (`gen_viewports`),
percentile helper, and dictionary reader by import (not forked); reuses
`reference/oracle/harness.py` for `Server`/port allocation/`stop_server`. Each
arm gets its own `spawn_server`-style boot (a fresh process, a fresh `tessera.toml`
config raising `[serve] max_k` above the largest k swept -- reused from
`bench_k_sweep.write_config_with_max_k`).

Usage:
  reference/.venv/bin/python scripts/discriminate_tail.py \
      --bundle /tmp/tessera-1e9 --out probes/tail-discrimination.json
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import requests  # noqa: E402

import bench_k_sweep as bks  # noqa: E402 -- reused, not forked (gen_viewports, percentile, ...)
from oracle import harness  # noqa: E402

EXTENT = 65536.0
K_VALUES = [50, 500, 1000]  # brief's Step 2 requirement #5: not 30, not 2500/5000.
SUMMARY_K = 1000  # top-level p50/p99/max report this k -- the largest swept, where the
# hypothesised tail is most pronounced; full per-k detail is nested under "k" regardless.


# ---------------------------------------------------------------------------
# Process/kernel measurement helpers
# ---------------------------------------------------------------------------


def read_proc_stat_faults(pid: int) -> tuple[int, int]:
    """`(minflt, majflt)` for `pid` from `/proc/<pid>/stat` (man proc(5)): field 10 is minflt,
    field 12 is majflt (1-indexed). `comm` (field 2) is parenthesised and may itself contain
    spaces/parens, so the split point is the *last* `)` in the line, not a naive whitespace split."""
    text = Path(f"/proc/{pid}/stat").read_text()
    rest = text[text.rindex(")") + 2 :]
    fields = rest.split()
    # `fields[0]` is field 3 (state) after removing pid+comm, so field N -> index (N - 3).
    minflt = int(fields[10 - 3])
    majflt = int(fields[12 - 3])
    return minflt, majflt


def read_vmhwm_kb(pid: int) -> int | None:
    """Peak resident set size (`VmHWM`, kB) from `/proc/<pid>/status`, or `None` if the process
    has already exited or the kernel doesn't expose it (non-Linux; not expected here)."""
    try:
        text = Path(f"/proc/{pid}/status").read_text()
    except FileNotFoundError:
        return None
    for line in text.splitlines():
        if line.startswith("VmHWM:"):
            return int(line.split()[1])
    return None


def read_meminfo() -> dict[str, int]:
    """Every `/proc/meminfo` line as `{key: value_kb}` -- used here only for `SwapTotal`/
    `SwapFree`, per the brief's Step 2 requirement #6."""
    out: dict[str, int] = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, _, rest = line.partition(":")
        rest = rest.strip()
        if not rest:
            continue
        out[key] = int(rest.split()[0])
    return out


def try_drop_caches() -> bool:
    """Attempt `sync; echo 3 > /proc/sys/vm/drop_caches`. Returns whether it succeeded. WSL2
    commonly refuses this non-interactively (no passwordless sudo) -- the brief requires that
    failure be surfaced, not silently swallowed: a caller must record `cache_dropped: false` and
    say plainly that the 'cold' arms are only warm-ish."""
    try:
        subprocess.run(["sync"], check=True, timeout=30)
    except (subprocess.CalledProcessError, OSError, subprocess.TimeoutExpired) as e:
        print(f"WARNING: `sync` failed ({e}); page cache will not be dropped.", file=sys.stderr)
        return False
    try:
        result = subprocess.run(
            "echo 3 | sudo -n tee /proc/sys/vm/drop_caches",
            shell=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        print(f"WARNING: drop_caches attempt raised {e}.", file=sys.stderr)
        return False
    if result.returncode != 0:
        print(
            "WARNING: could not drop the page cache (no passwordless sudo for "
            "/proc/sys/vm/drop_caches under this environment). The 'cold' arms (A, C) are "
            "only warm-ish -- whatever the OS page cache already holds from prior activity "
            "(including this run's own bundle-open verify_files pass on an earlier arm) "
            "stays resident. This is WEAKER EVIDENCE than a true drop and is reported as such.",
            file=sys.stderr,
        )
        print(f"  stderr: {result.stderr.strip()}", file=sys.stderr)
        return False
    return True


def prefault(bundle_root: Path) -> tuple[float, list[str]]:
    """Read every byte of `columns.arrow`, `morton.u32` and `permutation.bin`, once
    (`cat <file> > /dev/null` per file is sufficient per the brief -- a plain sequential read
    achieves the same page-touching effect without a subprocess per file). Returns
    `(wall_seconds, paths_touched)`; NOT counted against the viewport budget."""
    targets = sorted(bundle_root.glob("*/partitions/*/slices/*/segments/*/columns.arrow"))
    targets += sorted(bundle_root.glob("*/partitions/*/slices/*/segments/*/morton.u32"))
    targets += sorted(bundle_root.glob("*/partitions/*/slices/*/permutation.bin"))
    if not targets:
        raise RuntimeError(f"prefault found no columns.arrow/morton.u32/permutation.bin under {bundle_root}")
    t0 = time.perf_counter()
    chunk_size = 64 * 1024 * 1024
    for path in targets:
        with open(path, "rb") as fh:
            while fh.read(chunk_size):
                pass
    elapsed = time.perf_counter() - t0
    return elapsed, [str(p) for p in targets]


# ---------------------------------------------------------------------------
# Server lifecycle: two binaries (feature on/off), each spawned fresh per arm
# ---------------------------------------------------------------------------


def build_and_copy(dest_name: str, feature: bool) -> Path:
    """`cargo build --release` (optionally `--features tessera-engine/skip-id-index`), then copy
    the produced `target/release/tessera` to `target/release/<dest_name>` -- so both variants
    exist simultaneously under distinct paths regardless of which was built last (the brief's
    Step 3 shell block builds both before invoking this script; this function is this script's
    own guarantee that the right bytes end up at the right path even if invoked standalone)."""
    cmd = ["cargo", "build", "--release"]
    if feature:
        cmd += ["--features", "tessera-engine/skip-id-index"]
    print(f"Building: {' '.join(cmd)}")
    subprocess.run(cmd, cwd=REPO_ROOT, check=True)
    src = REPO_ROOT / "target" / "release" / "tessera"
    dest = REPO_ROOT / "target" / "release" / dest_name
    shutil.copy2(src, dest)

    # Never leave a measurement binary at the default path. `cargo build --features
    # tessera-engine/skip-id-index` writes to `target/release/tessera`, and on
    # 2026-07-30 that binary was left there: the reference suite's `ensure_cli_built`
    # short-circuited on existence, silently reused it, and every `/control/changes`
    # request panicked the server with the external-ID index disabled. The failures
    # read as a code regression and survived a `git stash`, because stashing sources
    # does not rebuild a binary.
    #
    # `ensure_cli_built` no longer short-circuits, so that specific trap is closed at
    # the other end too -- but a feature-enabled binary sitting at the path every other
    # tool reaches for is a hazard regardless of who is careful. Remove it, and let the
    # next caller's own build put default-feature bytes back.
    if feature:
        src.unlink(missing_ok=True)
        print(f"  removed {src} (measurement build must not persist at the default path)")

    return dest


def spawn(cli_bin: Path, bundle_root: Path, tmp_dir: Path, boot_deadline_s: float, max_k: int):
    """Boot `<cli_bin> serve` against `bundle_root`. Mirrors `bench_k_sweep.py`'s
    `spawn_with_long_boot_deadline`, parameterised on the binary path so arms A/B (normal build)
    and C/D (`skip-id-index` build) can each boot their own variant."""
    cache_dir = tmp_dir / "cache"
    wal_path = tmp_dir / "wal.log"
    log_path = tmp_dir / "server.log"

    viewer_port = harness.free_port()
    session_port = harness.free_port()
    control_port = harness.free_port()

    config_path = bks.write_config_with_max_k(
        tmp_dir, bundle_root, cache_dir, wal_path, viewer_port, session_port, control_port, max_k
    )

    env = os.environ.copy()
    env["TESSERA_REFERENCE_SESSION_CRED"] = harness.SESSION_CREDENTIAL
    env["TESSERA_REFERENCE_OPERATOR_CRED"] = harness.OPERATOR_CREDENTIAL

    log_file = open(log_path, "ab")
    boot_start = time.monotonic()
    proc = subprocess.Popen(
        [str(cli_bin), "serve", "-c", str(config_path)],
        cwd=REPO_ROOT,
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
    )
    log_file.close()

    srv = harness.Server(viewer_port, session_port, control_port)

    deadline = time.monotonic() + boot_deadline_s
    up = False
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            extra = log_path.read_text(errors="replace")
            raise RuntimeError(f"{cli_bin} serve exited early ({proc.returncode}):\n{extra}")
        try:
            resp = requests.get(f"{srv.viewer_base}/healthz", timeout=2)
            if resp.status_code == 200:
                up = True
                break
        except requests.exceptions.ConnectionError:
            pass
        time.sleep(0.5)
    boot_elapsed = time.monotonic() - boot_start

    if not up:
        proc.terminate()
        raise RuntimeError(f"{cli_bin} serve did not become healthy within {boot_deadline_s}s")

    return srv, proc, boot_elapsed


# ---------------------------------------------------------------------------
# One arm
# ---------------------------------------------------------------------------


def run_arm(
    name: str,
    cli_bin: Path,
    bundle_root: Path,
    tmp_root: Path,
    grant: list[str],
    viewports: list[tuple[int, list[float]]],
    do_prefault: bool,
    boot_deadline: float,
    max_k_config: int,
) -> dict:
    print(f"\n=== Arm {name}: cli={cli_bin.name} prefault={do_prefault} ===")

    arm_tmp = tmp_root / f"arm-{name}"
    if arm_tmp.exists():
        shutil.rmtree(arm_tmp)
    arm_tmp.mkdir(parents=True)

    meminfo_before = read_meminfo()
    srv, proc, boot_elapsed = spawn(cli_bin, bundle_root, arm_tmp, boot_deadline, max_k_config)
    print(f"  boot: {boot_elapsed:.1f}s")

    try:
        minflt0, majflt0 = read_proc_stat_faults(proc.pid)

        prefault_elapsed = None
        prefault_files: list[str] = []
        if do_prefault:
            prefault_elapsed, prefault_files = prefault(bundle_root)
            print(f"  prefault: {prefault_elapsed:.1f}s ({len(prefault_files)} files)")

        auth = srv.authorise(grant)
        token = auth["token"]

        # Warm-up pass (row-projection cache fill), excluded from every measured k -- same recipe
        # as bench_p99.py/bench_k_sweep.py.
        warm_resp = srv.viewport_response(token, "s0", 6, [0.0, 0.0, EXTENT, EXTENT], k=30)
        warmup_server_us = int(warm_resp.headers.get("x-tessera-server-us", "0"))
        print(f"  warm-up viewport: {warmup_server_us / 1000:.3f} ms server-side")

        per_k: dict[str, dict] = {}
        for k in K_VALUES:
            server_us: list[float] = []
            e2e_us: list[float] = []
            for i, (zoom, bbox) in enumerate(viewports):
                t0 = time.perf_counter()
                resp = srv.viewport_response(token, "s0", zoom, bbox, k=k)
                e2e = (time.perf_counter() - t0) * 1e6
                e2e_us.append(e2e)
                server_us.append(float(resp.headers.get("x-tessera-server-us", "nan")))
                if (i + 1) % 500 == 0:
                    print(f"    k={k}: {i + 1}/{len(viewports)} viewports issued...")
            per_k[str(k)] = {
                "p50": bks.percentile(server_us, 0.50),
                "p99": bks.percentile(server_us, 0.99),
                "max": max(server_us),
                "e2e_p50": bks.percentile(e2e_us, 0.50),
                "e2e_p99": bks.percentile(e2e_us, 0.99),
                "e2e_max": max(e2e_us),
                "n": len(server_us),
            }
            print(
                f"    k={k}: server p50={per_k[str(k)]['p50'] / 1000:.3f}ms "
                f"p99={per_k[str(k)]['p99'] / 1000:.3f}ms"
            )

        minflt1, majflt1 = read_proc_stat_faults(proc.pid)
        peak_rss_kb = read_vmhwm_kb(proc.pid)
        meminfo_after = read_meminfo()
    finally:
        harness.stop_server(proc)

    summary = per_k[str(SUMMARY_K)]
    swap_free_before = meminfo_before.get("SwapFree", 0)
    swap_free_after = meminfo_after.get("SwapFree", 0)

    return {
        "summary_k": SUMMARY_K,
        "p50": summary["p50"],
        "p99": summary["p99"],
        "max": summary["max"],
        "majflt_delta": majflt1 - majflt0,
        "minflt_delta": minflt1 - minflt0,
        "peak_rss": peak_rss_kb,
        "boot_seconds": boot_elapsed,
        "prefault_seconds": prefault_elapsed,
        "prefault_files": prefault_files,
        "warmup_server_us": warmup_server_us,
        "swap_total_kb_before": meminfo_before.get("SwapTotal"),
        "swap_free_kb_before": swap_free_before,
        "swap_total_kb_after": meminfo_after.get("SwapTotal"),
        "swap_free_kb_after": swap_free_after,
        "swap_used_delta_kb": swap_free_before - swap_free_after,
        "k": per_k,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-discriminate")
    ap.add_argument("-n", "--n-viewports", type=int, default=2000)
    ap.add_argument("--width", type=int, default=10_000, help="grant width w")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=1800.0)
    ap.add_argument("--max-k-config", type=int, default=2000)
    ap.add_argument("--out", default="probes/tail-discrimination.json")
    ap.add_argument(
        "--skip-build",
        action="store_true",
        help="reuse target/release/tessera-normal / tessera-skipidx from a prior run",
    )
    args = ap.parse_args()

    bundle_root = Path(args.bundle)
    if not (bundle_root / "CURRENT").exists():
        print(f"ERROR: no bundle at {bundle_root}", file=sys.stderr)
        return 1

    tmp_root = Path(args.tmp)
    tmp_root.mkdir(parents=True, exist_ok=True)

    if args.skip_build:
        normal_bin = REPO_ROOT / "target" / "release" / "tessera-normal"
        skipidx_bin = REPO_ROOT / "target" / "release" / "tessera-skipidx"
        if not (normal_bin.exists() and skipidx_bin.exists()):
            print("ERROR: --skip-build given but one or both binaries are missing", file=sys.stderr)
            return 1
    else:
        normal_bin = build_and_copy("tessera-normal", feature=False)
        skipidx_bin = build_and_copy("tessera-skipidx", feature=True)

    print(f"Bundle: {bundle_root}")
    frozen_bytes = sum(f.stat().st_size for f in bundle_root.rglob("*") if f.is_file())
    print(f"Bundle on-disk size: {frozen_bytes / 1e9:.2f} GB")

    descriptors = bks.read_dictionary_descriptors(bundle_root)
    print(f"Dictionary vocab: {len(descriptors)} terms")
    rng = random.Random(args.seed)
    w = min(args.width, len(descriptors))
    grant = rng.sample(descriptors, w)

    # Same viewports, same order, same seed across every arm (brief's Step 2 requirement #4).
    viewports = bks.gen_viewports(args.seed + 1, args.n_viewports)

    cache_dropped = try_drop_caches()

    results: dict = {
        "bundle": str(bundle_root),
        "frozen_bundle_bytes": frozen_bytes,
        "n_viewports": args.n_viewports,
        "k_values": K_VALUES,
        "grant_width": w,
        "seed": args.seed,
        "cache_dropped": cache_dropped,
        "arms": {},
    }

    arm_specs = [
        ("A", normal_bin, False),
        ("B", normal_bin, True),
        ("C", skipidx_bin, False),
        ("D", skipidx_bin, True),
    ]

    for name, cli_bin, do_prefault in arm_specs:
        # Best-effort cache drop before each "cold" arm (A, C); B/D pre-fault deliberately, so a
        # cache drop immediately before them would just be undone by the prefault pass itself --
        # only attempt it for A and C, and only if the very first attempt (above) worked at all
        # (retrying a failing sudo four times produces four identical warnings for no benefit).
        if not do_prefault and cache_dropped:
            cache_dropped_this_arm = try_drop_caches()
            if not cache_dropped_this_arm:
                cache_dropped = False
                results["cache_dropped"] = False

        arm_result = run_arm(
            name,
            cli_bin,
            bundle_root,
            tmp_root,
            grant,
            viewports,
            do_prefault,
            args.boot_deadline,
            args.max_k_config,
        )
        results["arms"][name] = arm_result

        Path(args.out).write_text(json.dumps(results, indent=2, default=str))
        print(f"(checkpoint written to {args.out} after arm {name})")

    print("\n" + json.dumps({a: results["arms"][a] for a in "ABCD"}, indent=2, default=str))

    # Discrimination check, printed for a human but NOT a verdict on its own -- Step 4's memo
    # writes the actual CONFIRMED/REFUTED call.
    print("\n=== majflt_delta by arm (direct evidence) ===")
    for a in "ABCD":
        r = results["arms"][a]
        print(
            f"  {a}: majflt_delta={r['majflt_delta']:>10}  minflt_delta={r['minflt_delta']:>10}  "
            f"p50={r['p50'] / 1000:.3f}ms  p99={r['p99'] / 1000:.3f}ms  gap={(r['p99'] - r['p50']) / 1000:.3f}ms"
        )

    Path(args.out).write_text(json.dumps(results, indent=2, default=str))
    print(f"\nResults written to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
