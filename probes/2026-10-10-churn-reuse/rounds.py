"""One CSV row per churn round from the cycle's JSON lines, or with `--table`, the README's table."""

import csv
import json
import sys

FIELDS = [
    "round",
    "deleted",
    "live",
    "entity_id_high_water",
    "high_water_rise",
    "free_ids",
    "held_ids",
    "postings_bytes",
    "postings_vs_fresh",
    "term_images_bytes",
    "bundle_bytes",
    "fold_s",
    "narrow_union_ms",
    "narrow_p50_ms",
    "narrow_p95_ms",
    "broad_union_ms",
    "broad_p50_ms",
    "broad_p95_ms",
]


def flat(row: dict) -> dict:
    out = {k: row.get(k) for k in FIELDS}
    out["fold_s"] = (row.get("fold") or {}).get("fold_s")
    principals = sorted(row.get("principals") or [], key=lambda p: p["target"])
    for name, p in zip(["narrow", "broad"], [principals[0], principals[-1]] if principals else []):
        out[f"{name}_union_ms"] = p.get("union_ms")
        out[f"{name}_p50_ms"] = (p.get("server_ms") or {}).get("p50")
        out[f"{name}_p95_ms"] = (p.get("server_ms") or {}).get("p95")
    return out


def main() -> None:
    args = [a for a in sys.argv[1:] if a != "--table"]
    rows = [flat(json.loads(line)) for line in open(args[0])]
    if "--table" in sys.argv:
        print("| round | high water | rise | free | held | postings | × fresh | compaction s "
              "| union ms, 1% / all | p50 ms, 1% / all | p95 ms, 1% / all |")
        print("|---|---|---|---|---|---|---|---|---|---|---|")
        for r in rows:
            print(
                f"| {r['round']} | {r['entity_id_high_water']:,} | "
                f"{'' if r['high_water_rise'] is None else format(r['high_water_rise'], ',')} | "
                f"{r['free_ids']:,} | {r['held_ids']:,} | {r['postings_bytes']:,} | "
                f"{r['postings_vs_fresh']:,.0f} | {r['fold_s'] or ''} | "
                f"{r['narrow_union_ms']} / {r['broad_union_ms']} | "
                f"{r['narrow_p50_ms']} / {r['broad_p50_ms']} | "
                f"{r['narrow_p95_ms']} / {r['broad_p95_ms']} |"
            )
        return
    out = csv.DictWriter(sys.stdout, FIELDS)
    out.writeheader()
    out.writerows(rows)


if __name__ == "__main__":
    main()
