# Churn with reused numbers, GeoNames at one shard

**Status:** measurement, 2026-10-10 and 11. Stage 1's gate run (`docs/sharding.md` §8.4) at one shard.
Server: `target/release/mosaica` at 9daa44c7, the first commit that frees a deleted item's number.
Harness: `test_corpora/common/ingest_cycle`'s churn mode at f8515f3a. Corpus: GeoNames at the
2026-10-10 vintage, 13,472,324 items, built by the same binary. Host: a cloud container with 4 cores
and 15 GB, nothing else running. It has no user bus for `systemd-run --user --scope`, so the cycle
served through a stand-in that runs the command without a cgroup, and its eviction reached the page
cache only. Every figure is measured.

## What was measured

[`run.sh`](run.sh) runs ten rounds. Each deletes half the corpus through `/control/changes`,
ingests the same rows again as new items, flushes, and compacts. The halves alternate, so every item
is deleted and inserted again every second round. After each compaction the cycle records:

- the high water and the free and held ids from `/control/status` and the side-manifest;
- the term index's bytes against the bundle as built, which round 0 is;
- the compaction's own time;
- each of two principals' union, timed as the authorisation that builds its fragment;
- viewport latency over the serve battery's boxes.

The two principals are 1% of the corpus (141,823 items) and all of it. `python3 rounds.py <jsonl>`
writes [`rounds.csv`](rounds.csv), and with `--table` prints the table below.

Afterwards the bundle as built and round 10's churned bundle were each served by a new server with
no rounds, back to back and twice over. [`ab.csv`](ab.csv) holds those runs.

## Findings

**The high water stops rising once numbers are reused.**

- **Round 1:** it rose by 710,000. A compaction triggered by the deletion gauge ran during the
  ingest and freed the deleted half, so the rest of the round's new items took freed numbers.
- **Round 2:** it rose by the 6,026,690 the free ids could not cover.
- **Rounds 3 to 10:** it did not move, at 20,209,014.

That plateau is the live count plus one round's inserts, because each round inserts before its
compaction frees the half it deleted. After each compaction the freed ids are held until the log
rotates past it, and the next round's new items take all of them.

**Every round held.**

- The live count returned to 13,472,324 each time.
- No compaction was refused.
- Compaction took 338 to 429 s with no trend.
- The cycle's checks found nothing. These include that no round took more ids from the high water
  than the free ids could not cover.

**Reused numbers fragment the term index.**

- Its postings grew from 9,394 bytes as built to 7.1 to 12.9 MB after a round, 761 to 1,373 times.
  That is 0.5 to 1 byte an item, against a 2.0 GB bundle.
- GeoNames as built is a best case: each item carries one access term, and items are numbered by
  signature, so each term's postings are one run.
- A new item takes the lowest freed number, and the halves are drawn at random. So each term's
  items scatter across the space, and its containers become arrays and bitmaps.
- Without reuse, new items would take high-water ids in signature order and stay in runs. This was
  not measured.

**The union costs more; viewport latency does not change.** Served side by side by new servers:

| | union, 1% | union, all | p95, 1% | p95, all |
|---|---|---|---|---|
| as built | 2.9 to 4.2 ms | 6.3 to 13.3 ms | 51.0 to 51.3 ms | 163.7 to 175.3 ms |
| round 10 | 9.7 to 13.1 ms | 16.7 to 22.1 ms | 51.7 to 56.7 ms | 161.7 to 162.6 ms |

The union is paid once per grant until the next write moves its fragment.

**The long-running server's tail grew; the churned bundle's did not.**

- During the run, the 95th percentile rose by rounds 7 to 10: 164 ms became 207 to 233 ms for the
  whole corpus, and 60 ms became 83 to 101 ms for 1%.
- A new server over round 10's bundle served the same tail as the bundle as built.
- So the rise belongs to the server that ran the ten rounds, or to the host, and not to reused
  numbers. Not investigated.

**Compaction reports.** Each compaction's report, `reports/fold-v*.json`, was 71 MB here, listing
347,583 admin artifacts that lost members to the deletions. Every version's report is kept, so the
bundle grew by 71 MB a compaction. This holds without reuse.

## Not measured

- Tree of Life (§8.4's second corpus; its 40 GB bundle does not fit this host).
- Several shards (stage 5).
- Handing pool ids out in runs (§4.1).
- The same churn without reuse.
- The union apart from the session plane and its round trip.

## Rounds

| round | high water | rise | free | held | postings | × fresh | compaction s | union ms, 1% / all | p50 ms, 1% / all | p95 ms, 1% / all |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 13,472,324 |  | 0 | 0 | 9,394 | 1 |  | 11.77 / 7.05 | 11.925 / 11.698 | 60.228 / 164.006 |
| 1 | 14,182,324 | 710,000 | 710,000 | 0 | 9,166,450 | 976 | 429 | 10.88 / 17.18 | 11.07 / 11.511 | 54.415 / 175.41 |
| 2 | 20,209,014 | 6,026,690 | 0 | 6,736,690 | 7,146,482 | 761 | 338 | 12.57 / 37.62 | 13.273 / 11.882 | 68.461 / 177.695 |
| 3 | 20,209,014 | 0 | 1,056 | 6,735,634 | 7,596,210 | 809 | 417 | 22.99 / 30.9 | 10.987 / 11.108 | 59.595 / 181.046 |
| 4 | 20,209,014 | 0 | 0 | 6,736,690 | 12,898,226 | 1,373 | 375 | 14.46 / 17.41 | 11.554 / 12.252 | 57.677 / 184.032 |
| 5 | 20,209,014 | 0 | 1,056 | 6,735,634 | 7,600,434 | 809 | 363 | 10.33 / 42.61 | 10.566 / 12.544 | 68.031 / 168.276 |
| 6 | 20,209,014 | 0 | 0 | 6,736,690 | 7,244,338 | 771 | 340 | 8.37 / 31.36 | 10.826 / 11.873 | 73.442 / 182.487 |
| 7 | 20,209,014 | 0 | 1,056 | 6,735,634 | 12,852,146 | 1,368 | 350 | 124.56 / 18.5 | 11.725 / 11.594 | 90.986 / 233.498 |
| 8 | 20,209,014 | 0 | 0 | 6,736,690 | 7,536,562 | 802 | 360 | 144.38 / 35.41 | 11.684 / 13.507 | 100.749 / 228.428 |
| 9 | 20,209,014 | 0 | 1,056 | 6,735,634 | 7,232,434 | 770 | 358 | 8.6 / 45.93 | 11.142 / 13.132 | 86.981 / 211.706 |
| 10 | 20,209,014 | 0 | 0 | 6,736,690 | 12,857,650 | 1,369 | 377 | 26.52 / 22.29 | 15.237 / 11.976 | 83.263 / 207.68 |
