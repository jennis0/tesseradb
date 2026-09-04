#!/usr/bin/env python3
"""Fold `runs/*.jsonl` into `result.json` and print the README's tables.

A consumer of the harness's output, never part of it: every figure here is copied or derived
(fits, ratios) from the JSON the Rust binary wrote, and the derivation is stated beside each.
"""

import glob
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RUNS = os.path.join(HERE, "runs")
RECORDED_10E9_25PC_MS = 1277.3  # probes/2026-08-14-project-decomposition/1e9-scattered.txt


def load():
    rows = []
    for path in sorted(glob.glob(os.path.join(RUNS, "*.jsonl"))):
        if os.path.basename(path).startswith("smoke"):
            continue
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    row = json.loads(line)
                    row["source"] = os.path.basename(path)[: -len(".jsonl")]
                    rows.append(row)
    return rows


def passes(linearity, n, cov, mask):
    """Every process that measured this configuration, least-interfered first.

    Two processes at one configuration differed by half on the shared box; the lower median is
    the one with fewer preemptions in every case seen, so it is the figure used, and the other is
    shown beside it.
    """
    found = [x for x in linearity if x["rows"] == n and x["coverage"] == cov and x["mask"] == mask]
    return sorted(found, key=lambda x: x["median_ms"])


def best(linearity, n, cov, mask):
    found = passes(linearity, n, cov, mask)
    return found[0] if found else None


def fit_line(points):
    """Least-squares `t = a + b * n` over (n, t) pairs; returns (a, b, r2)."""
    n = len(points)
    mx = sum(p[0] for p in points) / n
    my = sum(p[1] for p in points) / n
    sxx = sum((p[0] - mx) ** 2 for p in points)
    sxy = sum((p[0] - mx) * (p[1] - my) for p in points)
    b = sxy / sxx
    a = my - b * mx
    ss_res = sum((p[1] - (a + b * p[0])) ** 2 for p in points)
    ss_tot = sum((p[1] - my) ** 2 for p in points)
    r2 = 1.0 - ss_res / ss_tot if ss_tot else 1.0
    return a, b, r2


def per_decade(n1, t1, n2, t2):
    """How much worse than linear the step n1 -> n2 is, normalised to a factor of ten in rows.

    A perfectly linear step gives 1.0 whatever the size ratio; 1.3 means the per-row cost grew
    30 % over a tenfold increase in rows.
    """
    excess = (t2 / t1) / (n2 / n1)
    return excess ** (1.0 / math.log10(n2 / n1))


def fmt_rows(n):
    return {1_000_000: "10⁶", 10_000_000: "10⁷", 100_000_000: "10⁸",
            400_000_000: "4×10⁸", 1_000_000_000: "10⁹"}.get(n, f"{n:,}")


def main():
    rows = load()
    host = next((r for r in rows if r["part"] == "host"), {})
    linearity = [r for r in rows if r["part"] == "linearity"]
    shards = [r for r in rows if r["part"] == "shards"]
    tokens = [r for r in rows if r["part"] == "tokens"]

    commit = (sys.argv[1] if len(sys.argv) > 1 else
              os.popen("git rev-parse --short HEAD 2>/dev/null").read().strip())
    out = {"commit": commit, "host": host, "recorded_10e9_25pc_ms": RECORDED_10E9_25PC_MS,
           "linearity": linearity, "shards": shards, "tokens": tokens, "fits": []}

    # ---- (a) linearity -------------------------------------------------------------------
    coverages = sorted({r["coverage"] for r in linearity}, reverse=True)
    sizes = sorted({r["rows"] for r in linearity})
    print("## (a) `Permutation::project`, scattered permutation, scattered mask — median of "
          f"{linearity[0]['reps'] if linearity else '?'} after a warm-up\n")
    print("Each size ran in two processes; the lower median is used and the other is shown. "
          "`preempted` is the used process's non-voluntary context switches summed over its "
          "timed calls, `sleeps` its voluntary ones.\n")
    print("| rows | coverage | projected rows | ms used | cpu ms | min ms | other process ms | "
          "preempted / sleeps | major faults | ns per row | ns per projected row |")
    print("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for cov in coverages:
        for n in sizes:
            found = passes(linearity, n, cov, "scattered")
            if not found:
                continue
            r = found[0]
            other = " / ".join(f"{x['median_ms']:.2f}" for x in found[1:]) or "—"
            print(f"| {fmt_rows(n)} | {cov:.0%} | {r['mask_cardinality']:,} | "
                  f"{r['median_ms']:.2f} | {r['median_cpu_ms']:.2f} | {r['min_ms']:.2f} | "
                  f"{other} | {sum(r['nonvoluntary_switches'])} / "
                  f"{sum(r['voluntary_switches'])} | {sum(r['majflt'])} | "
                  f"{r['ns_per_row']:.3f} | {r['ns_per_projected_row']:.2f} |")
    print()

    print("## (a) per-decade nonlinearity and the fit\n")
    print("Per-decade factor: `(t₂/t₁)/(n₂/n₁)` normalised to a tenfold step; 1.0 is linear.\n")
    print("| coverage | 10⁶→10⁷ | 10⁷→10⁸ | 10⁸→4×10⁸ | fit over ≥10⁷: a + b·n (ms) | r² | "
          "predicted at 10⁹ | 4×10⁸ × 2.5 |")
    print("|---:|---:|---:|---:|---|---:|---:|---:|")
    for cov in coverages:
        pts = sorted((n, best(linearity, n, cov, "scattered")["median_ms"]) for n in sizes
                     if best(linearity, n, cov, "scattered"))
        by_n = dict(pts)
        steps = []
        for n1, n2 in [(1_000_000, 10_000_000), (10_000_000, 100_000_000),
                       (100_000_000, 400_000_000)]:
            if n1 in by_n and n2 in by_n:
                steps.append(f"{per_decade(n1, by_n[n1], n2, by_n[n2]):.2f}")
            else:
                steps.append("—")
        big = [p for p in pts if p[0] >= 10_000_000]
        if len(big) >= 2:
            a, b, r2 = fit_line(big)
            pred = a + b * 1e9
            fit_txt = f"{a:.1f} + {b * 1e6:.3f}·(n/10⁶)"
            r2_txt = f"{r2:.4f}"
            pred_txt = f"{pred:,.0f} ms"
        else:
            a = b = r2 = pred = None
            fit_txt = r2_txt = pred_txt = "—"
        scaled = by_n.get(400_000_000)
        scaled_txt = f"{scaled * 2.5:,.0f} ms" if scaled else "—"
        print(f"| {cov:.0%} | {steps[0]} | {steps[1]} | {steps[2]} | {fit_txt} | {r2_txt} | "
              f"{pred_txt} | {scaled_txt} |")
        out["fits"].append({"coverage": cov, "points": pts, "fit_over_rows_at_least": 10_000_000,
                            "intercept_ms": a, "slope_ms_per_row": b, "r2": r2,
                            "predicted_10e9_ms": pred,
                            "scaled_from_4e8_ms": scaled * 2.5 if scaled else None,
                            "per_decade_factors": steps})
    print()
    rec = next((f for f in out["fits"] if abs(f["coverage"] - 0.25) < 1e-9), None)
    if rec and rec["predicted_10e9_ms"]:
        print(f"Recorded at 10⁹ over a 25 % grant: **{RECORDED_10E9_25PC_MS:,.0f} ms** "
              f"(`probes/2026-08-14-project-decomposition/1e9-scattered.txt`). "
              f"Fit predicts {rec['predicted_10e9_ms']:,.0f} ms "
              f"({rec['predicted_10e9_ms'] / RECORDED_10E9_25PC_MS:.2f}× the recorded figure); "
              f"scaling the 4×10⁸ point by 2.5 gives {rec['scaled_from_4e8_ms']:,.0f} ms "
              f"({rec['scaled_from_4e8_ms'] / RECORDED_10E9_25PC_MS:.2f}×).\n")
        out["prediction_vs_recorded"] = {
            "fit_ratio": rec["predicted_10e9_ms"] / RECORDED_10E9_25PC_MS,
            "scaled_ratio": rec["scaled_from_4e8_ms"] / RECORDED_10E9_25PC_MS,
        }

    contig_keys = sorted({(x["rows"], x["coverage"]) for x in linearity
                          if x["mask"] == "contiguous"}, key=lambda k: (k[0], -k[1]))
    if contig_keys:
        print("## (a) contiguous entity set against scattered, same permutation\n")
        print("| rows | coverage | scattered ms | contiguous ms | contiguous / scattered |")
        print("|---:|---:|---:|---:|---:|")
        for n, cov in contig_keys:
            c = best(linearity, n, cov, "contiguous")
            s = best(linearity, n, cov, "scattered")
            if c and s:
                print(f"| {fmt_rows(n)} | {cov:.0%} | {s['median_ms']:.2f} | "
                      f"{c['median_ms']:.2f} | {c['median_ms'] / s['median_ms']:.2f} |")
        print()

    # ---- (b) shards ----------------------------------------------------------------------
    if shards:
        s0 = shards[0]
        print(f"## (b) one mask over {fmt_rows(s0['rows'])} rows: one permutation against "
              f"{s0['shards']} × {s0['per_shard_rows']:,}\n")
        print("`project` allocates its scratch on every call (the session path); `project_with` "
              "reuses one scratch (the artifact pass). The split is the emulation's cost of "
              "restricting the global mask to a shard and is shown so it can be subtracted.\n")
        print("| entry point | coverage | one: ms (cpu) | sharded: ms (cpu) | split ms | "
              "sharded / one | with split | one: portable bytes | sharded: portable bytes | "
              "bytes ratio | containers |")
        print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
        for s in sorted(shards, key=lambda x: (x.get("scratch", False), -x["coverage"])):
            one, sh = s["one"], s["sharded"]
            entry = "project_with" if s.get("scratch") else "project"
            print(f"| {entry} | {s['coverage']:.0%} | {one['median_ms']:.2f} "
                  f"({one['median_cpu_ms']:.2f}) | "
                  f"{sh['median_ms']:.2f} ({sh['median_cpu_ms']:.2f}) | "
                  f"{sh['split']['median_ms']:.2f} | {s['ratio_project_only']:.3f} | "
                  f"{s['ratio_with_split']:.3f} | {one['result']['portable_bytes']:,} | "
                  f"{sh['result']['portable_bytes']:,} | {s['ratio_portable_bytes']:.4f} | "
                  f"{one['result']['containers']:,} / {sh['result']['containers']:,} |")
        print()

    # ---- (b) tokens ----------------------------------------------------------------------
    if tokens:
        t0 = tokens[0]
        print(f"## (b) {t0['tokens']:,} tokens' leaf projections held at once, "
              f"{fmt_rows(t0['rows'])}-row universe at {t0['coverage']:.0%}\n")
        print("| entry point | shape | bitmaps held | project µs/token (cpu) | split µs/token | "
              "RSS Δ MB | RSS Δ KB/token | RSS Δ after trim MB | malloc in-use Δ KB/token | "
              "portable KB/token | container KB/token | containers/token |")
        print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
        for t in sorted(tokens, key=lambda x: (x.get("scratch", False), x["shards"])):
            n = t["tokens"]
            res = t["result"]
            entry = "project_with" if t.get("scratch") else "project"
            print(f"| {entry} | {t['shape']} (N={t['shards']}) | {t['bitmaps_held']:,} | "
                  f"{t['project_us_per_token']:.1f} ({t['project_cpu_us_per_token']:.1f}) | "
                  f"{t['split_us_per_token']:.1f} | "
                  f"{t['rss_delta_bytes'] / 1e6:.1f} | {t['rss_delta_bytes_per_token'] / 1e3:.2f} | "
                  f"{(t['rss_after_trim_bytes'] - t['rss_before_bytes']) / 1e6:.1f} | "
                  f"{t['malloc_in_use_delta_bytes_per_token'] / 1e3:.2f} | "
                  f"{res['portable_bytes'] / n / 1e3:.2f} | {res['container_bytes'] / n / 1e3:.2f} | "
                  f"{res['containers'] / n:.1f} |")
        out["token_ratios_sharded_over_one"] = {}
        for scratch in (False, True):
            one = next((t for t in tokens if t["shards"] == 1
                        and t.get("scratch", False) == scratch), None)
            sh = next((t for t in tokens if t["shards"] > 1
                       and t.get("scratch", False) == scratch), None)
            if not (one and sh):
                continue
            ratios = {
                "project_time": sh["project_us_per_token"] / one["project_us_per_token"],
                "project_time_with_split": (sh["project_us_per_token"] + sh["split_us_per_token"])
                / one["project_us_per_token"],
                "rss": sh["rss_delta_bytes"] / one["rss_delta_bytes"],
                "rss_after_trim": (sh["rss_after_trim_bytes"] - sh["rss_before_bytes"])
                / (one["rss_after_trim_bytes"] - one["rss_before_bytes"]),
                "malloc_in_use": sh["malloc_in_use_delta_bytes"] / one["malloc_in_use_delta_bytes"],
                "portable_bytes": sh["result"]["portable_bytes"] / one["result"]["portable_bytes"],
            }
            entry = "project_with" if scratch else "project"
            out["token_ratios_sharded_over_one"][entry] = ratios
            print()
            print(f"`{entry}`, sharded over one, per token: build time "
                  f"**{ratios['project_time']:.2f}×** ({ratios['project_time_with_split']:.2f}× "
                  f"with the split); resident set **{ratios['rss']:.2f}×** "
                  f"({ratios['rss_after_trim']:.2f}× after `malloc_trim`); allocator in-use bytes "
                  f"{ratios['malloc_in_use']:.2f}×; portable bytes {ratios['portable_bytes']:.3f}×.")
        print()

    with open(os.path.join(HERE, "result.json"), "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print(f"wrote {os.path.join(HERE, 'result.json')}", file=sys.stderr)


if __name__ == "__main__":
    main()
