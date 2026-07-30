#!/usr/bin/env python3
"""Ask 4: true wire calls with 5/10/100/1000 simultaneous users, two arms.

Boots a real `tessera serve`, authorises the sessions, and drives `tessera-bench load` against
it while sampling the server's RSS and CPU. The hot loop is Rust because a Python client is the
bottleneck at 1000 concurrent connections pulling multi-megabyte Arrow bodies; everything else is
here, reusing `reference/oracle/harness.py`'s proven config/boot/teardown machinery and
`bench_k_sweep.py`'s long-boot spawner rather than reimplementing them.

Two arms, differing only in the token file handed to the generator:

  Arm A -- N DISTINCT grant sets, so N distinct fragments and N distinct row projections. This is
  the memory question. Design §13.1 names the materialised mask as the first thing to break: at
  10^9 a dense mask is 125 MB and "a thousand live auth inputs is 125 GB". At the scales this
  suite runs (<=25M) a dense mask is ~3 MB, so 1000 sessions is ~3 GB and the wall is NOT reached
  -- what this arm yields is the per-session marginal cost and the RSS-vs-N slope, which
  extrapolate. Reported as a slope; do not read it as having found the ceiling.

  Arm B -- N sessions over a handful of grant sets, so fragments and row projections are shared.
  Isolates request-path throughput and lock contention from per-session memory. The suspected
  contention points, all on the hot path:
    * `Engine::row_projection_cache: Mutex<FxHashMap>`, locked on every viewport
    * `state.engine.meta()` per request -- a second `load_full` plus two `Vec` clones
    * `build_scalar_columns` -- a transpose with per-point `String` clones

The generator's own ceiling is measured first, against `/healthz`, and any viewport cell within
3x of it is flagged `generator_bound`: proving the client is not the bottleneck rather than
assuming it.
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "reference"))

import requests  # noqa: E402

from oracle import harness  # noqa: E402
from bench_k_sweep import read_dictionary_descriptors, spawn_with_long_boot_deadline  # noqa: E402

BENCH_BIN = REPO_ROOT / "target" / "release" / "tessera-bench"


def sample_process(pid: int, stop: threading.Event, out: list) -> None:
    """Poll a process's RSS and CPU until told to stop.

    Server-side, deliberately: the generator and the server share a 12-core box, so reporting only
    one of them would hide contention that is real and that a reader needs to see.
    """
    stat_path = Path(f"/proc/{pid}/stat")
    status_path = Path(f"/proc/{pid}/status")
    ticks = 100.0
    prev_cpu = None
    prev_t = None
    while not stop.is_set():
        try:
            fields = stat_path.read_text().rsplit(") ", 1)[1].split()
            cpu = (int(fields[11]) + int(fields[12])) / ticks
            rss_kib = 0
            for line in status_path.read_text().splitlines():
                if line.startswith("VmRSS:"):
                    rss_kib = int(line.split()[1])
                    break
            now = time.monotonic()
            pct = None
            if prev_cpu is not None:
                pct = 100.0 * (cpu - prev_cpu) / max(now - prev_t, 1e-6)
            out.append({"t": now, "rss_kib": rss_kib, "cpu_pct": pct})
            prev_cpu, prev_t = cpu, now
        except (FileNotFoundError, IndexError, ValueError):
            pass
        stop.wait(0.25)


def authorise(srv, terms: list[str]) -> str:
    import base64

    auth = json.dumps({"terms": terms}).encode()
    resp = requests.post(
        f"{srv.session_base}/session/authorise",
        headers={"Authorization": f"Bearer {harness.SESSION_CREDENTIAL}"},
        json={"auth_data": base64.b64encode(auth).decode()},
        timeout=120,
    )
    resp.raise_for_status()
    return resp.json()["token"]


def build_tokens(srv, descriptors: list[str], n: int, distinct: bool, w: int, seed: int) -> list[str]:
    """One token per virtual user.

    Arm A gives every user its own grant set, so no two share a fragment. Arm B draws from a small
    pool, so they do. The distinction is the whole experiment, so it is made here, explicitly,
    rather than left to the generator to infer.
    """
    rng = random.Random(seed)
    if distinct:
        tokens = []
        for i in range(n):
            local = random.Random(seed * 1_000_003 + i)
            terms = local.sample(descriptors, min(w, len(descriptors)))
            tokens.append(authorise(srv, terms))
        return tokens
    pool_size = min(4, n)
    pool = [
        authorise(srv, rng.sample(descriptors, min(w, len(descriptors))))
        for _ in range(pool_size)
    ]
    return [pool[i % pool_size] for i in range(n)]


def run_load(args, tokens: list[str], concurrency: int, run_dir: Path, healthz: bool,
             srv, scale: int, label_set: str) -> dict | None:
    tokens_file = run_dir / f"tokens-{concurrency}-{'h' if healthz else 'v'}.txt"
    tokens_file.write_text("\n".join(tokens[:concurrency]) + "\n")

    cmd = [
        str(BENCH_BIN), "load",
        "--viewer-url", srv.viewer_base,
        "--tokens", str(tokens_file),
        "--concurrency", str(concurrency),
        "--duration-s", str(args.duration),
        "--threads", str(args.threads),
        "--bundle-scale", str(scale),
        "--bundle-label-set", label_set,
        "--k", str(args.k),
        "--zoom", str(args.zoom),
        "--run-dir", str(run_dir),
    ]
    if healthz:
        cmd.append("--healthz")

    proc = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"  load failed: {proc.stderr.strip()[:400]}")
        return None

    records = [json.loads(l) for l in (run_dir / "load.jsonl").read_text().splitlines() if l.strip()]
    return records[-1] if records else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", type=Path, required=True)
    ap.add_argument("--scale", type=int, required=True)
    ap.add_argument("--label-set", default="categories-subclass")
    ap.add_argument("--concurrency", default="5,10,100,1000")
    ap.add_argument("--arms", default="B,A", help="B=shared fragments, A=distinct fragments")
    ap.add_argument("--duration", type=float, default=10.0)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--w", type=int, default=100, help="grant width per principal")
    ap.add_argument("--k", type=int, default=30)
    ap.add_argument("--zoom", type=int, default=8)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--rss-abort-gib", type=float, default=30.0)
    ap.add_argument("--run-dir", type=Path, default=Path("/tmp/tessera-bench/runs/concurrency"))
    args = ap.parse_args()

    if not BENCH_BIN.exists():
        print(f"missing {BENCH_BIN} -- cargo build --release -p tessera-bench", file=sys.stderr)
        return 1

    levels = [int(c) for c in args.concurrency.split(",")]
    args.run_dir.mkdir(parents=True, exist_ok=True)
    tmp_dir = args.run_dir / "server"
    tmp_dir.mkdir(exist_ok=True)

    descriptors = read_dictionary_descriptors(args.bundle)
    print(f"bundle {args.bundle} -- {len(descriptors)} descriptors")

    print("booting server...")
    srv, proc, boot_s = spawn_with_long_boot_deadline(args.bundle, tmp_dir, 1800.0, max(args.k, 200))
    print(f"  up in {boot_s:.1f}s (pid {proc.pid})")

    summary = {"boot_s": boot_s, "scale": args.scale, "label_set": args.label_set, "cells": []}

    try:
        # The generator's own ceiling, first. Every viewport number below is read against this.
        print("\ncalibrating generator ceiling against /healthz")
        ceiling = {}
        warm = build_tokens(srv, descriptors, 1, False, args.w, args.seed)
        for c in levels:
            rec = run_load(args, warm * c, c, args.run_dir, True, srv, args.scale, args.label_set)
            if rec:
                rps = rec["params"]["throughput_rps"]
                ceiling[c] = rps
                print(f"  c={c:<5} healthz {rps:>10,.0f} rps  p99={rec['timing']['p99_ns']/1e6:>7.2f} ms")

        for arm in args.arms.split(","):
            arm = arm.strip().upper()
            distinct = arm == "A"
            label = "A (distinct principals)" if distinct else "B (shared fragments)"
            print(f"\nArm {label}")
            print(f"  {'conc':>6} {'rps':>10} {'p50_ms':>9} {'p99_ms':>9} {'srv_p99_ms':>11} "
                  f"{'rss_gib':>9} {'cpu%':>7}  flags")

            for c in levels:
                rss_before = 0
                try:
                    for line in Path(f"/proc/{proc.pid}/status").read_text().splitlines():
                        if line.startswith("VmRSS:"):
                            rss_before = int(line.split()[1])
                except FileNotFoundError:
                    pass

                t0 = time.monotonic()
                tokens = build_tokens(srv, descriptors, c, distinct, args.w, args.seed)
                auth_s = time.monotonic() - t0

                samples: list = []
                stop = threading.Event()
                sampler = threading.Thread(target=sample_process, args=(proc.pid, stop, samples))
                sampler.start()
                rec = run_load(args, tokens, c, args.run_dir, False, srv, args.scale, args.label_set)
                stop.set()
                sampler.join()

                if not rec:
                    continue

                rss_peak = max((s["rss_kib"] for s in samples), default=0)
                cpu = [s["cpu_pct"] for s in samples if s["cpu_pct"] is not None]
                cpu_mean = sum(cpu) / len(cpu) if cpu else 0.0
                rps = rec["params"]["throughput_rps"]

                flags = list(rec.get("flags", []))
                if c in ceiling and rps > ceiling[c] / 3.0:
                    # Within 3x of what the generator can do against a trivial endpoint: this cell
                    # is measuring the client as much as the server.
                    flags.append("generator_bound")

                print(f"  {c:>6} {rps:>10,.0f} {rec['timing']['median_ns']/1e6:>9.2f} "
                      f"{rec['timing']['p99_ns']/1e6:>9.2f} {rec['params']['server_us_p99']/1000:>11.2f} "
                      f"{rss_peak/1048576:>9.2f} {cpu_mean:>7.0f}  {','.join(flags)}")

                summary["cells"].append({
                    "arm": arm, "concurrency": c, "rps": rps,
                    "p50_ms": rec["timing"]["median_ns"] / 1e6,
                    "p99_ms": rec["timing"]["p99_ns"] / 1e6,
                    "server_us_p99": rec["params"]["server_us_p99"],
                    "rss_peak_kib": rss_peak, "rss_before_kib": rss_before,
                    "rss_delta_kib": rss_peak - rss_before,
                    "authorise_s": auth_s, "server_cpu_pct_mean": cpu_mean,
                    "distinct_tokens": rec["params"]["distinct_tokens"],
                    "generator_ceiling_rps": ceiling.get(c),
                    "flags": flags,
                })

                if rss_peak / 1048576 > args.rss_abort_gib:
                    print(f"  RSS {rss_peak/1048576:.1f} GiB exceeded --rss-abort-gib "
                          f"{args.rss_abort_gib}; stopping this arm. The curve up to here IS the "
                          f"result.")
                    break
    finally:
        harness.stop_server(proc)

    out = args.run_dir / "concurrency-summary.json"
    out.write_text(json.dumps(summary, indent=2))
    print(f"\nwrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
