"""Serving during a fold — the named gap. The fold's artifact pass has only ever run on an idle box.

Thirty-two mixed sessions pan continuously; ingest lands rows so the fold has something to execute;
`POST /control/compact` schedules it — the nightly window is `off` in the campaign's deployment, so
the fold is asked for on demand rather than waited for, and `request_fold` is the same call the
window's tick makes. That is what makes this a measurement of the fold rather than of the clock.

Four things are recorded, and the plan names the first three:

- **request latency through the fold** — p50/p99 in four windows (before, ingest and flush, during
  the fold, after) taken from the load arm's own per-request record, from sessions that were never
  re-established, so a degradation is the fold's and not a cold cache's;
- **the fold's own duration under load** — both the wall time from the `POST` to the
  `compaction.folds` counter moving, and the fold's own `elapsed_ms` from the server log, against
  an unloaded run of the same fold at the same tier (`--unloaded`);
- **freshness across it** — an artifact's masked count before the ingest, after its flush and after
  the fold, for a predicate layer and an enumerated one, which answer differently and both
  correctly;
- and **the shed count**, because a fold that keeps every request is a different result from one
  that keeps the median and drops the tail.

**The load is the Rust arm**, not this file's threads:
`probes/.../concurrency.py`'s docstring records why — a Python driver holding the GIL was the
first sweep's answer rather than the engine's.

Run: `python3 fold_under_load.py --work DIR --n N --sessions 32`
"""

from __future__ import annotations

import argparse
import csv
import json
import re
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


def fold_elapsed_from_log(log: Path, since: int) -> list[dict]:
    """The fold's own timings, parsed out of the server log it wrote them to.

    The wall time from the `POST` to the counter moving includes whatever queueing the request met;
    `elapsed_ms` on the publish line is the fold itself, and the artifact pass logs its own beside
    it. Both belong in the record — the first is what a live session experiences, the second is
    what the fold cost.
    """
    out = []
    ansi = re.compile(r"\x1b\[[0-9;]*m")
    for raw in log.read_text(errors="replace").splitlines()[since:]:
        line = ansi.sub("", raw)
        if "a compaction fold published" in line or "the fold's artifact pass rebuilt" in line:
            fields = {}
            for token in line.split():
                if "=" in token:
                    k, _, v = token.partition("=")
                    fields[k.strip()] = v.strip()
            out.append({
                "kind": "fold" if "published" in line else "artifact_pass",
                "elapsed_ms": fields.get("elapsed_ms"),
                "fold_secs": fields.get("fold_secs"),
                "staircase_rss": fields.get("staircase_rss"),
                "projections": fields.get("projections"),
            })
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--n", required=True, type=int)
    ap.add_argument("--seed", type=int, default=20_260_822)
    ap.add_argument("--terms-per-level", type=int, default=65_536)
    ap.add_argument("--sessions", type=int, default=32)
    ap.add_argument("--settle", type=float, default=30.0, help="the before-window's length")
    ap.add_argument("--after", type=float, default=30.0, help="the after-window's length")
    ap.add_argument("--layer", default=C.LAYER_PARTITION_ATTR)
    ap.add_argument("--membership-layer", default=C.LAYER_PARTITION_ENUM)
    ap.add_argument("--ingest-rows", type=int, default=500_000)
    ap.add_argument("--load-seconds", type=float, default=3600.0,
                    help="an upper bound on the load arm's life; it is stopped with SIGTERM the "
                         "moment the after-window closes, and writes its record on the way out")
    ap.add_argument("--flush-budget", type=float, default=300.0,
                    help="how long to wait for the flush's rows to reach the served count")
    ap.add_argument("--unloaded", action="store_true",
                    help="the comparison run: the same ingest and fold with no sessions at all")
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}
    # The freshness observer is the broadest principal, and the ingested rows carry its own access
    # label — a check whose new members are invisible to the observer passes vacuously.
    observer_terms = grants["0.9375"]["grant"].split(",")
    access_override = ",".join(observer_terms[:4])
    # The artifact whose count is watched: key "1", the partition arm's second artifact, which
    # exists in every roster at every tier and is therefore not a size-dependent choice.
    watched_key = "1"

    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)
    server = C.Server(work, *ports)
    server.spawn()
    report: dict = {
        "sessions": 0 if args.unloaded else args.sessions,
        "unloaded": args.unloaded,
        "layer": args.layer,
        "n": args.n,
    }
    load = None
    samples_path = work / "fold-samples.csv"
    try:
        log_lines_before = len((server.log).read_text(errors="replace").splitlines())
        observer, _ = server.authorise(observer_terms)
        report["before"] = {
            "attribute": counts_for(server, observer, args.layer, watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }
        status0 = server.status()
        folds_before = status0["compaction"]["folds"]
        shed_before = status0["compute"]["shed_total"]

        t0 = time.monotonic()
        if not args.unloaded:
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
                "--seconds", str(args.load_seconds),
                "--server-pid", str(server.proc.pid),
                "--samples-out", str(samples_path),
                "--out", str(work / "fold-load.json"),
            ]
            for breadth, count in counts.items():
                argv += ["--mix", f"{breadth}={count}"]
            report["mix"] = counts
            load = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            # The load arm authorises every session before it starts timing; give it that much
            # before the before-window's clock is taken to have started.
            time.sleep(3.0)
            t0 = time.monotonic()

        while time.monotonic() - t0 < args.settle:
            time.sleep(0.5)

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

        # **The wait is on the answer, not on a gauge.** An earlier revision waited for the flush
        # counter to move and `buffered_items` to reach zero; under a live serving load the second
        # never came, and the run spent fifteen minutes in a wait whose deadline it then hit. What
        # the claim under test actually says is that the count moves at its flush, so that is what
        # is polled — and **how long it took** becomes a figure rather than a slept-through
        # assumption.
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
            "at": round(time.monotonic() - t0, 2),
            "seconds_after_flush_request": round(time.monotonic() - flush_asked, 2),
            "moved": attribute_now != report["before"]["attribute"],
            "attribute": attribute_now,
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }

        fold_asked = time.monotonic() - t0
        requests.post(
            f"{server.control_base}/control/compact",
            headers={"Authorization": f"Bearer {C.OPERATOR_CREDENTIAL}"}, timeout=60,
        )
        fold_done = None
        deadline = time.monotonic() + 3600
        while time.monotonic() < deadline:
            if server.status()["compaction"]["folds"] > folds_before:
                fold_done = time.monotonic() - t0
                break
            time.sleep(0.5)
        report["fold"] = {
            "asked_at": round(fold_asked, 2),
            "completed_at": None if fold_done is None else round(fold_done, 2),
            "wall_seconds": None if fold_done is None else round(fold_done - fold_asked, 2),
            "completed": fold_done is not None,
        }

        after_started = time.monotonic() - t0
        while time.monotonic() - t0 < after_started + args.after:
            time.sleep(0.5)

        report["after_fold"] = {
            "attribute": counts_for(server, observer, args.layer, watched_key),
            "enumerated": counts_for(server, observer, args.membership_layer, watched_key),
        }
        status_end = server.status()
        report["shed_during_run"] = status_end["compute"]["shed_total"] - shed_before
        report["peak_rss_bytes"] = server.rss_bytes()
    finally:
        if load is not None:
            load.terminate()   # a clean stop: the arm writes its per-request record on the way out
            try:
                load.wait(timeout=300)
            except subprocess.TimeoutExpired:
                load.kill()
        server.stop()

    report["fold_log"] = fold_elapsed_from_log(work / "server.log", log_lines_before)

    if samples_path.exists() and not args.unloaded:
        with samples_path.open() as handle:
            samples = [(float(r["start_s"]), float(r["latency_s"])) for r in csv.DictReader(handle)]
        # The load arm's clock starts ~3 s before `t0`; both are monotonic within this process's
        # lifetime and the offset is the sleep above, so the windows are stated in the load arm's
        # own frame with that offset applied.
        offset = 3.0
        report["windows"] = {
            "before": window(samples, offset, offset + ingest_started),
            "ingest_and_flush": window(samples, offset + ingest_started, offset + fold_asked),
            "during_fold": window(samples, offset + fold_asked,
                                  offset + (fold_done if fold_done else after_started)),
            "after": window(samples, offset + after_started, 1e9),
        }
        report["load_requests"] = len(samples)

    out = args.out or (work / ("fold-unloaded.json" if args.unloaded else "fold-under-load.json"))
    out.write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))
    print(f"-> {out}")


if __name__ == "__main__":
    main()
