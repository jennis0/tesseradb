"""The churn simulated under allocation policies: postings bytes and high water per round.

Items carry one access term each, with GeoNames' country sizes scaled to `--items`, numbered as a
build numbers them: by signature, so each term's postings are one run. Each round deletes half the
items, inserts the same items again as new ones in windows of 10,000 sorted by signature, then
compacts, which frees the deleted ids. `alternate` deletes the two halves in turn, as the churn run
does; `random` deletes a fresh random half each round. Postings are each term's run-optimised
Roaring bitmap, serialised, summed over terms.

Policies: `none` takes every id from the high water; `lowest` takes freed ids lowest first, as
built; `runs` takes freed ids from the longest free runs first; `runs_min` takes only free runs of
at least 64; `signature` gives a term's new items the ids its deleted items held first, then the
lowest free.

    python3 policies.py <rung>/country-ranks.json --items 1000000 --pattern random \
        --policies none,lowest,signature
"""

import argparse
import json

import numpy as np
from pyroaring import BitMap

WINDOW = 10_000


def postings_bytes(sig_of_item: np.ndarray, id_of: np.ndarray) -> int:
    order = np.argsort(sig_of_item, kind="stable")
    sigs, ids = sig_of_item[order], id_of[order]
    total = 0
    for chunk in np.split(ids, np.flatnonzero(np.diff(sigs)) + 1):
        b = BitMap(np.sort(chunk).astype(np.uint32))
        b.run_optimize()
        total += len(b.serialize())
    return total


def take_ids(policy, need, items, sig_of_item, free, by_sig):
    if policy == "none" or not len(free):
        return []
    if policy == "lowest":
        return list(free[:need])
    if policy == "signature":
        take, used = [], BitMap()
        for s in np.unique(sig_of_item[items]):
            k = int((sig_of_item[items] == s).sum())
            own = by_sig.get(int(s), BitMap()) - used
            got = list(own[:k])
            used |= BitMap(got)
            if k > len(got):
                rest = free - used
                got += list(rest[: k - len(got)])
                used |= BitMap(got)
            take += got
        return take
    arr = np.array(free.to_array(), dtype=np.int64)
    runs = sorted(np.split(arr, np.flatnonzero(np.diff(arr) != 1) + 1), key=lambda a: (-len(a), a[0]))
    take = []
    for run in runs:
        if (policy == "runs_min" and len(run) < 64) or len(take) >= need:
            break
        take += list(run[: need - len(take)])
    return sorted(take)


def churn(policy, pattern, sig_of_item, rounds, seed=0):
    n = len(sig_of_item)
    rng = np.random.default_rng(seed)
    position = rng.random(n)
    id_of = np.arange(n, dtype=np.int64)
    free, by_sig, high_water, rows = BitMap(), {}, n, []
    for r in range(1, rounds + 1):
        if pattern == "alternate":
            chosen = np.flatnonzero((position - ((r - 1) * 0.5) % 1.0) % 1.0 < 0.5)
        else:
            chosen = np.flatnonzero(rng.random(n) < 0.5)
        dead = id_of[chosen].copy()
        for w in range(0, len(chosen), WINDOW):
            items = chosen[w : w + WINDOW]
            items = items[np.argsort(sig_of_item[items], kind="stable")]
            take = [int(t) for t in take_ids(policy, len(items), items, sig_of_item, free, by_sig)]
            free -= BitMap(take)
            if policy == "signature" and take:
                taken = BitMap(take)
                for s in by_sig:
                    by_sig[s] -= taken
            extra = len(items) - len(take)
            ids = np.array(take + list(range(high_water, high_water + extra)), dtype=np.int64)
            high_water += extra
            id_of[items] = ids if policy == "signature" else np.sort(ids)
        free |= BitMap(dead.astype(np.uint32))
        if policy == "signature":
            for s in np.unique(sig_of_item[chosen]):
                mine = dead[sig_of_item[chosen] == s].astype(np.uint32)
                by_sig.setdefault(int(s), BitMap()).update(mine)
        rows.append((r, high_water, postings_bytes(sig_of_item, id_of)))
    return rows


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("ranks")
    ap.add_argument("--items", type=int, default=1_000_000)
    ap.add_argument("--rounds", type=int, default=10)
    ap.add_argument("--pattern", choices=["alternate", "random"], default="random")
    ap.add_argument("--policies", default="none,lowest,signature")
    args = ap.parse_args()
    counts = np.array(sorted((int(r["pairs"]) for r in json.load(open(args.ranks))), reverse=True))
    counts = np.maximum(1, np.round(counts * args.items / counts.sum()).astype(int))
    sig_of_item = np.repeat(np.arange(len(counts)), counts)
    fresh = postings_bytes(sig_of_item, np.arange(len(sig_of_item)))
    print("pattern,policy,round,high_water,postings_bytes,vs_fresh")
    for policy in args.policies.split(","):
        for r, high_water, b in churn(policy, args.pattern, sig_of_item, args.rounds):
            print(f"{args.pattern},{policy},{r},{high_water},{b},{b / fresh:.1f}")


if __name__ == "__main__":
    main()
