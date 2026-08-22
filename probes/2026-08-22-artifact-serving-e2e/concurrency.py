"""The concurrency sweep: 1 / 8 / 32 / 128 sessions at mixed breadths, panning.

**The envelope is the deliverable, not a pass/fail count** (the campaign's plan, §8). What this
records per concurrency is where latency starts to degrade and what binds first — the compute
gate's queue, the server's resident set, or the caches. Every session is its own principal with its
own `M_auth`: masks are not shared, which is the scenario the owner set, and it is what makes 128
sessions 128 bitmaps rather than one.

**Broad principals are capped at 8 concurrent**, the row-projection cache's sizing. Above that the
mix is median and narrow, which is also the realistic shape: a deployment has a few analysts who
can see most of the corpus and many who cannot.

Panning is a walk of the viewport ladder rather than one repeated request, so the caches are
exercised the way a moving map exercises them instead of being answered from one warm entry.

Run: `python3 concurrency.py --work DIR --levels 1 8 32 128 --seconds 30`
"""

from __future__ import annotations

import argparse
import json
import random
import statistics
import threading
import time
from pathlib import Path

import campaign as C

from grid import percentile

#: The panning route: a ring of viewports at a mix of zooms, offset per session so two sessions are
#: never asking the identical question at the identical moment.
def pan_route(rng: random.Random) -> list[C.Viewport]:
    ladder = C.viewports()
    route = []
    for _ in range(8):
        vp = rng.choice(ladder)
        # Jitter the box within the grid so the walk is a pan rather than a zoom in place.
        side = C.GRID * (vp.fraction ** 0.5)
        x = rng.uniform(0.0, max(0.0, C.GRID - side))
        y = rng.uniform(0.0, max(0.0, C.GRID - side))
        route.append(C.Viewport(vp.name, [x, y, x + side, y + side], vp.zoom, vp.fraction))
    return route


def mix_for(level: int, broad_cap: int) -> list[str]:
    """The breadth mix at one concurrency: broad up to the cap, then median, then narrow.

    At 32 that is 2 broad / 6 median / 24 narrow — the plan's own example — and the same rule
    scales it to 1, 8 and 128.
    """
    broad = min(broad_cap, max(1, level // 16))
    median = min(level - broad, max(1, level // 5))
    narrow = level - broad - median
    return ["0.9375"] * broad + ["0.25"] * median + ["0.031"] * max(0, narrow)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--levels", nargs="*", type=int, default=[1, 8, 32, 128])
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--layer", default=C.LAYER_PARTITION_ATTR)
    ap.add_argument("--broad-cap", type=int, default=8)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}

    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports)
    server = C.Server(work, *ports)
    server.spawn()
    results = []
    try:
        for level in args.levels:
            mix = mix_for(level, args.broad_cap)
            tokens = []
            auth_seconds = []
            for i, breadth in enumerate(mix):
                token, seconds = server.authorise(grants[breadth]["grant"].split(","))
                tokens.append((breadth, token))
                auth_seconds.append(seconds)
            print(f"concurrency {level}: {len(mix)} session(s), mix "
                  f"{ {b: mix.count(b) for b in set(mix)} }, authorised in "
                  f"{sum(auth_seconds):.1f}s total")

            latencies: dict[str, list[float]] = {b: [] for b in set(mix)}
            errors: list[str] = []
            lock = threading.Lock()
            stop = threading.Event()
            peak_rss = [server.rss_bytes()]

            def session(index: int, breadth: str, token: str) -> None:
                rng = random.Random(0xC0FFEE + index)
                route = pan_route(rng)
                local: list[float] = []
                step = 0
                while not stop.is_set():
                    vp = route[step % len(route)]
                    step += 1
                    try:
                        seconds, _b, _a, _t = C.viewport_request(server, token, vp, [args.layer])
                        local.append(seconds)
                    except Exception as exc:  # a shed or a timeout is data, not a crash
                        with lock:
                            errors.append(f"{type(exc).__name__}: {exc}"[:200])
                        time.sleep(0.05)
                with lock:
                    latencies[breadth].extend(local)

            threads = [
                threading.Thread(target=session, args=(i, breadth, token), daemon=True)
                for i, (breadth, token) in enumerate(tokens)
            ]
            started = time.monotonic()
            for t in threads:
                t.start()
            while time.monotonic() - started < args.seconds:
                time.sleep(0.5)
                peak_rss.append(server.rss_bytes())
            stop.set()
            for t in threads:
                t.join(timeout=300)
            elapsed = time.monotonic() - started
            status = server.status()

            everything = [s for v in latencies.values() for s in v]
            row = {
                "concurrency": level,
                "mix": {b: mix.count(b) for b in sorted(set(mix))},
                "layer": args.layer,
                "seconds": round(elapsed, 2),
                "requests": len(everything),
                "throughput_rps": round(len(everything) / elapsed, 2) if elapsed else 0.0,
                "p50_ms": round(statistics.median(everything) * 1000, 2) if everything else None,
                "p99_ms": round(percentile(everything, 0.99) * 1000, 2) if everything else None,
                "max_ms": round(max(everything) * 1000, 2) if everything else None,
                "per_breadth": {
                    b: {
                        "requests": len(v),
                        "p50_ms": round(statistics.median(v) * 1000, 2) if v else None,
                        "p99_ms": round(percentile(v, 0.99) * 1000, 2) if v else None,
                    }
                    for b, v in sorted(latencies.items())
                },
                "errors": len(errors),
                "error_examples": errors[:5],
                "peak_rss_bytes": max(peak_rss),
                "authorise_total_seconds": round(sum(auth_seconds), 2),
                "compute_gate": status.get("compute"),
                "row_projection_cache": status.get("row_projection_cache"),
                "fragment_cache": status.get("fragment_cache"),
                "sessions": status.get("sessions"),
            }
            results.append(row)
            print(f"  {row['requests']} req in {row['seconds']}s — {row['throughput_rps']} rps, "
                  f"p50 {row['p50_ms']} ms, p99 {row['p99_ms']} ms, "
                  f"RSS {row['peak_rss_bytes'] / 1024**2:.0f} MB, {row['errors']} error(s)")
    finally:
        server.stop()

    out = args.out or (work / "concurrency.json")
    out.write_text(json.dumps(results, indent=2))
    print(f"\n-> {out}")


if __name__ == "__main__":
    main()
