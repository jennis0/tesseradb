"""Ingest during serving: sustained point ingest carrying the attribute column while sessions pan.

Two questions, and they are asked of the two planes separately because they have different
postures. **Does the read path degrade** — request p50/p99 with ingest running against the same
figures with it stopped. **Does the write path** — the ingest batch's own latency, which the
write-latency budget puts in seconds and which a serving load should not move beyond that.

And the freshness spot check between them: an artifact's masked count before, and after the flush
that publishes what arrived. **The two membership kinds answer differently and both are correct.**
A predicate layer reads a value column, so a point ingested with value *v* counts at its flush; an
enumerated membership projects through *base* rows, so a growth counts at the **fold** that makes
them base rows and understates until then — fail-closed, `annotation-write-cycle.md` §4.1's
posture, and the campaign records both rather than calling the second a defect.

Run: `python3 ingest_during_serving.py --work DIR --n N --sessions 32 --seconds 60`
"""

from __future__ import annotations

import argparse
import json
import random
import statistics
import subprocess
import threading
import time
from pathlib import Path

import requests

import campaign as C
from concurrency import mix_for, pan_route
from grid import percentile

WHOLE_MAP = C.Viewport("100%", [0.0, 0.0, C.GRID, C.GRID], 0, 1.0)


def counts_for(server: C.Server, token: str, layer: str, key: str) -> int | None:
    _s, _b, artifacts, _t = C.viewport_request(server, token, WHOLE_MAP, [layer])
    for a in artifacts:
        if a.key == key:
            return a.masked_count
    return None


def stats(samples: list[float]) -> dict:
    if not samples:
        return {"requests": 0}
    return {
        "requests": len(samples),
        "p50_ms": round(statistics.median(samples) * 1000, 2),
        "p99_ms": round(percentile(samples, 0.99) * 1000, 2),
        "max_ms": round(max(samples) * 1000, 2),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--n", required=True, type=int)
    ap.add_argument("--seed", type=int, default=20_260_822)
    ap.add_argument("--terms-per-level", type=int, default=65_536)
    ap.add_argument("--sessions", type=int, default=32)
    ap.add_argument("--seconds", type=float, default=60.0)
    ap.add_argument("--layer", default=C.LAYER_PARTITION_ATTR)
    ap.add_argument("--membership-layer", default=C.LAYER_PARTITION_ENUM)
    ap.add_argument("--rows", type=int, default=500_000)
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
    try:
        observer, _ = server.authorise(observer_terms)
        report["before"] = {
            "attribute": counts_for(server, observer, args.layer, watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }

        mix = mix_for(args.sessions, broad_cap=8)
        tokens = [server.authorise(grants[b]["grant"].split(","))[0] for b in mix]
        quiet: list[float] = []
        loaded: list[float] = []
        errors: list[str] = []
        lock = threading.Lock()
        stop = threading.Event()
        ingesting = threading.Event()

        def session(index: int, token: str) -> None:
            rng = random.Random(0x1465 + index)
            route = pan_route(rng)
            step = 0
            while not stop.is_set():
                vp = route[step % len(route)]
                step += 1
                writing = ingesting.is_set()
                try:
                    seconds, _b, _a, _t = C.viewport_request(
                            server, token, vp, [args.layer], decode=False
                        )
                    with lock:
                        (loaded if writing else quiet).append(seconds)
                except Exception as exc:
                    with lock:
                        errors.append(f"{type(exc).__name__}: {exc}"[:200])
                    time.sleep(0.05)

        threads = [threading.Thread(target=session, args=(i, t), daemon=True)
                   for i, t in enumerate(tokens)]
        for t in threads:
            t.start()

        time.sleep(args.seconds / 2)          # the quiet half
        ingesting.set()
        ingest = subprocess.run(
            [
                str(C.REPO_ROOT / "target" / "release" / "artifact_campaign_ingest"),
                "--seed", str(args.seed), "--n", str(args.n),
                "--terms-per-level", str(args.terms_per_level),
                "--control", server.control_base,
                "--credential", C.OPERATOR_CREDENTIAL,
                "--rows", str(args.rows),
                "--batch-rows", "2000",
                "--seconds", str(args.seconds / 2),
                "--access", access_override,
                "--partition-value", watched_key,
                "--membership-layer", args.membership_layer,
                "--membership-key", watched_key,
                "--batch-prefix", "load",
            ],
            capture_output=True, text=True,
        )
        ingesting.clear()
        report["ingest"] = (
            json.loads(ingest.stdout) if ingest.stdout.strip().startswith("{")
            else {"stdout": ingest.stdout, "stderr": ingest.stderr[-2000:]}
        )
        peak_rss = server.rss_bytes()
        stop.set()
        for t in threads:
            t.join(timeout=300)

        status_before_flush = server.status()
        requests.post(
            f"{server.control_base}/control/flush",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=600,
        )
        deadline = time.monotonic() + 900
        flushes_before = status_before_flush["write_executor"]["flush"]["flushes"]
        while time.monotonic() < deadline:
            st = server.status()
            if st["write_executor"]["flush"]["flushes"] > flushes_before and \
               st["write_executor"]["flush"]["buffered_items"] == 0:
                break
            time.sleep(0.25)
        report["after_flush"] = {
            "attribute": counts_for(server, observer, args.layer, watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }
        report["read_path"] = {"quiet": stats(quiet), "under_ingest": stats(loaded)}
        report["errors"] = len(errors)
        report["error_examples"] = errors[:5]
        report["peak_rss_bytes"] = peak_rss
        report["status"] = server.status()
    finally:
        server.stop()

    out = args.out or (work / "ingest-during-serving.json")
    out.write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items() if k != "status"}, indent=2))
    print(f"-> {out}")


if __name__ == "__main__":
    main()
