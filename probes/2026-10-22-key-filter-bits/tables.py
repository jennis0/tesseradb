"""The probe's tables, from the raw-*.txt files run.sh writes beside this file.

    python3 tables.py
"""

import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUNS = [
    ("geonames-13.5m", "13.5 M"),
    ("run-max-67m", "2^26"),
    ("treeoflife-233m", "233 M"),
    ("run-max-67m-random", "2^26 spread"),
]
# The shapes a writer would choose for each size: the hash count nearest bits × ln 2, at most 7.
MATCHED = ["0", "6:4", "8:6", "10:7", "12:7", "16:7"]
NS = [1, 4, 14, 32, 128]


def load(name):
    rows = {}
    for line in (HERE / f"raw-{name}.txt").read_text().splitlines():
        if line.startswith("filter-shape "):
            row = json.loads(line.split(" ", 1)[1])
            rows[row["spec"]] = row
    return rows


def blocked_fpr(bits, k, m=512):
    """A blocked Bloom filter's false-positive rate: keys per block Poisson, k bits each."""
    lam, p, total = m / bits, math.exp(-m / bits), 0.0
    for j in range(400):
        if j:
            p *= lam / j
        total += p * (1 - (1 - 1 / m) ** (k * j)) ** k
    return total


def us(ns):
    return f"{ns / 1000:.2f}" if ns < 10_000 else f"{ns / 1000:.0f}"


def median(row, case):
    return row[case]["p50_ns"] if row.get(case) else None


def table_fpr(data):
    print("## False positives\n")
    head = "| Bits:hashes | Formula | " + " | ".join(label for _, label in RUNS) + " |"
    print(head)
    print("|" + "---|" * (2 + len(RUNS)))
    for spec in data[RUNS[0][0]]:
        if spec == "0":
            continue
        bits, k = map(int, spec.split(":"))
        cells = []
        for name, _ in RUNS:
            row = data[name][spec]
            cells.append(f"{100 * row['fpr']:.3f}% ({row['fpr_passed']:,})")
        print(f"| {spec} | {100 * blocked_fpr(bits, k):.3f}% | " + " | ".join(cells) + " |")
    print()
    print("Formula only, hash counts the format cannot hold:", end=" ")
    print(f"12:8 {100 * blocked_fpr(12, 8):.3f}%, 16:11 {100 * blocked_fpr(16, 11):.3f}%\n")


def table_bytes(data):
    print("## Bytes\n")
    print("| Bits | " + " | ".join(f"{label} filter MB | share" for _, label in RUNS) + " |")
    print("|" + "---|" * (1 + 2 * len(RUNS)))
    for spec in MATCHED:
        cells = []
        for name, _ in RUNS:
            row = data[name][spec]
            cells.append(f"{row['filter_bytes'] / 1e6:.1f} | {100 * row['filter_bytes'] / row['rest_bytes']:.1f}%")
        print(f"| {spec.split(':')[0]} | " + " | ".join(cells) + " |")
    print()
    for name, label in RUNS:
        row = data[name]["0"]
        print(f"{label}: run without a filter {row['file_bytes'] / 1e6:.1f} MB, "
              f"{row['file_bytes'] / row['entries']:.2f} bytes an entry")
    print()
    print("Whole corpus, one unique column (modelled: items × bits / 8; index at 4.28 bytes an "
          "entry):\n")
    corpora = [("GeoNames", 13.5e6), ("Tree of Life", 233e6), ("GBIF", 3.5e9)]
    print("| Bits | " + " | ".join(f"{c} filter GB" for c, _ in corpora) + " |")
    print("|" + "---|" * (1 + len(corpora)))
    for bits in [6, 8, 10, 12, 16]:
        print(f"| {bits} | " + " | ".join(f"{n * bits / 8 / 1e9:.2f}" for _, n in corpora) + " |")
    print("| index | " + " | ".join(f"{n * 4.28 / 1e9:.2f}" for _, n in corpora) + " |")
    print()


def table_costs(data, name):
    rows = data[name]
    print(f"## Single lookups, {dict(RUNS)[name]} entries (mean µs, p50 in brackets)\n")
    print("| Bits:hashes | open cold ms | open, filter read ahead, ms | present warm | refused warm | maybe warm "
          "| present, pages cold | refused, pages cold | maybe, pages cold "
          "| present cold | refused cold | maybe cold | load |")
    print("|" + "---|" * 13)
    for spec in MATCHED:
        row = rows[spec]
        cells = [f"{row['open_cold_ns'] / 1e6:.1f}", f"{row['open_cold_readahead_ns'] / 1e6:.1f}"]
        for case in ["present_warm", "refused_warm", "maybe_warm",
                     "present_pages_cold", "refused_pages_cold", "maybe_pages_cold",
                     "present_cold", "refused_cold", "maybe_cold"]:
            s = row.get(case)
            cells.append(f"{us(s['mean_ns'])} ({us(s['p50_ns'])})" if s else "–")
        cells.append(row["load_before"].split()[0])
        print(f"| {spec} | " + " | ".join(cells) + " |")
    print()


def table_batch(data, name):
    rows = data[name]
    print(f"## A batch of absent keys, {dict(RUNS)[name]} entries (ms)\n")
    print("| Bits:hashes | keys | passed | warm | pages cold (median of 3) | cold (median of 3) |")
    print("|---|---|---|---|---|---|")
    for spec in MATCHED:
        row = rows[spec]
        print(f"| {spec} | {row['batch_keys']:,} | {row['batch_passed']:,} | "
              f"{row['batch_warm_ns'] / 1e6:.2f} | {row['batch_pages_cold']['p50_ns'] / 1e6:.1f} | "
              f"{row['batch_cold']['p50_ns'] / 1e6:.1f} |")
    print()


def expected(row, none_row, n, state):
    """A unique lookup over n runs, one holding the key, in µs, from each case's median."""
    hit = median(row, f"present_{state}")
    if row["bits"] == 0:
        return (hit + (n - 1) * median(row, f"maybe_{state}")) / 1000
    p = row["fpr"]
    miss = (1 - p) * median(row, f"refused_{state}") + p * median(row, f"maybe_{state}")
    return (hit + (n - 1) * miss) / 1000


def table_expected(data, name):
    rows = data[name]
    print(f"## Expected cost over N runs, per-run costs from {dict(RUNS)[name]} entries (µs, modelled from medians)\n")
    for state, label in [("warm", "warm"), ("pages_cold", "entry pages cold, filter resident"),
                         ("cold", "entry pages and filter cold")]:
        print(f"{label}:\n")
        print("| Bits:hashes | " + " | ".join(f"N = {n}" for n in NS) + " |")
        print("|" + "---|" * (1 + len(NS)))
        for spec in MATCHED:
            cells = [f"{expected(rows[spec], rows['0'], n, state):,.1f}" for n in NS]
            print(f"| {spec} | " + " | ".join(cells) + " |")
        print()


def main():
    global RUNS
    data = {name: load(name) for name, _ in RUNS if (HERE / f"raw-{name}.txt").exists()}
    RUNS = [(name, label) for name, label in RUNS if name in data and "0" in data[name]]
    table_fpr(data)
    table_bytes(data)
    for name, _ in RUNS:
        table_costs(data, name)
        table_batch(data, name)
    for name in ["geonames-13.5m", "run-max-67m"]:
        if name in data:
            table_expected(data, name)


if __name__ == "__main__":
    main()
