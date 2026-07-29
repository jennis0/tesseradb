#!/usr/bin/env python3
"""Task 16, Step 3: the 10^9 exit-criteria measurement (plan §5).

Boots `tessera serve` against a pre-built bundle (default `/tmp/tessera-1e9`, built by
`scripts/build_full.sh`), authorises one realistic principal (w = 10,000 random grants over the
bundle's real dictionary — the recipe in `probes/mask_probe.py`'s "random w=" family), does one
warm-up pass (fragment build + row-projection cache fill — reported separately, NOT counted
against the viewport budget), then fires >= 2,000 random-pan viewports (~300 tiles each, mixed
zooms, k=30) over HTTP and reports p50/p99/max both server-side (`x-tessera-server-us` response
header, Task 16's added instrumentation) and end-to-end (wall-clock around the HTTP call).

Exit gate (plan §5): server-side p99 < 10 ms.

Usage: reference/.venv/bin/python scripts/bench_p99.py [--bundle /tmp/tessera-1e9] [-n 2000]
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))

import requests  # noqa: E402

from oracle import harness  # noqa: E402

EXTENT = 65536.0


def read_dictionary_descriptors(bundle_root: Path) -> list[str]:
    """Read every descriptor out of `<prefix>/dictionary/terms-0.dict` (contracts §2.1's record
    format: u32 LE length ‖ descriptor bytes, term_id = ordinal) — real term ids from the actual
    built bundle, not guessed ones (an unresolvable descriptor just drops out of `terms_of_auth`
    silently, which would understate a "realistic" principal's width)."""
    current = json.loads((bundle_root / "CURRENT").read_text())
    dict_path = bundle_root / current["prefix"] / "dictionary" / "terms-0.dict"
    data = dict_path.read_bytes()
    descriptors = []
    off = 0
    while off < len(data):
        length = int.from_bytes(data[off : off + 4], "little")
        off += 4
        descriptors.append(data[off : off + length].decode("utf-8"))
        off += length
    return descriptors


def spawn_with_long_boot_deadline(bundle_root: Path, tmp_dir: Path, boot_deadline_s: float):
    """Like `harness.spawn_server`, but with a boot-health deadline long enough for a 10^9-row
    bundle (Task 7's ledger note: `verify_files` reads every bundle byte at boot, ~60 GB here —
    this can be minutes, not the 20s `spawn_server` allows for the small fixtures)."""
    harness.ensure_cli_built()

    cache_dir = tmp_dir / "cache"
    wal_path = tmp_dir / "wal.log"
    log_path = tmp_dir / "server.log"

    viewer_port = harness.free_port()
    session_port = harness.free_port()
    control_port = harness.free_port()

    config_path = harness.write_config(
        tmp_dir, bundle_root, cache_dir, wal_path, viewer_port, session_port, control_port
    )

    import os

    env = os.environ.copy()
    env["TESSERA_REFERENCE_SESSION_CRED"] = harness.SESSION_CREDENTIAL
    env["TESSERA_REFERENCE_OPERATOR_CRED"] = harness.OPERATOR_CREDENTIAL

    log_file = open(log_path, "ab")
    boot_start = time.monotonic()
    proc = subprocess.Popen(
        [str(harness.CLI_BIN), "serve", "-c", str(config_path)],
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
            raise RuntimeError(f"tessera serve exited early ({proc.returncode}):\n{extra}")
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
        raise RuntimeError(f"tessera serve did not become healthy within {boot_deadline_s}s")

    return srv, proc, boot_elapsed


def percentile(values: list[float], p: float) -> float:
    s = sorted(values)
    idx = min(int(len(s) * p), len(s) - 1)
    return s[idx]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-serve")
    ap.add_argument("-n", "--n-viewports", type=int, default=2000)
    ap.add_argument("--width", type=int, default=10_000, help="grant width w")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=1800.0)
    ap.add_argument("--out", default=None, help="write JSON results here")
    args = ap.parse_args()

    bundle_root = Path(args.bundle)
    if not (bundle_root / "CURRENT").exists():
        print(f"ERROR: no bundle at {bundle_root} (run scripts/build_full.sh first)", file=sys.stderr)
        return 1

    tmp_dir = Path(args.tmp)
    tmp_dir.mkdir(parents=True, exist_ok=True)

    frozen_bytes = sum(f.stat().st_size for f in bundle_root.rglob("*") if f.is_file())
    print(f"Bundle on-disk size: {frozen_bytes / 1e9:.2f} GB ({bundle_root})")

    print("Booting server (this can take minutes at 10^9 -- verify_files reads every byte)...")
    srv, proc, boot_elapsed = spawn_with_long_boot_deadline(bundle_root, tmp_dir, args.boot_deadline)
    print(f"Boot time (process start -> /healthz 200): {boot_elapsed:.1f}s")

    try:
        descriptors = read_dictionary_descriptors(bundle_root)
        print(f"Dictionary vocab: {len(descriptors)} terms")
        rng = random.Random(args.seed)
        w = min(args.width, len(descriptors))
        grant = rng.sample(descriptors, w)

        t0 = time.perf_counter()
        auth = srv.authorise(grant)
        token = auth["token"]
        authorise_s = time.perf_counter() - t0
        print(f"authorise() [fragment build, one-off, w={w}]: {authorise_s * 1000:.1f} ms")

        # Warm-up pass: one viewport to force row-projection cache fill (Permutation::project is
        # seconds at 10^9 rows -- cached per (token, slice, pin), never on the per-viewport path).
        # bbox picked to touch a decent tile spread at a modest zoom.
        t0 = time.perf_counter()
        warm_resp = srv.viewport_response(token, "s0", 6, [0.0, 0.0, EXTENT, EXTENT], k=30)
        warmup_s = time.perf_counter() - t0
        warmup_server_us = int(warm_resp.headers.get("x-tessera-server-us", "0"))
        print(
            f"Warm-up viewport [row-projection cache fill, one-off]: "
            f"{warmup_s * 1000:.1f} ms end-to-end, {warmup_server_us / 1000:.3f} ms server-side"
        )

        rng2 = random.Random(args.seed + 1)
        server_us: list[float] = []
        e2e_us: list[float] = []
        n = args.n_viewports
        for i in range(n):
            # Mixed zooms 4..12; span sized so a tile-covering bbox touches roughly ~300 tiles at
            # these zooms (16x16 .. 4096x4096 grids) -- matches the brief's "~300 tiles" sweep.
            zoom = rng2.randint(4, 12)
            span = EXTENT / (2 ** max(zoom - 4, 1))
            x0 = rng2.uniform(0.0, EXTENT - span)
            y0 = rng2.uniform(0.0, EXTENT - span)
            bbox = [x0, y0, x0 + span, y0 + span]

            t0 = time.perf_counter()
            resp = srv.viewport_response(token, "s0", zoom, bbox, k=30)
            e2e = (time.perf_counter() - t0) * 1e6
            e2e_us.append(e2e)
            server_us.append(float(resp.headers.get("x-tessera-server-us", "nan")))

            if (i + 1) % 500 == 0:
                print(f"  {i + 1}/{n} viewports issued...")

        result = {
            "bundle": str(bundle_root),
            "frozen_bundle_bytes": frozen_bytes,
            "boot_seconds": boot_elapsed,
            "dictionary_vocab": len(descriptors),
            "grant_width": w,
            "authorise_seconds": authorise_s,
            "warmup_end_to_end_ms": warmup_s * 1000,
            "warmup_server_us": warmup_server_us,
            "n_viewports": n,
            "server_us": {
                "p50": percentile(server_us, 0.50),
                "p99": percentile(server_us, 0.99),
                "max": max(server_us),
            },
            "end_to_end_us": {
                "p50": percentile(e2e_us, 0.50),
                "p99": percentile(e2e_us, 0.99),
                "max": max(e2e_us),
            },
        }
        print(json.dumps(result, indent=2))

        p99_ms = result["server_us"]["p99"] / 1000
        gate_pass = p99_ms < 10.0
        print(f"\nEXIT GATE (server-side p99 < 10ms): {'PASS' if gate_pass else 'FAIL'} ({p99_ms:.3f} ms)")

        if args.out:
            Path(args.out).write_text(json.dumps(result, indent=2))

        return 0 if gate_pass else 2
    finally:
        harness.stop_server(proc)


if __name__ == "__main__":
    raise SystemExit(main())
