"""Census exactness: the served artifact counts against the generator's closed form, every artifact.

**This is the campaign's correctness spine.** Everything else here is a number that could be
slower or faster; this is the one that can be wrong. For each of a spread of principals and each of
the layer shapes, the server's answer at the whole map is compared to
`tessera corpus artifact-census` — the same relation stated by a generator that has never read a
Tessera artefact — as sets of `(artifact, masked count)` pairs, with **exact** equality required
in both directions: an artifact the server serves and the census does not is as much a failure as
one the census holds and the server drops.

The four shapes are four different routes through the engine and the comparison is the same for
all of them: `generator/flat` is an enumerated overlapping membership, `generator/partition-
enumerated` its stored twin, `generator/partition-attribute` the same relation read as a predicate
over a value column, `campaign/boundary` a spatial predicate decomposed to Morton ranges, and
`generator/treed` a `nested` lineage whose count includes every descendant's.

Run: `python3 census.py --work DIR --n N [--layers ...] [--grants ...]`
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import campaign as C

#: Served layer name -> the `--layer` value of the census verb that states it.
LAYER_ARMS = {
    C.LAYER_FLAT: "flat",
    C.LAYER_PARTITION_ENUM: "partition",
    C.LAYER_PARTITION_ATTR: "partition",
    C.LAYER_BOUNDARY: "boundary",
    C.LAYER_TREED: "treed",
}

WHOLE_MAP = C.Viewport("100%", [0.0, 0.0, C.GRID, C.GRID], 0, 1.0)


def served_counts(server: C.Server, token: str, layer: str) -> tuple[dict[int, int], float]:
    """`{artifact key as an integer: masked count}` for one layer at the whole map.

    The key is the generator's own: every arm writes `a.to_string()` as the artifact key, and the
    spatial arm writes the tile prefix, which is what the census's `artifact` column carries.
    """
    seconds, _bytes, artifacts, _trailer = C.viewport_request(server, token, WHOLE_MAP, [layer])
    out: dict[int, int] = {}
    for a in artifacts:
        if a.layer != layer:
            raise RuntimeError(f"asked for {layer!r}, served {a.layer!r}")
        if a.key is None:
            raise RuntimeError(f"{layer}: an artifact came back with no key; the census is keyed")
        out[int(a.key)] = a.masked_count
    return out, seconds


def compare(served: dict[int, int], expected: dict[int, int]) -> dict:
    """The two directions, each named, with a minimal reproduction attached to whichever fails."""
    served_only = sorted(set(served) - set(expected))
    census_only = sorted(set(expected) - set(served))
    disagreed = sorted(a for a in set(served) & set(expected) if served[a] != expected[a])
    verdict = {
        "served": len(served),
        "census": len(expected),
        "served_not_in_census": len(served_only),
        "census_not_served": len(census_only),
        "count_disagreements": len(disagreed),
        "exact": not (served_only or census_only or disagreed),
    }
    if served_only:
        verdict["served_not_in_census_examples"] = [
            {"artifact": a, "served_count": served[a]} for a in served_only[:5]
        ]
    if census_only:
        verdict["census_not_served_examples"] = [
            {"artifact": a, "census_count": expected[a]} for a in census_only[:5]
        ]
    if disagreed:
        verdict["count_disagreement_examples"] = [
            {"artifact": a, "served": served[a], "census": expected[a]} for a in disagreed[:5]
        ]
    return verdict


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--n", required=True, type=int)
    ap.add_argument("--seed", type=int, default=20_260_822)
    ap.add_argument("--terms-per-level", type=int, default=65_536)
    ap.add_argument("--layers", nargs="*", default=list(LAYER_ARMS))
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
        for name, spec in grants.items():
            terms = spec["grant"].split(",")
            token, auth_seconds = server.authorise(terms)
            print(f"principal {name}: {len(terms)} term(s), authorised in {auth_seconds:.2f}s")
            for layer in args.layers:
                arm = LAYER_ARMS[layer]
                started = time.monotonic()
                expected = C.artifact_census(
                    args.seed, args.n, arm, spec["grant"], args.terms_per_level, work / "scratch"
                )
                census_seconds = time.monotonic() - started
                served, request_seconds = served_counts(server, token, layer)
                verdict = compare(served, expected)
                verdict.update(
                    principal=name,
                    principal_terms=len(terms),
                    principal_visible=spec.get("visible"),
                    layer=layer,
                    arm=arm,
                    authorise_seconds=round(auth_seconds, 3),
                    census_seconds=round(census_seconds, 3),
                    request_seconds=round(request_seconds, 4),
                )
                results.append(verdict)
                mark = "exact" if verdict["exact"] else "MISMATCH"
                print(
                    f"  {layer:38s} {mark}: served {verdict['served']}, census {verdict['census']}"
                    f" ({request_seconds * 1000:.1f} ms)"
                )
                if not verdict["exact"]:
                    print(json.dumps(verdict, indent=2))
    finally:
        server.stop()

    out = args.out or (work / "census-report.json")
    out.write_text(json.dumps(results, indent=2))
    bad = [r for r in results if not r["exact"]]
    print(f"\n{len(results) - len(bad)} of {len(results)} cells exact")
    raise SystemExit(1 if bad else 0)


if __name__ == "__main__":
    main()
