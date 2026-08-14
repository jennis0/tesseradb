"""What a `utf8` filter column costs, and what any other encoding of it would cost.

Three questions, one corpus of real arXiv titles:

1. **Block compression** — the obvious answer, and the one the filter column cannot take. Ratio and
   decompression cost across block size, so the two curves can be read against each other.
2. **Per-value-independent encodings** — FSST-like symbol tables and front coding, which keep the
   random access block compression destroys.
3. **The baseline** — what parquet achieves on the same values, as the "native representation"
   reference point.

Ratios are measured. Decompression *throughput* is measured for zstd (pyarrow calls into C);
FSST-like decode speed is **not** measured and is not claimed here — a Python inner loop says
nothing about a C implementation, and the published figure is cited in the memo instead.

The corpus is four contiguous 75,000-record blocks read at 5%, 35%, 65% and 92% through the Kaggle
snapshot, rather than a random sample: contiguity is what a dictionary or a front coder would
exploit, so a scattered sample would flatter every scheme that groups similar values.

Usage:
    reference/.venv/bin/python probes/2026-08-12-string-storage/schemes.py \
        --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json
"""

import argparse
import collections
import io
import json
import os
import time

import pyarrow as pa
import pyarrow.parquet as pq

ap = argparse.ArgumentParser()
ap.add_argument("--snapshot", required=True, help="the Kaggle arXiv metadata JSON-lines snapshot")
ap.add_argument("--per-block", type=int, default=75_000)
args = ap.parse_args()

PATH = os.path.expanduser(args.snapshot)
size = os.path.getsize(PATH)

titles = []
with open(PATH, "rb") as f:
    for frac in (0.05, 0.35, 0.65, 0.92):
        f.seek(int(size * frac))
        f.readline()  # discard the partial line the seek landed inside
        for _ in range(args.per_block):
            line = f.readline()
            if not line:
                break
            try:
                titles.append(" ".join(json.loads(line)["title"].split()))
            except Exception:
                pass

vals = [t.encode() for t in titles]
n = len(vals)
raw = sum(len(v) for v in vals)
blob = b"".join(vals)
mean = raw / n
# What the column costs today: the bytes, plus one 64-bit offset per value. The width is a capacity
# requirement at 10^9, not a preference (`Codes::Text`), so it is what every scale pays.
ours = mean + 8

print(f"{n:,} titles, {mean:.1f} B mean, {len(set(vals)):,} distinct, {raw/1e6:.1f} MB concatenated")

# ---------------------------------------------------------------- 1. block compression
print("\n## Block compression (zstd)\n")
print(f"{'block':>9} {'ratio':>7} {'B/value':>9} {'GB/s':>7} {'ns/value':>10} {'ns/value':>11}")
print(f"{'':>9} {'':>7} {'stored':>9} {'':>7} {'block read':>10} {'one value':>11}")
for block in (4096, 16384, 65536, 262144, 1 << 20):
    blocks = [blob[i : i + block] for i in range(0, len(blob), block)]
    comp = [pa.compress(pa.py_buffer(b), codec="zstd") for b in blocks]
    stored = sum(len(c) for c in comp)
    t0 = time.perf_counter()
    for c, b in zip(comp, blocks):
        pa.decompress(c, decompressed_size=len(b), codec="zstd")
    dt = time.perf_counter() - t0
    # Two costs, and the gap between them is the whole finding: amortised over every value in a
    # block (a scan that reads all of it) against one block decompressed to read one value.
    seq = dt / n * 1e9
    rand = dt / len(blocks) * 1e9
    print(
        f"{block:>9} {len(blob)/stored:>7.2f} {stored/n + 8:>9.1f} "
        f"{len(blob)/dt/1e9:>7.2f} {seq:>9.1f} {rand:>10.0f}"
    )

# ---------------------------------------------------------------- 2. per-value encodings
# An FSST-like symbol table: up to 255 symbols of 1-8 bytes, greedy longest match, one escape byte
# for an unencodable literal. The table build is deliberately crude — a few counting rounds rather
# than the published construction — so the ratio it reaches is a **floor**, not FSST's number.
def build_table(sample, rounds=4, limit=255):
    table = set()
    for _ in range(rounds):
        gain = collections.Counter()
        for v in sample:
            i = 0
            while i < len(v):
                best = None
                for length in range(8, 0, -1):
                    if v[i : i + length] in table:
                        best = v[i : i + length]
                        break
                if best is None:
                    best = v[i : i + 1]
                    for length in range(2, 9):
                        if i + length <= len(v):
                            gain[v[i : i + length]] += length - 1
                else:
                    gain[best] += len(best) - 1
                    nxt = v[i + len(best) : i + len(best) + 8]
                    if nxt:
                        gain[best + nxt[:4]] += len(best) + min(4, len(nxt)) - 1
                i += len(best)
        table = set(s for s, _ in gain.most_common(limit))
    return table


def encoded_len(v, table):
    out, i = 0, 0
    while i < len(v):
        hit = None
        for length in range(8, 1, -1):
            if v[i : i + length] in table:
                hit = length
                break
        if hit:
            out += 1
            i += hit
        elif v[i : i + 1] in table:
            out += 1
            i += 1
        else:
            out += 2  # escape byte + literal
            i += 1
    return out


table = build_table(vals[:20_000])
sample = vals[:40_000]
fsst = sum(encoded_len(v, table) for v in sample) / len(sample)

# Front coding over the *sorted* distinct values — what a Lucene terms dictionary does. One byte of
# shared-prefix length, then the suffix.
srt = sorted(set(vals))
front, prev = 0, b""
for v in srt:
    k = 0
    while k < min(len(v), len(prev), 255) and v[k] == prev[k]:
        k += 1
    front += 1 + (len(v) - k)
    prev = v
front_per = front / len(srt)
ordinal_bytes = ((len(srt) - 1).bit_length() + 7) / 8

print("\n## Per-value-independent encodings (random access survives)\n")
print(f"{'scheme':44} {'B/value':>9} {'vs ours':>9}")
print(f"{'flat column: bytes + i64 offset (today)':44} {ours:>9.1f} {1.00:>8.2f}x")
print(f"{'  + u16 length, per-block i64 base':44} {mean + 2:>9.1f} {ours/(mean+2):>8.2f}x")
print(f"{'FSST-like symbol table (floor)':44} {fsst + 2:>9.1f} {ours/(fsst+2):>8.2f}x")
print(
    f"{'front-coded sorted dict + ordinal':44} "
    f"{front_per + ordinal_bytes:>9.1f} {ours/(front_per+ordinal_bytes):>8.2f}x"
)
print(
    f"\nfront coding alone: {mean:.1f} -> {front_per:.1f} B/value ({mean/front_per:.2f}x). "
    f"Weak because titles share no prefixes; a surname column would differ."
)

# ---------------------------------------------------------------- 3. the parquet reference
tbl = pa.table({"title": pa.array(titles, pa.string())})


def parquet_bytes(**kw):
    buf = io.BytesIO()
    pq.write_table(tbl, buf, **kw)
    return buf.tell()


print("\n## Parquet, the same values (no random access — the reference point, not an option)\n")
for name, v in [
    ("zstd, no dictionary", parquet_bytes(compression="zstd", use_dictionary=False)),
    ("zstd + dictionary", parquet_bytes(compression="zstd", use_dictionary=True)),
    ("snappy + dictionary", parquet_bytes(compression="snappy", use_dictionary=True)),
]:
    print(f"{name:44} {v/n:>9.1f} {ours/(v/n):>8.2f}x")
