"""Build a corpus whose one unique id is sparse and out of file order, and measure the build.

    python3 probe.py make --items 100000000 --order shuffled --out DIR
    python3 probe.py run --binary tessera --data DIR --cap 600M --budget 500m --out run.json
    python3 probe.py run ... --main      # the declaration as main reads it, with a join field

`make` writes `points.parquet` (`id` u64, `x`, `y`) and `members.parquet` (`entity` u64, `key` i32), one
row per item in each. The ids have GBIF's spacing: blocks of ten ids over eighteen values, the two
gaps of one in each block placed at random, so the gaps average 1.8. With `--order shuffled` the
points file holds the ranks in the order `(a * row + c) mod items` and the members file in another
such order; with `--order aligned` the members file holds them in the points' order, as GBIF's
does; with `--order file` both hold them ascending.

`run` builds under a transient systemd scope with `MemoryMax` and `MemorySwapMax=2G` and samples
the process once a second: resident and anonymous memory, the scope's charge, bytes read and written
and major faults. Each stage of `--stage-timings-json` gets the bytes and faults sampled inside it.
The bundle is removed after the run.
"""

from __future__ import annotations

import argparse
import itertools
import json
import shutil
import subprocess
import sys
import time
from datetime import datetime
from math import gcd
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

CHUNK = 4_000_000
KEYS = 1000

# For each of the 45 ways to place two gaps of one among a block's ten (the tenth leads to the next
# block, eighteen values on), the offset of each id in its block.
PATTERNS = np.array(
    [
        np.concatenate(([0], np.cumsum([1 if g in ones else 2 for g in range(9)])))
        for ones in itertools.combinations(range(10), 2)
    ],
    dtype=np.uint64,
)


def mix(x: np.ndarray) -> np.ndarray:
    """splitmix64's finaliser, elementwise."""
    x = x.astype(np.uint64, copy=True)
    x ^= x >> np.uint64(30)
    x *= np.uint64(0xBF58476D1CE4E5B9)
    x ^= x >> np.uint64(27)
    x *= np.uint64(0x94D049BB133111EB)
    x ^= x >> np.uint64(31)
    return x


def ids_of(ranks: np.ndarray) -> np.ndarray:
    block = ranks // np.uint64(10)
    pattern = mix(block) % np.uint64(len(PATTERNS))
    return np.uint64(1_000_000_007) + block * np.uint64(18) + PATTERNS[pattern, ranks % np.uint64(10)]


def multiplier(items: int, near: float) -> int:
    a = int(items * near) | 1
    while gcd(a, items) != 1:
        a += 2
    return a


def ranks_at(rows: np.ndarray, items: int, order: str, a: int, c: int) -> np.ndarray:
    if order == "file":
        return rows
    return (rows * np.uint64(a) + np.uint64(c)) % np.uint64(items)


def make(args: argparse.Namespace) -> None:
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)
    n = args.items
    points_order = (multiplier(n, 0.6180339887), n // 3)
    members_order = (multiplier(n, 0.7548776662), n // 7)
    points = pq.ParquetWriter(
        out / "points.parquet",
        pa.schema([("id", pa.uint64()), ("x", pa.float64()), ("y", pa.float64())]),
        compression="zstd",
    )
    members = pq.ParquetWriter(
        out / "members.parquet",
        pa.schema([("entity", pa.uint64()), ("key", pa.int32())]),
        compression="zstd",
    )
    began = time.time()
    for first in range(0, n, CHUNK):
        rows = np.arange(first, min(first + CHUNK, n), dtype=np.uint64)
        ranks = ranks_at(rows, n, args.order, *points_order)
        h = mix(ranks + np.uint64(0x9E3779B97F4A7C15))
        x = (h >> np.uint64(11)).astype(np.float64) / float(1 << 53) * 100.0
        y = (mix(h) >> np.uint64(11)).astype(np.float64) / float(1 << 53) * 100.0
        points.write_table(
            pa.table({"id": ids_of(ranks), "x": x, "y": y}), row_group_size=1 << 20
        )
        if args.order != "aligned":
            ranks = ranks_at(rows, n, args.order, *members_order)
        key = (mix(ranks) % np.uint64(KEYS)).astype(np.int32)
        members.write_table(
            pa.table({"entity": ids_of(ranks), "key": key}), row_group_size=1 << 20
        )
        print(f"\r{first + len(rows):,} rows, {time.time() - began:.0f}s", end="", file=sys.stderr)
    points.close()
    members.close()
    print(file=sys.stderr)
    (out / "tessera.toml").write_text(
        """[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:18141"
session = "127.0.0.1:18142"
control = "127.0.0.1:18143"
"""
    )
    (out / "corpus.toml").write_text(declaration(main=False))
    (out / "corpus-main.toml").write_text(declaration(main=True))


def declaration(main: bool) -> str:
    join = 'join_field = "id"\n' if main else ""
    fields = "" if main else '  fields = { id = "entity" }\n'
    return f"""[sources]
points  = "points.parquet"
members = "members.parquet"

[defaults]
source = "points"
{join}
[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 100.0 }}
point_visibility = {{ default = "public" }}

[[attribute]]
name   = "id"
type   = "u64"
unique = true

[[layer]]
name                      = "groups"
views                     = ["s0"]
membership                = "enumerated"
value_set                 = "open"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "any"

  [layer.members]
  source = "members"
{fields}"""


def read_kib(pid: int, key: str) -> int | None:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(key + ":"):
                return int(line.split()[1])
    except OSError:
        return None
    return None


def read_io(pid: int) -> tuple[int, int] | None:
    try:
        io = dict(line.split(": ") for line in Path(f"/proc/{pid}/io").read_text().splitlines())
        return int(io["read_bytes"]), int(io["write_bytes"])
    except (OSError, KeyError, ValueError):
        return None


def read_majflt(pid: int) -> int | None:
    try:
        return int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[9])
    except (OSError, IndexError, ValueError):
        return None


def build_pid(scope_pid: int) -> int | None:
    """The tessera process: systemd-run execs it in place, so it is the scope's own pid."""
    try:
        comm = Path(f"/proc/{scope_pid}/comm").read_text().strip()
    except OSError:
        return None
    return scope_pid if comm.startswith("tessera") else None


def cgroup_of(pid: int) -> Path | None:
    try:
        rel = Path(f"/proc/{pid}/cgroup").read_text().strip().split("::", 1)[1]
    except (OSError, IndexError):
        return None
    return Path("/sys/fs/cgroup") / rel.lstrip("/")


def stamp(when: str | float) -> float:
    if isinstance(when, (int, float)):
        return float(when)
    return datetime.fromisoformat(when.replace("Z", "+00:00")).timestamp()


def run(args: argparse.Namespace) -> int:
    data: Path = args.data.resolve()
    bundle = data / "bundle-probe"
    stages_path = data / "stages.json"
    for stale in (bundle, stages_path):
        if stale.is_dir():
            shutil.rmtree(stale)
        elif stale.exists():
            stale.unlink()
    cmd = [
        str(args.binary.resolve()), "build",
        "--deployment", str(data / "tessera.toml"),
        "--config", str(data / ("corpus-main.toml" if args.main else "corpus.toml")),
        "--out", str(bundle),
        "--no-oracle-pairs",
        "--stage-timings-json", str(stages_path),
    ]
    if args.budget:
        cmd += ["--memory-budget", args.budget]
    scoped = ["systemd-run", "--user", "--scope", "--collect", "-p", f"MemoryMax={args.cap}",
              "-p", "MemorySwapMax=2G",
              *(["-p", f"RuntimeMaxSec={args.max_seconds}"] if args.max_seconds else []),
              "--", *cmd]
    began = time.time()
    samples: list[list] = []
    peak = None
    log_path = args.out.with_suffix(".log")
    with open(log_path, "wb") as log:
        child = subprocess.Popen(scoped, stdout=log, stderr=subprocess.STDOUT)
        group = None
        while child.poll() is None:
            pid = build_pid(child.pid)
            if pid is not None:
                if group is None or not group.name.endswith(".scope"):
                    group = cgroup_of(pid)
                io = read_io(pid) or (None, None)
                current = None
                if group is not None:
                    try:
                        current = int((group / "memory.current").read_text()) // 1024
                        peak = int((group / "memory.peak").read_text()) // 1024
                    except (OSError, ValueError):
                        pass
                samples.append([
                    round(time.time() - began, 2), read_kib(pid, "VmRSS"), read_kib(pid, "RssAnon"),
                    current, io[0], io[1], read_majflt(pid),
                ])
            time.sleep(1.0)
    wall = time.time() - began
    stages = json.loads(stages_path.read_text()) if stages_path.exists() else []
    stages = stages if isinstance(stages, list) else stages.get("stages", [])

    def delta(column: int, lo: float, hi: float) -> int | None:
        inside = [s[column] for s in samples if lo - 1 <= s[0] <= hi + 1 and s[column] is not None]
        return inside[-1] - inside[0] if len(inside) > 1 else None

    for stage in stages:
        lo, hi = stamp(stage["started_at"]) - began, stamp(stage["ended_at"]) - began
        stage["read_mib"] = (delta(4, lo, hi) or 0) >> 20
        stage["written_mib"] = (delta(5, lo, hi) or 0) >> 20
        stage["major_faults"] = delta(6, lo, hi)
        inside = [s for s in samples if lo - 1 <= s[0] <= hi + 1]
        stage["peak_anon_mib"] = max((s[2] or 0) for s in inside) >> 10 if inside else None
    last = samples[-1] if samples else [None] * 7
    result = {
        "cmd": cmd,
        "cap": args.cap,
        "exit": child.returncode,
        "wall_s": round(wall, 1),
        "peak_anon_mib": max((s[2] or 0) for s in samples) >> 10 if samples else None,
        "scope_peak_mib": peak >> 10 if peak else None,
        "read_mib": (last[4] or 0) >> 20,
        "written_mib": (last[5] or 0) >> 20,
        "major_faults": last[6],
        "stages": stages,
        "samples": samples,
    }
    args.out.write_text(json.dumps(result, indent=1) + "\n")
    if bundle.exists():
        shutil.rmtree(bundle)
    print(f"exit {child.returncode}, wall {wall:.1f}s, peak anon {result['peak_anon_mib']} MiB, "
          f"read {result['read_mib']} MiB, written {result['written_mib']} MiB, "
          f"major faults {result['major_faults']}", file=sys.stderr)
    return child.returncode


def main() -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="command", required=True)
    m = sub.add_parser("make")
    m.add_argument("--items", type=int, required=True)
    m.add_argument("--order", choices=["shuffled", "aligned", "file"], required=True)
    m.add_argument("--out", type=Path, required=True)
    r = sub.add_parser("run")
    r.add_argument("--binary", type=Path, required=True)
    r.add_argument("--data", type=Path, required=True)
    r.add_argument("--cap", required=True)
    r.add_argument("--budget")
    r.add_argument("--main", action="store_true")
    r.add_argument("--max-seconds", type=int, help="stop the build after this long")
    r.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    if args.command == "make":
        make(args)
        return 0
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
