"""Roll the per-artifact survey up into the tables the design cites.

    python3 report.py hdbscan
"""

import json
import os
import sys

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FAMILIES = ["convex", "dig", "alpha_complex", "chi", "chi_dp"]
NAMES = {
    "convex": "convex wrap",
    "dig": "alpha shape (as built)",
    "alpha_complex": "alpha-complex",
    "chi": "chi-shape",
    "chi_dp": "chi-shape, Douglas-Peucker at alpha/2",
}


def load(layer):
    with open(os.path.join(HERE, f"results-{layer}.json")) as fh:
        return json.load(fh)


def q(xs, p):
    return float(np.quantile(np.asarray(xs, dtype=float), p))


def main(layer="hdbscan"):
    rows = load(layer)
    n = len(rows)
    members = np.array([r["members"] for r in rows])
    print(f"# {layer}: {n} artifacts, {members.min()} … {members.max()} members\n")

    print("## Families, whole layer\n")
    print("| family | rings (median / max) | vertices | wire bytes | area / wrap (median) | fill (median) | members outside | precision (median) |")
    print("|---|---|---|---|---|---|---|---|")
    for f in FAMILIES:
        d = [r[f] for r in rows]
        print(
            f"| {NAMES[f]} "
            f"| {int(np.median([x['rings'] for x in d]))} / {max(x['rings'] for x in d)} "
            f"| {sum(x['vertices'] for x in d):,} "
            f"| {sum(x['bytes'] for x in d):,} "
            f"| {q([x['area_ratio'] for x in d], 0.5):.3f} "
            f"| {q([x['fill'] for x in d], 0.5):.3f} "
            f"| {sum(x['members_outside'] for x in d):,} "
            f"| {q([x['precision'] for x in d], 0.5):.3f} |"
        )

    print("\n## Fill, by family — the share of the drawn shape within alpha of a member\n")
    print("| family | min | p25 | median | p75 | artifacts under 0.5 |")
    print("|---|---|---|---|---|---|")
    for f in FAMILIES:
        v = [r[f]["fill"] for r in rows]
        print(
            f"| {NAMES[f]} | {min(v):.3f} | {q(v, 0.25):.3f} | {q(v, 0.5):.3f} | {q(v, 0.75):.3f} "
            f"| {sum(1 for x in v if x < 0.5)} / {n} |"
        )

    print("\n## Multi-modality at the same alpha\n")
    mo5 = np.array([r["modality"]["over_5pct"] for r in rows])
    mo10 = np.array([r["modality"]["over_10pct"] for r in rows])
    mo1 = np.array([r["modality"]["over_1pct"] for r in rows])
    largest = np.array([r["modality"]["largest_share"] for r in rows])
    print(f"- artifacts with 2+ components holding >= 5% of members:  **{int((mo5 >= 2).sum())} / {n}**")
    print(f"- artifacts with 2+ components holding >= 10% of members: **{int((mo10 >= 2).sum())} / {n}**")
    print(f"- artifacts with 2+ components holding >= 1% of members:  **{int((mo1 >= 2).sum())} / {n}**")
    print(f"- artifacts with 3+ components holding >= 5% of members:  **{int((mo5 >= 3).sum())} / {n}**")
    print(f"- largest component's share of members: median {np.median(largest):.3f}, min {largest.min():.3f}")
    print(
        f"- members outside their artifact's largest component: "
        f"{int(sum(r['members'] * (1 - r['modality']['largest_share']) for r in rows)):,} "
        f"of {int(members.sum()):,} member rows "
        f"({100 * sum(r['members'] * (1 - r['modality']['largest_share']) for r in rows) / members.sum():.1f}%)"
    )

    multi = [r for r in rows if r["modality"]["over_5pct"] >= 2]
    if multi:
        print("\n### The multi-modal artifacts, and what each family does with them\n")
        print("| artifact | members | components >=5% | largest share | dig fill | alpha-complex rings | chi fill |")
        print("|---|---|---|---|---|---|---|")
        for r in sorted(multi, key=lambda r: -r["members"])[:20]:
            print(
                f"| {r['key']} | {r['members']:,} | {r['modality']['over_5pct']} "
                f"| {r['modality']['largest_share']:.2f} | {r['dig']['fill']:.3f} "
                f"| {r['alpha_complex']['rings']} | {r['chi']['fill']:.3f} |"
            )

    print("\n## The dig's own limits\n")
    dig = [r["dig"] for r in rows]
    print(f"- at the 64-vertex budget: **{sum(d['at_budget'] for d in dig)} / {n}**")
    print(
        f"- finishing with a live edge still above alpha (the budget, not alpha, stopped it): "
        f"**{sum(1 for d in dig if d['bridges_left'] > 0)} / {n}**"
    )
    print(f"- digs refused for want of a candidate or for simplicity, whole layer: **{sum(d['refused_digs'] for d in dig)}**")
    big = [r for r in rows if r["members"] >= 200000]
    if big:
        print(
            f"- artifacts of >= 200,000 members: {len(big)}, median fill "
            f"{q([r['dig']['fill'] for r in big], 0.5):.3f}, median area/wrap "
            f"{q([r['dig']['area_ratio'] for r in big], 0.5):.3f}"
        )

    print("\n## By membership size\n")
    bands = [(0, 10000), (10000, 50000), (50000, 200000), (200000, 10**9)]
    print("| members | artifacts | " + " | ".join(f"{NAMES[f]} fill / vertices" for f in FAMILIES) + " |")
    print("|---" * (2 + len(FAMILIES)) + "|")
    for lo, hi in bands:
        band = [r for r in rows if lo <= r["members"] < hi]
        if not band:
            continue
        cells = [
            f"{q([r[f]['fill'] for r in band], 0.5):.3f} / {int(np.median([r[f]['vertices'] for r in band]))}"
            for f in FAMILIES
        ]
        label = f"{lo:,} – {hi:,}" if hi < 10**9 else f"{lo:,}+"
        print(f"| {label} | {len(band)} | " + " | ".join(cells) + " |")

    print("\n## Precision: does a shape swallow other artifacts' points?\n")
    for f in FAMILIES:
        v = [r[f]["precision"] for r in rows]
        print(f"- {NAMES[f]}: median {q(v, 0.5):.3f}, p10 {q(v, 0.10):.3f}, min {min(v):.3f}, artifacts under 0.9: {sum(1 for x in v if x < 0.9)} / {n}")

    print("\n## Alpha-complex: what it costs to be honest\n")
    ac = [r["alpha_complex"] for r in rows]
    print(f"- members left outside the shape, whole layer: **{sum(x['members_outside'] for x in ac):,}** of {int(members.sum()):,}")
    print(f"- artifacts leaving at least one member outside: {sum(1 for x in ac if x['members_outside'] > 0)} / {n}")
    print(f"- rings: median {int(np.median([x['rings'] for x in ac]))}, max {max(x['rings'] for x in ac)}")
    print(f"- holes: total {sum(x['holes'] for x in ac)}, artifacts with a hole {sum(1 for x in ac if x['holes'] > 0)} / {n}")
    print(f"- wire bytes, whole layer: {sum(x['bytes'] for x in ac):,} against the dig's {sum(r['dig']['bytes'] for r in rows):,}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "hdbscan")
