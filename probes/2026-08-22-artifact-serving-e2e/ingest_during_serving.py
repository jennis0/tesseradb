"""Ingest during serving: sustained point ingest carrying the attribute column while sessions pan.

Two questions, asked of the two planes separately because they have different postures. **Does the
read path degrade** — request p50/p99 with ingest running against the same figures with it stopped,
from one uninterrupted load run windowed at the moment ingest starts. **Does the write path** — the
ingest batch's own latency, which the write-latency budget puts in seconds and which a serving load
should not move beyond that.

And the freshness spot check between them: an artifact's masked count before, and after the flush
that publishes what arrived. **The two membership kinds answer differently and both are correct.**
A predicate layer reads a value column, so a point ingested with value *v* counts at its flush; an
enumerated membership projects through *base* rows, so a growth counts at the **fold** that makes
them base rows and understates until then — fail-closed, `annotation-write-cycle.md` §4.1's
posture, and the campaign records both rather than calling the second a defect.

The load is the Rust arm (`concurrency.py`'s docstring records why).

Run: `python3 ingest_during_serving.py --work DIR --n N --sessions 32 --seconds 120`
"""

from __future__ import annotations

import argparse
import csv
import json
import statistics
import subprocess
import time
from pathlib import Path

import requests

import campaign as C
from concurrency import mix_for
from grid import percentile

WHOLE_MAP = C.Viewport("100%", [0.0, 0.0, C.GRID, C.GRID], 0, 1.0)


def counts_for(server: C.Server, token: str, layer: str, key: str) -> int | None:
    _s, _b, artifacts, _t = C.viewport_request(server, token, WHOLE_MAP, [layer])
    for a in artifacts:
        if a.key == key:
            return a.masked_count
    return None


def window(samples: list[tuple[float, float]], lo: float, hi: float) -> dict:
    picked = [d for at, d in samples if lo <= at < hi]
    if not picked:
        return {"requests": 0}
    return {
        "requests": len(picked),
        "p50_ms": round(statistics.median(picked) * 1000, 2),
        "p99_ms": round(percentile(picked, 0.99) * 1000, 2),
        "max_ms": round(max(picked) * 1000, 2),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--n", required=True, type=int)
    ap.add_argument("--seed", type=int, default=20_260_822)
    ap.add_argument("--terms-per-level", type=int, default=65_536)
    ap.add_argument("--sessions", type=int, default=32)
    ap.add_argument("--seconds", type=float, default=120.0)
    ap.add_argument("--layer", default=C.LAYER_PARTITION_ATTR)
    ap.add_argument("--membership-layer", default=C.LAYER_PARTITION_ENUM)
    ap.add_argument("--rows", type=int, default=2_000_000)
    ap.add_argument("--flush-budget", type=float, default=300.0,
                    help="how long to wait for the flush's rows to reach the served count")
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}
    observer_terms = grants["0.9375"]["grant"].split(",")
    access_override = ",".join(observer_terms[:4])
    watched_key = "1"

    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)
    server = C.Server(work, *ports)
    server.spawn()
    report: dict = {"n": args.n, "sessions": args.sessions, "layer": args.layer}
    samples_path = work / "ingest-samples.csv"
    load = None
    try:
        observer, _ = server.authorise(observer_terms)
        report["before"] = {
            "attribute": counts_for(server, observer, args.layer, watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }
        shed_before = server.status()["compute"]["shed_total"]

        counts: dict[str, int] = {}
        for breadth in mix_for(args.sessions, broad_cap=8):
            counts[breadth] = counts.get(breadth, 0) + 1
        argv = [
            str(C.REPO_ROOT / "target" / "release" / "artifact_campaign_load"),
            "--viewer", server.viewer_base,
            "--session", server.session_base,
            "--session-credential", C.SESSION_CREDENTIAL,
            "--fixture-json", str(work / "fixture" / "fixture.json"),
            "--layer", args.layer,
            "--seconds", str(args.seconds),
            "--server-pid", str(server.proc.pid),
            "--samples-out", str(samples_path),
            "--out", str(work / "ingest-load.json"),
        ]
        for breadth, count in counts.items():
            argv += ["--mix", f"{breadth}={count}"]
        report["mix"] = counts
        load = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        time.sleep(3.0)
        t0 = time.monotonic()

        # The quiet half, then the loaded half — one uninterrupted load run, windowed.
        time.sleep(args.seconds / 2)
        ingest_started = time.monotonic() - t0
        ingest = subprocess.run(
            [
                str(C.REPO_ROOT / "target" / "release" / "artifact_campaign_ingest"),
                "--seed", str(args.seed), "--n", str(args.n),
                "--terms-per-level", str(args.terms_per_level),
                "--control", server.control_base,
                "--credential", C.OPERATOR_CREDENTIAL,
                "--rows", str(args.rows),
                "--batch-rows", "2000",
                "--seconds", str(args.seconds / 2 - 5),
                "--access", access_override,
                "--partition-value", watched_key,
                "--membership-layer", args.membership_layer,
                "--membership-key", watched_key,
                "--batch-prefix", "load",
            ],
            capture_output=True, text=True,
        )
        ingest_stopped = time.monotonic() - t0
        report["ingest"] = (
            json.loads(ingest.stdout) if ingest.stdout.strip().startswith("{")
            else {"stdout": ingest.stdout, "stderr": ingest.stderr[-2000:]}
        )
        report["peak_rss_bytes"] = server.rss_bytes()

        load.wait(timeout=args.seconds + 300)
        load = None

        # **The wait is on the answer, not on a gauge** — `fold_under_load.py` records why: under a
        # live load `buffered_items` does not reach zero, and a run that waited for it spent its
        # whole deadline before reading a count that had moved minutes earlier. The claim under
        # test is that the count moves at its flush, so that is what is polled, and how long it
        # took is the figure.
        flush_asked = time.monotonic()
        requests.post(
            f"{server.control_base}/control/flush",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=600,
        )
        deadline = flush_asked + args.flush_budget
        attribute_now = report["before"]["attribute"]
        while time.monotonic() < deadline:
            attribute_now = counts_for(server, observer, args.layer, watched_key)
            if attribute_now != report["before"]["attribute"]:
                break
            time.sleep(1.0)
        report["after_flush"] = {
            "seconds_after_flush_request": round(time.monotonic() - flush_asked, 2),
            "moved": attribute_now != report["before"]["attribute"],
            "attribute": attribute_now,
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }
        report["shed_during_run"] = server.status()["compute"]["shed_total"] - shed_before
        report["load"] = json.loads((work / "ingest-load.json").read_text())
    finally:
        if load is not None:
            load.terminate()
        server.stop()

    if samples_path.exists():
        with samples_path.open() as handle:
            samples = [(float(r["start_s"]), float(r["latency_s"])) for r in csv.DictReader(handle)]
        offset = 3.0
        report["read_path"] = {
            "quiet": window(samples, offset, offset + ingest_started),
            "under_ingest": window(samples, offset + ingest_started, offset + ingest_stopped),
        }

    out = args.out or (work / "ingest-during-serving.json")
    out.write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items() if k != "load"}, indent=2))
    print(f"-> {out}")


if __name__ == "__main__":
    main()
