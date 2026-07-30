#!/usr/bin/env python3
"""Sweep viewport latency over k = {50, 500, 1000, 2500, 5000} against a pre-built 10^9 bundle.

Sibling to `bench_p99.py` (Task 16's exit-criteria script): same boot recipe, same w=10^4
authorise recipe, same seeded viewport geometry (mixed zooms 4..12, ~300-tile spans) -- but
boots the server ONCE and reuses it across every k value, since boot at 10^9 costs ~177s
(`verify_files` digesting all 51 GB) and paying that five times is wasted. One warm-up viewport
(row-projection cache fill) is fired before the first measured k and excluded from every sample.

`[serve] max_k` defaults to 200 (Reference Sheet R1) and `Engine::viewport` clamps k to it
(crates/tessera-engine/src/viewport.rs) -- this script's generated config raises `max_k` to
comfortably above the largest k swept, and the per-k report includes mean/max points actually
returned so a plateau (== clamp still firing) is visible directly in the output, not inferred.

Usage: reference/.venv/bin/python scripts/bench_k_sweep.py [--bundle /tmp/tessera-1e9] [-n 500]
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
from oracle.wire import decode_viewport  # noqa: E402

EXTENT = 65536.0
K_VALUES = [50, 500, 1000, 2500, 5000]


def read_dictionary_descriptors(bundle_root: Path) -> list[str]:
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


def write_config_with_max_k(
    tmp_dir: Path,
    bundle_root: Path,
    cache_dir: Path,
    wal_path: Path,
    viewer_port: int,
    session_port: int,
    control_port: int,
    max_k: int,
    *,
    compute_threads: int | None = None,
    compute_admission: int | None = None,
    compute_queue: int | None = None,
    admission_timeout_ms: int | None = None,
) -> Path:
    """Like `harness.write_config`, but with `[serve] max_k` raised above the default 200 --
    otherwise `Engine::viewport` silently clamps every k > 200 and the sweep is measuring the
    same 200-per-tile budget five times over.

    Task 9: the four `compute_*` knobs (D-B/D-E's admission gate) are optional overrides, `None`
    by default -- omitted from the written config, so every existing caller keeps getting the
    server's own defaults (`compute_threads` = available parallelism, `compute_admission` =
    `compute_threads`, `compute_queue` = 2x that, `admission_timeout_ms` = 250) exactly as before
    this task. `scripts/bench_concurrency.py` is the only caller that passes them explicitly, to
    force a low admission bound for its shed cell (criterion 3) or a shorter timeout for its
    cold-build cell (criterion 6).
    """
    config_text = f"""
[bundle]
path = "{bundle_root}"
cache = "{cache_dir}"
wal = "{wal_path}"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:{viewer_port}"
session = "127.0.0.1:{session_port}"
control = "127.0.0.1:{control_port}"
max_k = {max_k}
session_credential_env = "TESSERA_REFERENCE_SESSION_CRED"
operator_credential_env = "TESSERA_REFERENCE_OPERATOR_CRED"
"""
    overrides = {
        "compute_threads": compute_threads,
        "compute_admission": compute_admission,
        "compute_queue": compute_queue,
        "admission_timeout_ms": admission_timeout_ms,
    }
    for key, value in overrides.items():
        if value is not None:
            config_text += f"{key} = {value}\n"

    config_path = tmp_dir / "tessera.toml"
    config_path.write_text(config_text)
    return config_path


def spawn_with_long_boot_deadline(
    bundle_root: Path,
    tmp_dir: Path,
    boot_deadline_s: float,
    max_k: int,
    *,
    compute_threads: int | None = None,
    compute_admission: int | None = None,
    compute_queue: int | None = None,
    admission_timeout_ms: int | None = None,
):
    harness.ensure_cli_built()

    cache_dir = tmp_dir / "cache"
    wal_path = tmp_dir / "wal.log"
    log_path = tmp_dir / "server.log"

    viewer_port = harness.free_port()
    session_port = harness.free_port()
    control_port = harness.free_port()

    config_path = write_config_with_max_k(
        tmp_dir,
        bundle_root,
        cache_dir,
        wal_path,
        viewer_port,
        session_port,
        control_port,
        max_k,
        compute_threads=compute_threads,
        compute_admission=compute_admission,
        compute_queue=compute_queue,
        admission_timeout_ms=admission_timeout_ms,
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


def gen_viewports(seed: int, n: int):
    """Same recipe as `bench_p99.py`'s sweep loop: mixed zooms 4..12, span sized so a bbox
    touches roughly ~300 tiles. Generated once from a fixed seed and reused verbatim across every
    k value so the k comparison is over identical geometry."""
    rng = random.Random(seed)
    out = []
    for _ in range(n):
        zoom = rng.randint(4, 12)
        span = EXTENT / (2 ** max(zoom - 4, 1))
        x0 = rng.uniform(0.0, EXTENT - span)
        y0 = rng.uniform(0.0, EXTENT - span)
        bbox = [x0, y0, x0 + span, y0 + span]
        out.append((zoom, bbox))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-serve-ksweep")
    ap.add_argument("-n", "--n-viewports", type=int, default=500)
    ap.add_argument("--width", type=int, default=10_000, help="grant width w")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=1800.0)
    ap.add_argument("--max-k-config", type=int, default=6000, help="[serve] max_k in generated config")
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

    print("Booting server ONCE (this can take minutes at 10^9 -- verify_files reads every byte)...")
    srv, proc, boot_elapsed = spawn_with_long_boot_deadline(
        bundle_root, tmp_dir, args.boot_deadline, args.max_k_config
    )
    print(f"Boot time (process start -> /healthz 200): {boot_elapsed:.1f}s")

    results = {
        "bundle": str(bundle_root),
        "frozen_bundle_bytes": frozen_bytes,
        "boot_seconds": boot_elapsed,
        "max_k_config": args.max_k_config,
        "n_viewports": args.n_viewports,
        "seed": args.seed,
        "k_sweep": {},
    }

    try:
        descriptors = read_dictionary_descriptors(bundle_root)
        print(f"Dictionary vocab: {len(descriptors)} terms")
        results["dictionary_vocab"] = len(descriptors)
        rng = random.Random(args.seed)
        w = min(args.width, len(descriptors))
        grant = rng.sample(descriptors, w)

        t0 = time.perf_counter()
        auth = srv.authorise(grant)
        token = auth["token"]
        authorise_s = time.perf_counter() - t0
        results["grant_width"] = w
        results["authorise_seconds"] = authorise_s
        print(f"authorise() [fragment build, one-off, w={w}]: {authorise_s * 1000:.1f} ms")

        # Warm-up: one viewport at k=30 (row-projection cache fill; excluded from every measured
        # k below -- this is a per-token one-off, ~19s at 10^9, and must NOT land in a sample).
        t0 = time.perf_counter()
        warm_resp = srv.viewport_response(token, "s0", 6, [0.0, 0.0, EXTENT, EXTENT], k=30)
        warmup_s = time.perf_counter() - t0
        warmup_server_us = int(warm_resp.headers.get("x-tessera-server-us", "0"))
        results["warmup_end_to_end_ms"] = warmup_s * 1000
        results["warmup_server_us"] = warmup_server_us
        print(
            f"Warm-up viewport [row-projection cache fill, one-off]: "
            f"{warmup_s * 1000:.1f} ms end-to-end, {warmup_server_us / 1000:.3f} ms server-side"
        )

        # Same seed -> same viewport sequence for every k, so the k comparison is apples-to-apples.
        viewports = gen_viewports(args.seed + 1, args.n_viewports)

        for k in K_VALUES:
            print(f"\n=== k={k} ===")
            server_us: list[float] = []
            e2e_us: list[float] = []
            resp_bytes: list[int] = []
            points_returned: list[int] = []

            for i, (zoom, bbox) in enumerate(viewports):
                t0 = time.perf_counter()
                resp = srv.viewport_response(token, "s0", zoom, bbox, k=k)
                e2e = (time.perf_counter() - t0) * 1e6
                e2e_us.append(e2e)
                server_us.append(float(resp.headers.get("x-tessera-server-us", "nan")))
                body = resp.content
                resp_bytes.append(len(body))
                # Only decode a subsample of responses for point counts -- pyarrow IPC decode of
                # a multi-MB payload 500-2000 times per k is itself expensive and not part of the
                # thing being measured. 50 samples is plenty for mean/max at this scale.
                if i < 50:
                    _, points = decode_viewport(body)
                    points_returned.append(len(points))

                if (i + 1) % 200 == 0:
                    print(f"  {i + 1}/{len(viewports)} viewports issued...")

            free_out = subprocess.run(["free", "-g"], capture_output=True, text=True).stdout
            print(free_out)

            k_result = {
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
                "response_bytes": {
                    "mean": sum(resp_bytes) / len(resp_bytes),
                    "max": max(resp_bytes),
                },
                "points_returned_sample_n": len(points_returned),
                "points_returned": {
                    "mean": sum(points_returned) / len(points_returned) if points_returned else None,
                    "max": max(points_returned) if points_returned else None,
                },
                "free_g_after": free_out,
            }
            results["k_sweep"][str(k)] = k_result
            print(json.dumps(k_result, indent=2, default=str))

        print("\n" + json.dumps(results, indent=2, default=str))

        if args.out:
            Path(args.out).write_text(json.dumps(results, indent=2, default=str))

        return 0
    finally:
        harness.stop_server(proc)


if __name__ == "__main__":
    raise SystemExit(main())
