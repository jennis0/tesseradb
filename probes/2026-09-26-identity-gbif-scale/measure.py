"""Run one command under a memory cap and sample its memory once a second.

    python3 measure.py --cap 26G --out run.json -- mosaica build ...

The command runs in a transient systemd scope with `MemoryMax` and `MemorySwapMax=2G`. Each
sample reads the process's `VmRSS` and its scope's `memory.current`, so a figure includes the
page cache the scope charged as well as the process's own pages, and the free space on `--disk`
(which other processes on the machine also move). A sample is `[seconds, VmRSS KiB, memory.current
KiB, free MiB, RssAnon KiB]`. With `--stages` naming a `--stage-timings-json` file, each stage's
peak is the largest sample taken between its start and end.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path


def read_kib(pid: int, key: str) -> int | None:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(key + ":"):
                return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError):
        return None
    return None


def cgroup_of(pid: int) -> Path | None:
    try:
        rel = Path(f"/proc/{pid}/cgroup").read_text().strip().split("::", 1)[1]
    except (FileNotFoundError, IndexError):
        return None
    return Path("/sys/fs/cgroup") / rel.lstrip("/")


def read_int(path: Path) -> int | None:
    try:
        return int(path.read_text().split()[0])
    except (FileNotFoundError, ValueError, OSError):
        return None


def stamp(text: str) -> float:
    if isinstance(text, (int, float)):
        return float(text)
    return datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cap", default="26G")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--stages", type=Path, default=None)
    ap.add_argument("--log", type=Path, default=None)
    ap.add_argument("--disk", default="/home", help="the filesystem whose free space is sampled")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    cmd = args.cmd[1:] if args.cmd and args.cmd[0] == "--" else args.cmd

    scoped = ["systemd-run", "--user", "--scope", "--collect", "-p", f"MemoryMax={args.cap}",
              "-p", "MemorySwapMax=2G", "--", *cmd]
    free_before = shutil.disk_usage(args.disk).free >> 20
    began = time.time()
    samples = []
    group = None
    peak = None
    with open(args.log or os.devnull, "wb") as log:
        child = subprocess.Popen(scoped, stdout=log if args.log else None,
                                 stderr=subprocess.STDOUT if args.log else None)
        while child.poll() is None:
            if group is None or not group.name.endswith(".scope"):
                group = cgroup_of(child.pid)
            rss = read_kib(child.pid, "VmRSS")
            anon = read_kib(child.pid, "RssAnon")
            current = read_int(group / "memory.current") if group else None
            peak = read_int(group / "memory.peak") if group else peak
            free = shutil.disk_usage(args.disk).free
            samples.append([round(time.time() - began, 2), rss,
                            current // 1024 if current else None, free >> 20, anon])
            time.sleep(1.0)
    wall = time.time() - began
    code = child.returncode

    result = {
        "cmd": cmd,
        "cap": args.cap,
        "exit": code,
        "wall_s": round(wall, 1),
        "peak_rss_kib": max((s[1] or 0) for s in samples) if samples else None,
        "peak_scope_kib": max((s[2] or 0) for s in samples) if samples else None,
        "peak_anon_kib": max((s[4] or 0) for s in samples) if samples else None,
        "scope_memory_peak_kib": peak // 1024 if peak else None,
        "free_mib_before": free_before,
        "free_mib_after": shutil.disk_usage(args.disk).free >> 20,
        "free_mib_least": min((s[3] for s in samples), default=None),
        "samples": samples,
    }
    if args.stages and args.stages.exists():
        stages = json.loads(args.stages.read_text())
        rows = stages if isinstance(stages, list) else stages.get("stages", [])
        for stage in rows:
            lo, hi = stamp(stage["started_at"]) - began, stamp(stage["ended_at"]) - began
            inside = [s for s in samples if lo - 1 <= s[0] <= hi + 1]
            stage["sampled_peak_rss_kib"] = max((s[1] or 0) for s in inside) if inside else None
            stage["sampled_peak_scope_kib"] = max((s[2] or 0) for s in inside) if inside else None
            stage["sampled_peak_anon_kib"] = max((s[4] or 0) for s in inside) if inside else None
        result["stages"] = rows
    args.out.write_text(json.dumps(result, indent=1) + "\n")
    print(f"exit {code}, wall {wall:.1f}s, peak rss {result['peak_rss_kib']} KiB, "
          f"peak scope {result['peak_scope_kib']} KiB", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
