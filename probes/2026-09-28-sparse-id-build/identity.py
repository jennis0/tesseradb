"""Time a build's identity pass alone: build under a memory cap and stop once `source_ids` ends.

    python3 identity.py --binary mosaica --data DIR --cap 7G --budget 6g --out run.json

Reports the stage's wall time from the build's own `--stage-timings` line, and the bytes the
process read and wrote and its major faults up to that point.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path


def read_io(pid: int) -> tuple[int, int, int] | None:
    try:
        io = dict(line.split(": ") for line in Path(f"/proc/{pid}/io").read_text().splitlines())
        faults = int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[9])
        return int(io["read_bytes"]), int(io["write_bytes"]), faults
    except (OSError, KeyError, ValueError, IndexError):
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--cap", required=True)
    ap.add_argument("--budget", required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    data = args.data.resolve()
    bundle = data / "bundle-identity"
    if bundle.exists():
        shutil.rmtree(bundle)
    cmd = [
        "systemd-run", "--user", "--scope", "--collect", "-p", f"MemoryMax={args.cap}",
        "-p", "MemorySwapMax=2G", "--",
        str(args.binary.resolve()), "build", "--deployment", str(data / "mosaica.toml"),
        "--config", str(data / "corpus.toml"), "--out", str(bundle), "--no-oracle-pairs",
        "--memory-budget", args.budget, "--stage-timings",
    ]
    began = time.time()
    child = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    last = None
    stage = None
    lines = []
    for line in child.stderr:
        lines.append(line.rstrip())
        if child.pid and (sample := read_io(child.pid)):
            last = sample
        found = re.search(r"stage\s+source_ids\s+([\d.]+)s", line)
        if found:
            stage = float(found.group(1))
            last = read_io(child.pid) or last
            child.send_signal(signal.SIGKILL)
            break
    child.wait()
    if bundle.exists():
        shutil.rmtree(bundle)
    result = {
        "binary": str(args.binary),
        "cap": args.cap,
        "budget": args.budget,
        "source_ids_s": stage,
        "wall_to_stage_end_s": round(time.time() - began, 1),
        "read_mib": last[0] >> 20 if last else None,
        "written_mib": last[1] >> 20 if last else None,
        "major_faults": last[2] if last else None,
        "log": [line for line in lines if line.startswith(("identity", "stage", "build"))],
    }
    args.out.write_text(json.dumps(result, indent=1) + "\n")
    print(json.dumps({k: v for k, v in result.items() if k != "log"}), file=sys.stderr)
    return 0 if stage is not None else 1


if __name__ == "__main__":
    sys.exit(main())
