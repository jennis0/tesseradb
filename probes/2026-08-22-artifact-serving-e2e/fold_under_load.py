"""Serving during a fold — the named gap. The fold's artifact pass has only ever run on an idle box.

Thirty-two mixed sessions pan continuously; ingest lands enough rows to give the fold something to
execute; `POST /control/compact` schedules it (the nightly window is `off` in the campaign's
deployment, so the fold is asked for on demand rather than waited for — the same `request_fold`
the window's tick calls, which is what makes this a measurement of the fold and not of the clock).
The three things recorded are the ones the plan names:

- **request latency through the fold** — p50/p99 in three windows, before, during and after, from
  the same sessions without re-establishing them, so a degradation is the fold's and not a cold
  cache's;
- **the fold's own duration under load**, taken from `/control/status`'s `compaction.folds`
  counter moving, against the 32.8 s unloaded figure the artifact pass was measured at;
- **freshness across it** — an artifact's masked count before the ingest, after its flush, and
  after the fold, which is the claim that a membership is whole on the far side.

Run: `python3 fold_under_load.py --work DIR --sessions 32 --seconds 60`
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import threading
import time
from pathlib import Path

import campaign as C
from concurrency import mix_for, pan_route
from grid import percentile

import random


WHOLE_MAP = C.Viewport("100%", [0.0, 0.0, C.GRID, C.GRID], 0, 1.0)


def counts_for(server: C.Server, token: str, layer: str) -> dict[str, int]:
    _s, _b, artifacts, _t = C.viewport_request(server, token, WHOLE_MAP, [layer])
    return {a.key: a.masked_count for a in artifacts if a.key is not None}


def window(samples: list[tuple[float, float]], lo: float, hi: float) -> dict:
    """The latency statistics of every request that *started* inside `[lo, hi)`."""
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
    ap.add_argument("--seconds", type=float, default=180.0)
    ap.add_argument("--settle", type=float, default=20.0, help="the before-window's length")
    ap.add_argument("--layer", default=C.LAYER_PARTITION_ATTR)
    ap.add_argument("--membership-layer", default=C.LAYER_PARTITION_ENUM)
    ap.add_argument("--ingest-rows", type=int, default=200_000)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}
    # The freshness observer is the broadest principal, and the ingested rows carry its own access
    # label — a check whose new members are invisible to the observer passes vacuously.
    observer_grant = grants["0.9375"]
    observer_terms = observer_grant["grant"].split(",")
    access_override = ",".join(observer_terms[:4])
    # The artifact whose count is watched: key "1", which exists in every arm's roster at every
    # tier (the partition arm's second artifact) and is therefore not a size-dependent choice.
    watched_key = "1"

    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)
    server = C.Server(work, *ports)
    server.spawn()
    report: dict = {"sessions": args.sessions, "layer": args.layer, "n": args.n}
    try:
        observer, _ = server.authorise(observer_terms)
        report["before"] = {
            "attribute": counts_for(server, observer, args.layer).get(watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer).get(watched_key),
        }
        status0 = server.status()
        folds_before = status0["compaction"]["folds"]

        mix = mix_for(args.sessions, broad_cap=8)
        tokens = [(b, server.authorise(grants[b]["grant"].split(","))[0]) for b in mix]

        samples: list[tuple[float, float]] = []
        errors: list[str] = []
        lock = threading.Lock()
        stop = threading.Event()
        rss: list[tuple[float, int]] = []
        t0 = time.monotonic()

        def session(index: int, token: str) -> None:
            rng = random.Random(0xF01D + index)
            route = pan_route(rng)
            step = 0
            while not stop.is_set():
                vp = route[step % len(route)]
                step += 1
                at = time.monotonic() - t0
                try:
                    seconds, _b, _a, _t = C.viewport_request(
                            server, token, vp, [args.layer], decode=False
                        )
                    with lock:
                        samples.append((at, seconds))
                except Exception as exc:
                    with lock:
                        errors.append(f"{type(exc).__name__}: {exc}"[:200])
                    time.sleep(0.05)

        threads = [threading.Thread(target=session, args=(i, t), daemon=True)
                   for i, (_b, t) in enumerate(tokens)]
        for t in threads:
            t.start()

        # ---- before window ----------------------------------------------------------------
        while time.monotonic() - t0 < args.settle:
            time.sleep(0.5)
            rss.append((time.monotonic() - t0, server.rss_bytes()))

        # ---- ingest, flush, then ask for the fold -------------------------------------------
        ingest_started = time.monotonic() - t0
        ingest = subprocess.run(
            [
                str(C.REPO_ROOT / "target" / "release" / "artifact_campaign_ingest"),
                "--seed", str(args.seed), "--n", str(args.n),
                "--terms-per-level", str(args.terms_per_level),
                "--control", server.control_base,
                "--credential", C.OPERATOR_CREDENTIAL,
                "--rows", str(args.ingest_rows),
                "--batch-rows", "2000",
                "--access", access_override,
                "--partition-value", watched_key,
                "--membership-layer", args.membership_layer,
                "--membership-key", watched_key,
                "--batch-prefix", "fold",
            ],
            capture_output=True, text=True,
        )
        report["ingest"] = json.loads(ingest.stdout) if ingest.stdout.strip().startswith("{") else {
            "stdout": ingest.stdout, "stderr": ingest.stderr[-2000:], "returncode": ingest.returncode
        }
        requests_flush = __import__("requests")
        requests_flush.post(
            f"{server.control_base}/control/flush",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=600,
        )
        # The flush is asynchronous (`/control/flush` answers 202), so the freshness read waits for
        # the flush **counter** to move rather than for a sleep. `buffered_items` is deliberately
        # not part of the condition: sessions are panning throughout and nothing stops more rows
        # arriving, so a drained buffer is not a state this run reaches.
        deadline = time.monotonic() + 600
        flushes_before = status0["write_executor"]["flush"]["flushes"]
        while time.monotonic() < deadline:
            st = server.status()
            if st["write_executor"]["flush"]["flushes"] > flushes_before:
                break
            time.sleep(0.25)
        report["after_flush"] = {
            "at": time.monotonic() - t0,
            "attribute": counts_for(server, observer, args.layer).get(watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer).get(watched_key),
        }

        fold_asked = time.monotonic() - t0
        requests_flush.post(
            f"{server.control_base}/control/compact",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=60,
        )
        fold_done = None
        deadline = time.monotonic() + max(60.0, args.seconds)
        while time.monotonic() < deadline:
            st = server.status()
            rss.append((time.monotonic() - t0, server.rss_bytes()))
            if st["compaction"]["folds"] > folds_before:
                fold_done = time.monotonic() - t0
                break
            time.sleep(0.5)
        report["fold"] = {
            "asked_at": fold_asked,
            "completed_at": fold_done,
            "duration_seconds": None if fold_done is None else round(fold_done - fold_asked, 2),
            "completed": fold_done is not None,
        }

        # ---- after window -------------------------------------------------------------------
        after_started = time.monotonic() - t0
        while time.monotonic() - t0 < after_started + args.settle:
            time.sleep(0.5)
            rss.append((time.monotonic() - t0, server.rss_bytes()))
        stop.set()
        for t in threads:
            t.join(timeout=300)

        report["after_fold"] = {
            "attribute": counts_for(server, observer, args.layer).get(watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer).get(watched_key),
        }
        report["windows"] = {
            "before": window(samples, 0.0, ingest_started),
            "ingest_and_flush": window(samples, ingest_started, fold_asked),
            "during_fold": window(samples, fold_asked, fold_done if fold_done else after_started),
            "after": window(samples, after_started, 1e9),
        }
        report["errors"] = len(errors)
        report["error_examples"] = errors[:5]
        report["peak_rss_bytes"] = max(r for _t, r in rss) if rss else None
        report["status_after"] = server.status()
    finally:
        server.stop()

    out = args.out or (work / "fold-under-load.json")
    out.write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items() if k != "status_after"}, indent=2))
    print(f"-> {out}")


if __name__ == "__main__":
    main()
