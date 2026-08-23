"""The serving grid, engine-side: layer × principal breadth × viewport, through the real request path.

`artifact-serving-at-scale.md` §7 is a probe-side grid — a bench binary owning its own control flow
over the engine's structures. This is the same grid asked over HTTP, of a running `tessera serve`,
with sessions established the way a client establishes them and frames decoded the way a client
decodes them. Where the two align the campaign compares them; where a cell runs more than twice the
probe's figure the collation flags it.

**Cold and warm are reported separately and neither is dropped.** The first request at a
(layer, level, principal) pays for a row form and a lineage the ones after it reuse — 283 ms
against 17 ms at 10⁶ artifacts — so a grid that quoted only the median of a warm run would be
describing a state no first viewport is ever in.

Run: `python3 grid.py --work DIR --iterations 9 [--layers ...] [--out FILE]`
"""

from __future__ import annotations

import argparse
import json
import statistics
import time
from pathlib import Path

import campaign as C

LAYERS = [C.LAYER_FLAT, C.LAYER_PARTITION_ENUM, C.LAYER_PARTITION_ATTR, C.LAYER_BOUNDARY, C.LAYER_TREED]


def percentile(values: list[float], q: float) -> float:
    """The nearest-rank percentile of a small sample — no interpolation, so a p99 over nine
    iterations is honestly the largest of them rather than a number no request took."""
    if not values:
        return float("nan")
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, int(round(q * len(ordered) + 0.5)) - 1))
    return ordered[index]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--iterations", type=int, default=9)
    ap.add_argument("--layers", nargs="*", default=LAYERS)
    ap.add_argument("--principals", nargs="*", default=None)
    ap.add_argument(
        "--viewports", nargs="*", default=None,
        help="restrict the zoom ladder by name — how the enumerated arms are sampled at a tier "
             "where a whole grid of them would cost hours rather than minutes",
    )
    ap.add_argument("--append", type=Path, default=None,
                    help="merge into an existing grid file rather than replacing it")
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--k", type=int, default=0, help="points asked for beside the artifacts")
    ap.add_argument("--stream-deadline-ms", type=int, default=60_000,
                    help="the shipped default; raised only by the design-ceiling probe, which "
                         "cannot measure its cold cell under it")
    args = ap.parse_args()

    work: Path = args.work
    fixture = json.loads((work / "fixture" / "fixture.json").read_text())
    grants = {str(g["target"]): g for g in fixture["grants"]}
    if args.principals:
        grants = {k: v for k, v in grants.items() if k in args.principals}

    ladder = C.viewports()
    if args.viewports:
        ladder = [vp for vp in ladder if vp.name in args.viewports]

    ports = (C.free_port(), C.free_port(), C.free_port())
    C.write_deployment(work, ports, stream_deadline_ms=args.stream_deadline_ms)
    server = C.Server(work, *ports)
    server.spawn()
    rows = []
    try:
        boot_rss = server.rss_bytes()
        for principal, spec in grants.items():
            token, auth_seconds = server.authorise(spec["grant"].split(","))
            for layer in args.layers:
                for vp in ladder:
                    samples = []
                    server_us: list[int] = []
                    stream_us: list[int] = []
                    serialise_ns: list[int] = []
                    cold = None
                    served = 0
                    body = 0
                    truncated = False
                    for i in range(args.iterations + 1):
                        if i == 0:
                            # The cold pass. A truncation here is the README's §8 defect and is
                            # recorded rather than raised — the row form is built either way, so
                            # the warm iterations that follow are the measurement they were going
                            # to be.
                            truncated, cold = C.warm(server, token, vp, [layer])
                            continue
                        seconds, body, artifacts, trailer = C.viewport_request(
                            server, token, vp, [layer], k=args.k
                        )
                        served = len(artifacts)
                        samples.append(seconds)
                        server_us.append(trailer.get("server_us") or 0)
                        stream_us.append(trailer.get("stream_us") or 0)
                        serialise_ns.append(trailer.get("arrow_serialise_ns") or 0)
                    rows.append({
                        "principal": principal,
                        "principal_terms": spec["terms"],
                        "principal_fraction": spec.get("measured_fraction"),
                        "authorise_seconds": round(auth_seconds, 4),
                        "layer": layer,
                        "viewport": vp.name,
                        "viewport_fraction": vp.fraction,
                        "zoom": vp.zoom,
                        "artifacts_served": served,
                        "body_bytes": body,
                        "cold_ms": round(cold * 1000, 3),
                        "cold_truncated": truncated,
                        "p50_ms": round(statistics.median(samples) * 1000, 3),
                        "p99_ms": round(percentile(samples, 0.99) * 1000, 3),
                        "min_ms": round(min(samples) * 1000, 3),
                        "max_ms": round(max(samples) * 1000, 3),
                        "iterations": len(samples),
                        # The server's own decomposition of the figure above.
                        "server_p50_ms": round(statistics.median(server_us) / 1000, 3),
                        "stream_p50_ms": round(statistics.median(stream_us) / 1000, 3),
                        "serialise_p50_ms": round(statistics.median(serialise_ns) / 1e6, 3),
                        "rss_bytes": server.rss_bytes(),
                    })
                    print(
                        f"{layer:38s} {principal:>12s} {vp.name:>8s}  "
                        f"cold {cold * 1000:8.1f}  p50 {rows[-1]['p50_ms']:8.1f}  "
                        f"p99 {rows[-1]['p99_ms']:8.1f}  "
                        f"srv {rows[-1]['server_p50_ms']:8.1f}  "
                        f"enc {rows[-1]['serialise_p50_ms']:6.1f}  n={served}"
                    )
        final_rss = server.rss_bytes()
        status = server.status()
    finally:
        server.stop()

    out = args.out or (work / "grid.json")
    if args.append and args.append.exists():
        # A tier measured in passes — the cheap layers over the whole ladder, the dear ones at a
        # sample of it — is still one grid, so the passes merge into one file rather than becoming
        # two records a reader has to join by hand.
        prior = json.loads(args.append.read_text())
        seen = {(r["layer"], r["principal"], r["viewport"]) for r in rows}
        rows = [r for r in prior["rows"] if (r["layer"], r["principal"], r["viewport"]) not in seen] + rows
    out.write_text(json.dumps({
        "boot_rss_bytes": boot_rss,
        "final_rss_bytes": final_rss,
        "status": status,
        "rows": rows,
    }, indent=2))
    print(f"\n{len(rows)} cells -> {out}; RSS {boot_rss / 1024**2:.0f} MB at boot, "
          f"{final_rss / 1024**2:.0f} MB after")


if __name__ == "__main__":
    main()
