#!/usr/bin/env python3
"""Split a process's resident set into anonymous and file-backed halves, per build stage.

`tessera build --stage-timings` reports one number a stage — `VmHWM`, the kernel's
high-water mark over *all* resident pages. Since 2026-08-30 the entity-order columns,
the render tail and the text index's runs are mapped files, so most of what a large
build has resident is page cache the kernel may evict rather than memory it must have.
`VmHWM` cannot tell the two apart. This can.

Run mode:

    sample_rss.py --out PREFIX -- tessera build ...

polls `/proc/<pid>/status` every 100 ms for `RssAnon`, `RssFile`, `RssShmem`, `VmRSS`
and `VmHWM`, writing `PREFIX.rss.csv`; every line the child writes to stderr is
timestamped on the same clock into `PREFIX.stages.csv`, which is what aligns a sample
with the stage that produced it. Both streams are echoed so the run is watchable.

Report mode:

    sample_rss.py --report PREFIX

joins the two: for each stage, the anonymous high-water, the file-backed high-water and
the `VmHWM` the build itself printed, over the samples between that stage's end and the
previous one's. A stage shorter than the sample interval gets the nearest sample and is
marked; nothing is interpolated.
"""

from __future__ import annotations

import argparse
import csv
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

FIELDS = ("RssAnon", "RssFile", "RssShmem", "VmRSS", "VmHWM")
# Beside the resident split: what the walk costs the disk. `majflt` is the process's
# cumulative major-fault count (`/proc/<pid>/stat` field 12, the whole tree's), `read_bytes`
# what the block layer actually fetched for it (`/proc/<pid>/io`), and `psi_io_full` the
# share of the last ten seconds in which *every* runnable task was blocked on I/O. Together
# they separate "reading a lot" from "reading a page at a time".
EXTRA = ("majflt", "read_bytes", "psi_io_full")
INTERVAL = 0.1

# `stage       TextIndex    96.35s  rows=1234        peak=  2246 MiB`
STAGE_RE = re.compile(
    r"^stage\s+(?P<stage>\S+)\s+(?P<secs>[\d.]+)s\s+rows=(?P<rows>\d+)\s+peak=\s*(?P<peak>\d+)\s*MiB"
)


def read_status(pid: int) -> dict[str, int] | None:
    """The five figures in KiB, or None once the process is gone."""
    try:
        with open(f"/proc/{pid}/status", "rb") as fh:
            raw = fh.read().decode("utf-8", "replace")
    except (FileNotFoundError, ProcessLookupError):
        return None
    out: dict[str, int] = {}
    for line in raw.splitlines():
        key, _, rest = line.partition(":")
        if key in FIELDS:
            out[key] = int(rest.split()[0])
    return out or None


def read_extra(pid: int) -> dict[str, float]:
    """Major faults, bytes fetched from the block layer, and the box's I/O pressure."""
    out: dict[str, float] = {"majflt": 0, "read_bytes": 0, "psi_io_full": 0.0}
    try:
        with open(f"/proc/{pid}/stat", "rb") as fh:
            raw = fh.read().decode("utf-8", "replace")
        # The command field may hold spaces and parentheses; everything after the last ")"
        # is positional, and `majflt` is the tenth of those (field 12 overall).
        tail = raw[raw.rindex(")") + 2 :].split()
        out["majflt"] = int(tail[9])
    except (OSError, ValueError, IndexError):
        pass
    try:
        with open(f"/proc/{pid}/io", "rb") as fh:
            for line in fh.read().decode().splitlines():
                if line.startswith("read_bytes:"):
                    out["read_bytes"] = int(line.split()[1])
    except OSError:
        pass
    try:
        with open("/proc/pressure/io", "rb") as fh:
            for line in fh.read().decode().splitlines():
                if line.startswith("full"):
                    out["psi_io_full"] = float(line.split()[1].split("=")[1])
    except OSError:
        pass
    return out


def run(out_prefix: Path, argv: list[str]) -> int:
    rss_path = out_prefix.with_suffix(".rss.csv")
    stage_path = out_prefix.with_suffix(".stages.csv")
    out_prefix.parent.mkdir(parents=True, exist_ok=True)

    t0 = time.monotonic()
    proc = subprocess.Popen(argv, stderr=subprocess.PIPE, stdout=None, text=True, bufsize=1)

    def pump() -> None:
        with open(stage_path, "w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["t", "line"])
            assert proc.stderr is not None
            for line in proc.stderr:
                line = line.rstrip("\n")
                w.writerow([f"{time.monotonic() - t0:.3f}", line])
                fh.flush()
                print(line, file=sys.stderr, flush=True)

    pump_thread = threading.Thread(target=pump, daemon=True)
    pump_thread.start()

    with open(rss_path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["t", *FIELDS, *EXTRA])
        while True:
            status = read_status(proc.pid)
            if status is None:
                break
            extra = read_extra(proc.pid)
            w.writerow([
                f"{time.monotonic() - t0:.3f}",
                *(status.get(f, 0) for f in FIELDS),
                *(extra[f] for f in EXTRA),
            ])
            fh.flush()
            if proc.poll() is not None:
                # One last sample was already taken above; the process may still be a
                # zombie with a readable status, so stop on the exit rather than on it.
                break
            time.sleep(INTERVAL)

    rc = proc.wait()
    pump_thread.join(timeout=5)
    return rc


def report(out_prefix: Path) -> None:
    rss_path = out_prefix.with_suffix(".rss.csv")
    stage_path = out_prefix.with_suffix(".stages.csv")
    samples = []
    with open(rss_path) as fh:
        for row in csv.DictReader(fh):
            sample = {f: int(row[f]) for f in FIELDS}
            for f in EXTRA:
                sample[f] = float(row.get(f, 0) or 0)
            samples.append((float(row["t"]), sample))

    stages = []
    with open(stage_path) as fh:
        for row in csv.DictReader(fh):
            m = STAGE_RE.match(row["line"])
            if m:
                stages.append(
                    (float(row["t"]), m["stage"], float(m["secs"]), int(m["rows"]), int(m["peak"]))
                )

    print(f"{'stage':>16} {'secs':>8} {'rows':>14} {'anonMB':>9} {'fileMB':>9} "
          f"{'rssMB':>9} {'VmHWM_MB':>9} {'majflt/s':>9} {'readMB/s':>9} {'psiIO%':>7} {'n':>4}")
    prev = 0.0
    for t_end, stage, secs, rows, peak in stages:
        window = [s for t, s in samples if prev <= t <= t_end]
        marker = ""
        if not window:
            # Shorter than the sample interval: take the nearest sample rather than
            # interpolating, and say so.
            nearest = min(samples, key=lambda s: abs(s[0] - t_end))
            window = [nearest[1]]
            marker = " *"
        anon = max(s["RssAnon"] for s in window)
        filed = max(s["RssFile"] + s["RssShmem"] for s in window)
        rss = max(s["VmRSS"] for s in window)
        span = max(t_end - prev, 1e-9)
        faults = (window[-1]["majflt"] - window[0]["majflt"]) / span
        readmb = (window[-1]["read_bytes"] - window[0]["read_bytes"]) / span / (1 << 20)
        psi = max(s["psi_io_full"] for s in window)
        print(f"{stage:>16} {secs:>8.2f} {rows:>14,} {anon / 1024:>9.0f} {filed / 1024:>9.0f} "
              f"{rss / 1024:>9.0f} {peak:>9,} {faults:>9.0f} {readmb:>9.1f} {psi:>7.1f} "
              f"{len(window):>4}{marker}")
        prev = t_end
    if samples:
        anon = max(s["RssAnon"] for _, s in samples)
        filed = max(s["RssFile"] + s["RssShmem"] for _, s in samples)
        rss = max(s["VmRSS"] for _, s in samples)
        hwm = max(s["VmHWM"] for _, s in samples)
        span = max(samples[-1][0] - samples[0][0], 1e-9)
        faults = (samples[-1][1]["majflt"] - samples[0][1]["majflt"]) / span
        readmb = (samples[-1][1]["read_bytes"] - samples[0][1]["read_bytes"]) / span / (1 << 20)
        psi = max(s["psi_io_full"] for _, s in samples)
        print(f"{'WHOLE RUN':>16} {samples[-1][0]:>8.2f} {'':>14} {anon / 1024:>9.0f} "
              f"{filed / 1024:>9.0f} {rss / 1024:>9.0f} {hwm / 1024:>9.0f} {faults:>9.0f} "
              f"{readmb:>9.1f} {psi:>7.1f} {len(samples):>4}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, required=True, help="CSV path prefix")
    ap.add_argument("--report", action="store_true", help="report over an existing prefix")
    ap.add_argument("argv", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    if args.report:
        report(args.out)
        return
    argv = args.argv[1:] if args.argv and args.argv[0] == "--" else args.argv
    if not argv:
        ap.error("nothing to run — pass the command after `--`")
    sys.exit(run(args.out, argv))


if __name__ == "__main__":
    main()
