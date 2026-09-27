"""Open the rung under a cap, run the six-principal serve battery against it, and record the
open and the server's memory while the battery ran.

    python3 serve_run.py --corpus data/ladder/gbif --results serve.json --cap 24G -- <battery args>
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from probe import Server, record  # noqa: E402

REPO = Path(__file__).resolve().parents[2]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", type=Path, required=True)
    ap.add_argument("--results", type=Path, required=True)
    ap.add_argument("--cap", default="24G")
    ap.add_argument("--battery-out", type=Path, required=True)
    ap.add_argument("battery", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    corpus = args.corpus.resolve()
    server = Server(corpus, args.cap)
    try:
        opened = server.start()
        opened["peaks_to_ready"] = server.peaks_since(server.began)
        record(args.results, "serve_open", opened)
        rel = Path(f"/proc/{server.child.pid}/cgroup").read_text().strip().split("::")[1]
        cgroup = Path("/sys/fs/cgroup") / rel.lstrip("/")
        extra = args.battery[1:] if args.battery[:1] == ["--"] else args.battery
        cmd = [sys.executable, "-m", "test_corpora.common.serve_battery",
               "--viewer", server.viewer, "--session", server.session,
               "--session-cred", server.session_cred, "--bundle", str(corpus / "bundle"),
               "--cache", str(corpus / ".tessera" / "cache"),
               "--ranks", str(corpus / "country-ranks.json"),
               "--server-pid", str(server.child.pid), "--cgroup", str(cgroup),
               "--out", str(args.battery_out), *extra]
        t0 = time.time()
        code = subprocess.run(cmd, cwd=REPO).returncode
        oom = (cgroup / "memory.events").read_text() if cgroup.exists() else None
        record(args.results, "battery_run", {
            "exit": code, "wall_s": round(time.time() - t0, 1),
            "memory": server.peaks_since(t0), "memory_at_end": server.memory(),
            "memory_events": oom,
            "cmd": [c if c != server.session_cred else "<session credential>" for c in cmd[3:]]})
    finally:
        server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
