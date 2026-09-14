"""The identity-bands probe's results as Markdown.

Reads one `results.json` from `identity_bands_probe` and prints, in order: the builder's file
sizes against their models; then per principal, its occupancy ladder and the depth the mark
budget chose; then per case the reference and band arms' wall, CPU, major faults and bytes read
under each condition, what the band route read, whether the two arms agreed on every tile, the
quantised counts against the exact one, and the render arm's two routes.

Every figure printed here is measured except the ones marked modelled: the page counts and the
bytes derived from them, and the pixel bound on the cell-resolution render's error.

  report.py <results.json>
"""

import json
import sys


def ms(value):
    return f"{value * 1000:.2f}"


def mb(value):
    return f"{value / 1e6:.1f}"


def ratio(a, b):
    return f"{a / b:.3f}" if b else "—"


def table(head, rows):
    print("| " + " | ".join(head) + " |")
    print("|" + "|".join("---" for _ in head) + "|")
    for row in rows:
        print("| " + " | ".join(str(cell) for cell in row) + " |")
    print()


def main(path):
    d = json.load(open(path))
    conditions = d["conditions"]

    print(f"# Identity bands over `{d['bundle']}`")
    print()
    print(
        f"{d['row_count']:,} rows in one segment, {d['cell_count']:,} occupied leaf cells. "
        f"Mark budget {d['budget']:,}, small-`k` cases at k = {d['k_small']}. "
        f"Commit `{d['commit'] or 'unknown'}`, box `{d['box'] or 'unknown'}`."
    )
    print()
    config = d["engine_config"]
    print(
        "Engine: "
        + ", ".join(f"`{k}` {v}" for k, v in config.items())
        + f". View `{d['view']}`."
    )
    print()
    verdict = d["disagreeing_tiles"]
    print(
        f"**Equality: {'every tile agreed' if verdict == 0 else f'{verdict} tile(s) disagreed'}** "
        "between the band route and the shipped selection."
    )
    print()

    print("## The builder's files, measured against their models")
    print()
    bands = d["bands_json"]
    table(
        ["file", "measured B", "model B", "measured/model", "what the model prices"],
        [
            [
                f"`{f['file']}`",
                f"{f['bytes']:,}",
                f"{f['model_bytes']:,}",
                f"{f['bytes_over_model']:.4f}",
                f["model_note"],
            ]
            for f in bands["files"]
        ],
    )
    print(
        f"One pass, {bands['wall_s']:.2f} s wall, {mb(bands['read_bytes'])} MB read "
        f"(`/proc/self/io`), over {bands['row_count']:,} rows and {bands['cell_count']:,} cells."
    )
    print()
    histogram = bands["lz_histogram"]
    top = [(i, n) for i, n in enumerate(histogram) if n][:14]
    table(
        ["leading zeros"] + [str(i) for i, _ in top],
        [["rows"] + [f"{n:,}" for _, n in top]],
    )

    print("## The zoom-`z` locations")
    print()
    print(
        f"Chosen once, under the widest principal `{d['widest_principal']}`, so every principal "
        "is measured at the same places."
    )
    print()
    table(
        ["z", "tile prefix", "visible under the widest principal"],
        [[loc["z"], loc["tile"], f"{loc['visible_widest']:,}"] for loc in d["locations"]],
    )

    for p in d["principals"]:
        print(f"## `{p['principal']}` — {len(p['terms'])} term(s)")
        print()
        print(
            f"{p['visible_total']:,} visible rows. `d*` = {p['d_star']} "
            f"(the depth whose `16 · N_occ(d)` is nearest the budget). "
            f"Projection build {ms(p['projection_build']['wall_s'])} ms wall, "
            f"{ms(p['projection_build']['cpu_s'])} ms CPU, built="
            f"{p['projection_build']['row_projection_built']}. "
            f"Occupancy ladder {ms(p['occupancy_cost']['wall_s'])} ms wall."
        )
        print()
        table(
            ["depth"] + [str(i) for i in range(len(p["occupancy_ladder"]))],
            [["N_occ"] + [f"{n:,}" for n in p["occupancy_ladder"]]],
        )

        cases = [c for c in p["cases"] if "refused" not in c]
        for c in p["cases"]:
            if "refused" in c:
                print(f"`{c['case']}` refused: {c['refused']} ({c['tiles']:,} tiles).")
                print()

        print("### Cost")
        print()
        rows = []
        for c in cases:
            for cond in conditions:
                r = c["arms"].get(f"R.{cond}")
                b = c["arms"].get(f"B.{cond}")
                if not r or not b:
                    continue
                rows.append(
                    [
                        f"`{c['case']}`" if cond == conditions[0] else "",
                        c["zoom"] if cond == conditions[0] else "",
                        cond,
                        ms(r["wall_s"]),
                        ms(r["cpu_s"]),
                        r["majflt"],
                        mb(r["read_bytes"]),
                        ms(b["wall_s"]),
                        ms(b["cpu_s"]),
                        b["majflt"],
                        mb(b["read_bytes"]),
                    ]
                )
        table(
            [
                "case",
                "zoom",
                "cond",
                "R wall ms",
                "R cpu ms",
                "R majflt",
                "R read MB",
                "B wall ms",
                "B cpu ms",
                "B majflt",
                "B read MB",
            ],
            rows,
        )
        print(
            "R runs on the engine's pool and includes the gather; B is one thread and answers the "
            "selection alone. CPU is the comparable column."
        )
        print()

        print("### What the band route read, and whether it agreed")
        print()
        last = conditions[-1]
        rows = []
        for c in cases:
            b = c["arms"].get(f"B.{last}")
            r = c["arms"].get(f"R.{last}")
            if not b:
                continue
            threshold = c["threshold"]
            rows.append(
                [
                    f"`{c['case']}`",
                    "saturated" if threshold == "saturated" else threshold["j"],
                    f"{b['tiles']:,}",
                    f"{b['band_tiles']:,}",
                    f"{b['s_total']:,}",
                    f"{b['column_reads']:,}",
                    mb(b["list_bytes"]),
                    mb(b["lz_bytes"]),
                    f"{b['cut_lookups']:,}",
                    f"{b['floor_widened_tiles']:,}",
                    f"{b['fallback_scan_tiles']:,}",
                    f"{r['points']:,}" if r else "—",
                    f"{b['served_total']:,}",
                    "agree" if b["disagreeing_tiles"] == 0 else f"{b['disagreeing_tiles']} DIFFER",
                ]
            )
        table(
            [
                "case",
                "j",
                "tiles",
                "banded",
                "Σ\\|S\\|",
                "column reads",
                "list MB",
                "lz MB",
                "cut lookups",
                "floor widened",
                "fallback scan",
                "R served",
                "B served",
                "verdict",
            ],
            rows,
        )

        print("### The quantised counts against the exact one, over the banded tiles")
        print()
        rows = []
        for c in cases:
            b = c["arms"].get(f"B.{last}")
            if not b or not b["band_tiles"]:
                continue
            exact = b["exact_banded"]
            rows.append(
                [
                    f"`{c['case']}`",
                    f"{exact:,}",
                    f"{b['s_total']:,}",
                    ratio(b["s_total"], exact),
                    f"{b['band_above_total']:,}",
                    ratio(b["band_above_total"], exact),
                    f"{b['fp16_total']:,}",
                    ratio(b["fp16_total"], exact),
                    f"{b['fp16_tie_reads']:,}",
                ]
            )
        if rows:
            table(
                [
                    "case",
                    "exact ΣC",
                    "band j Σ\\|S\\|",
                    "÷ exact",
                    "band j+1",
                    "÷ exact",
                    "fp16",
                    "÷ exact",
                    "fp16 tie reads",
                ],
                rows,
            )
        else:
            print("No case was settled by a band; every tile fell back to the scan.")
            print()

        print("### The render: the two columns against the cut index")
        print()
        rows = []
        for c in cases:
            for cond in conditions:
                g = c["arms"].get(f"G.{cond}")
                if not g:
                    continue
                cols, cells = g["columns"], g["cells"]
                rows.append(
                    [
                        f"`{c['case']}`" if cond == conditions[0] else "",
                        f"{g['rows']:,}" if cond == conditions[0] else "",
                        cond,
                        ms(cols["wall_s"]),
                        cols["majflt"],
                        f"{cols['morton_pages']:,}",
                        f"{cols['residual_pages']:,}",
                        mb(cols["modelled_bytes"]),
                        ms(cells["wall_s"]),
                        cells["majflt"],
                        f"{cells['cell_code_pages']:,}",
                        f"{cells['cut_index_pages']:,}",
                        mb(cells["modelled_bytes"]),
                        f"{c['pixels_per_cell']:.3f}",
                    ]
                )
        table(
            [
                "case",
                "rows",
                "cond",
                "(i) wall ms",
                "(i) majflt",
                "morton pages",
                "residual pages",
                "(i) modelled MB",
                "(ii) wall ms",
                "(ii) majflt",
                "cell-code pages",
                "cut-index pages",
                "(ii) modelled MB",
                "px/cell",
            ],
            rows,
        )
        print(
            "(i) is `morton.u32[row]` and `residual[row]` for every served row, scattered. "
            "(ii) is one `cuts.u32` binary search and one `cell-codes.u32` read for the same rows. "
            "The page counts and the bytes from them are **modelled** — distinct 4 KiB pages the "
            "row indices touch, times 4096 — not what the kernel moved. (ii)'s position error is "
            "the residual alone: under one leaf cell, 2⁻¹⁶ of the extent per axis, which is the "
            "`px/cell` column in a 2,000-pixel viewport showing that case's bbox."
        )
        print()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
