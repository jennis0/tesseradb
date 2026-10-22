# The key filter's bits per key

**Status:** measurement, 2026-10-22, on main at e73d47ad with the harness this probe adds. Host: the
12-core Ryzen 9 5900X with 47 GB under WSL2, kernel 6.18, the scratch files on the ext4 disc the
bundles live on. Other jobs shared the host: the one-minute load average was 21 to 24 during the
GeoNames-sized and the first half of the 2²⁶ runs, and 7 to 14 after. Each shape's load is in
its raw line. The bench is one thread, run under `nice -n 10`. Every figure is measured unless
marked modelled.

This is the key filter item of §8.5 of [the sharding plan](../../docs/sharding.md). A unique `eq`
lookup, and a write resolving a unique value, reads every live run of the column's index in every
shard and the one base run per shard whose key range holds the value (§4.2). A run's key filter is
a blocked Bloom filter, one 64-byte block per key, written at `BITS_PER_KEY = 10` and `HASHES = 7`
(`crates/mosaica-store/src/key_index/filter.rs`). The writer fixes the hash count at 7 whatever the
size, and the format holds at most 7: each hash takes 9 bits of one 64-bit word.

## What was measured

Harness: `crates/mosaica-bench/src/bin/key_index_scale.rs --filters`, which writes one run through
the store's own `KeyRunWriter` for each filter shape and reads it with `KeyRun`. The store gained
one knob for it, `KeyRunWriter::with_key_filter_of(bits, hashes)`, and `KeyRun::may_hold` became
public so the bench can count the filter's answers. The default is unchanged. Sequence:
[`run.sh`](run.sh); `python3 tables.py` prints the tables below from the `raw-*.txt` beside it.

- **Runs.** Record-number keys (`--keys dense`: the `i`th key is `2i` or `2i + 1`), as GeoNames,
  Tree of Life and GBIF identifiers are, at 13.5 million entries (GeoNames at one shard), at 2²⁶
  (`RUN_MAX_ENTRIES`, the largest run a unique index writes, so every base run of Tree of Life and
  GBIF) and at 233 million as one run. A fourth run of 2²⁶ keys spread over the `u64` range
  (`--keys random`) packs at 8.9 bytes an entry instead of 4.3.
- **Shapes.** No filter, and bits:hashes 6:4, 6:7, 8:5, 8:6, 8:7, 10:7, 12:7 and 16:7. The hash
  count nearest bits × ln 2 is 4 at 6 bits, 5 or 6 at 8, 7 at 10, 8 at 12 and 11 at 16; the last
  two cannot be written, so the formula stands in for them.
- **False positives.** Up to 20 million keys absent from the run, spread evenly over its key range,
  each asked of `may_hold`.
- **Single lookups.** A one-key `lookup_sorted`, the call a unique index makes, for 2,000 present
  keys, 2,000 absent keys the filter refuses and 2,000 absent keys it passes ("maybe"). Warm: every
  page resident, second pass. Pages cold: the run open and its entry pages dropped before each
  lookup, the filter resident. Cold: entry pages and filter blocks dropped before each lookup
  (`madvise(MADV_PAGEOUT)` on the run's mapping, then `posix_fadvise(DONTNEED)`); the header and
  page index stay resident, as they do once a run is open. 300 lookups for each cold case. A cold
  read under WSL2 may come from the host's cache, not the disc.
- **Batch.** One `lookup_sorted` of 10,000 sorted absent keys, as a write of new values resolves
  against a run that holds none of them: warm (fastest of five), pages cold and cold (median of
  three).
- **Open.** `KeyRun::open` with the whole file dropped from the page cache. The open checks the
  filter's checksum, so it reads every filter byte. It was timed again with
  `posix_fadvise(WILLNEED)` over the filter first.

## Findings

**The measured false-positive rate is the blocked Bloom formula's, at every run size and key
spread.** 10:7 passes 0.966% to 0.971% of absent keys against 0.957% modelled (Table 1). The rate
does not move with the run's size from 13.5 million to 233 million keys, or with record numbers
against spread keys.

**The fixed hash count costs nothing measurable.** At 8 bits, 6 hashes pass 2.36% and 7 pass
2.55%. At 12 and 16 bits the format's cap of 7 against the best 8 and 11 costs 0.409% against
0.407%, and 0.099% against 0.086% (both from the formula). Only at 6 bits does it matter: 7 hashes
pass 7.7% against 5.8% for 4.

**Warm, the filter saves about 1.7 µs a run.** A miss without a filter decodes one page: 1.6 to
2.0 µs. A refused key costs 0.06 µs. A unique lookup over 14 runs takes 23 µs without a filter and
3 µs at 10 bits, and over 128 runs 209 µs and 11 µs (Table 4, modelled from the medians).

**With the entry pages out of the page cache and the filter in it, the filter saves one read a
run.** A cold read here is about 160 µs. Over 14 runs a lookup costs 2.2 ms without a filter and
0.20 ms at 10 bits; over 128 runs 20 ms and 0.44 ms. Between 6 and 16 bits the expected cost of
each extra run is the false-positive rate times one read: 8.5 µs at 6:4, 4 µs at 8:6, 2 µs at 10:7,
1 µs at 12:7 and 0.5 µs at 16:7. Every one of those is below the 160 µs of the run that holds the
key.

**With the filter's own pages out of the cache, the filter saves nothing.** A refused key then
reads the filter's page, about 150 µs, which is what a miss without a filter costs, and a key the
filter passes reads two pages, about 320 µs. Over 14 runs: 2.6 ms without a filter and 2.4 ms at
10 bits.

**A write resolving new values gains most.** A batch of 10,000 absent keys against one 2²⁶ run takes
11.6 ms warm without a filter and 0.36 ms at 10 bits; with the entry pages cold, 101 ms and 2.6 ms
(Table 3). Across 14 runs that is 160 ms against 5 ms warm. With the filter's pages cold a batch is
slower with a filter: 1.6 s at 10 bits against 78 ms without, and 13 to 60 times the time without
a filter at every size and shape. A batch prefetches the entry pages it needs in ranges, and the
filter's probes fault one page at a time.

**Opening a run reads its filter one page at a time.** The run is mapped for random access and the
open checks the filter's checksum through the mapping, so with the file out of the page cache each
4 KiB of filter is one fault and one read. A 2²⁶ run opens in 1.1 ms without a filter and 3.7 s at
10 bits; at 233 million keys, 9 ms and 10 s (Table 2). Advising `WILLNEED` first brought the 13.5
million run from 1.0 s to 0.38 s and did not help at 2²⁶ or above. A sequential read of 88 MB, about
the size of a 2²⁶ run's filter at 10 bits, took 31 ms from a cold page cache on this host. The first
unique lookup after a cold start opens one run per shard plus the live runs, so on GBIF at 14
shards it would spend about 50 s in opens at 10 bits (modelled: 14 × 3.7 s).

**Writing a filter adds about 0.1 µs a key, whatever its size.** A 2²⁶ run writes in 1.3 s without
a filter and in 6.4 to 13.7 s with one, from 6 to 16 bits, with no order between the sizes beyond
the load's noise; 233 million keys take 3.9 s and 29 to 43 s. The writer reads the run's pages back
and sets bits at random across the filter, one cache miss a key.

**At 10 bits a filter is 29% of a record-number run's other bytes and 14% of a spread one's.** On
GBIF's 3.5 billion items one unique column's filters total 4.4 GB, against 15 GB of index (Table
2, modelled). 8 bits would save 0.9 GB and 12 bits cost 0.9 GB more.

## What it decides

Keep the filter, at 10 bits and 7 hashes. The filter is needed: when its pages are resident it
takes a lookup over 14 runs from 2.2 ms to 0.2 ms with the entry pages cold, and a write's batch of
new values from 160 ms to 5 ms warm. No figure argues for another size. Between 8 and 12 bits the
cost of a lookup moves by 1 to 2 µs a run, under the noise of the one read the run holding the key
costs, while the filter's memory moves by 0.25 bytes a key.

Two costs come from how the filter is read, and are left for a decision. A cold open reads the
filter page by page, about 3.7 s for each 2²⁶ run at 10 bits. A filter whose pages have been
evicted makes a batch 13 to 60 times slower than no filter. An open that read the filter
sequentially into memory the process holds would remove both, at the price of that memory: 4.4 GB
of filters on GBIF at 10 bits, counted against the server's memory cap instead of the page cache.
Not measured: that
change.

The measurement assumed every run's key range holds the key. Within a shard only the base run whose
range holds the key is read. Where record numbers arrive in order, each shard's runs cover a narrow
range of keys and the range check alone skips most of them.

## Table 1: false positives

Share of absent keys the filter passes, with the count passed in brackets. Formula: a blocked Bloom
filter of 512-bit blocks with Poisson keys a block.

| Bits:hashes | Formula | 13.5 M | 2²⁶ | 233 M | 2²⁶ spread |
|---|---|---|---|---|---|
| 6:4 | 5.753% | 5.765% (778,289) | 5.776% (1,155,118) | 5.770% (1,154,014) | 5.785% (1,156,943) |
| 6:7 | 7.679% | 7.726% (1,042,964) | 7.744% (1,548,787) | 7.740% (1,547,927) | 7.747% (1,549,365) |
| 8:5 | 2.312% | 2.324% (313,699) | 2.329% (465,835) | 2.326% (465,118) | 2.325% (464,948) |
| 8:6 | 2.342% | 2.356% (318,038) | 2.366% (473,140) | 2.363% (472,568) | 2.363% (472,551) |
| 8:7 | 2.526% | 2.550% (344,244) | 2.555% (511,049) | 2.550% (509,939) | 2.557% (511,447) |
| 10:7 | 0.957% | 0.966% (130,364) | 0.970% (194,064) | 0.966% (193,220) | 0.971% (194,101) |
| 12:7 | 0.409% | 0.415% (56,040) | 0.415% (83,020) | 0.415% (82,932) | 0.415% (83,079) |
| 16:7 | 0.099% | 0.099% (13,399) | 0.101% (20,111) | 0.101% (20,115) | 0.100% (19,990) |

Modelled only, hash counts the format cannot hold: 12:8 0.407%, 16:11 0.086%. Keys tested: 13.5
million at 13.5 M, 20 million elsewhere.

## Table 2: bytes, and the cold open

A filter's bytes and its share of the rest of the run (header, entry pages and page index). Without
a filter the runs are 57.7 MB, 286.9 MB and 996.1 MB at 4.28 bytes an entry, and the spread run is
598.7 MB at 8.92.

| Bits | 13.5 M MB | share | 2²⁶ MB | share | 233 M MB | share | 2²⁶ spread share |
|---|---|---|---|---|---|---|---|
| 6 | 10.1 | 17.5% | 50.3 | 17.5% | 174.8 | 17.5% | 8.4% |
| 8 | 13.5 | 23.4% | 67.1 | 23.4% | 233.0 | 23.4% | 11.2% |
| 10 | 16.9 | 29.2% | 83.9 | 29.2% | 291.3 | 29.2% | 14.0% |
| 12 | 20.3 | 35.1% | 100.7 | 35.1% | 349.5 | 35.1% | 16.8% |
| 16 | 27.0 | 46.8% | 134.2 | 46.8% | 466.0 | 46.8% | 22.4% |

One unique column's filters over a whole corpus, modelled as items × bits ÷ 8, beside its index at
4.28 bytes an entry:

| Bits | GeoNames GB | Tree of Life GB | GBIF GB |
|---|---|---|---|
| 6 | 0.01 | 0.17 | 2.62 |
| 8 | 0.01 | 0.23 | 3.50 |
| 10 | 0.02 | 0.29 | 4.38 |
| 12 | 0.02 | 0.35 | 5.25 |
| 16 | 0.03 | 0.47 | 7.00 |
| index | 0.06 | 1.00 | 14.98 |

Cold open of one run, ms; in brackets, with the filter advised `WILLNEED` first:

| Bits:hashes | 13.5 M | 2²⁶ | 233 M | 2²⁶ spread |
|---|---|---|---|---|
| 0 | 0.5 (0.7) | 1.1 (1.2) | 9.1 (2.3) | 1.5 (1.4) |
| 6:4 | 591 (142) | 1,644 (1,400) | 7,208 (6,257) | 1,734 (1,406) |
| 8:6 | 761 (127) | 1,918 (2,067) | 8,320 (6,550) | 2,457 (2,080) |
| 10:7 | 1,020 (378) | 3,653 (4,378) | 10,091 (13,268) | 2,666 (3,289) |
| 12:7 | 1,379 (948) | 4,569 (3,834) | 15,419 (11,786) | 4,427 (3,218) |
| 16:7 | 1,847 (1,156) | 4,301 (4,794) | 16,473 (15,842) | 4,075 (3,779) |

## Table 3: one lookup against one run

The 2²⁶ run, the size of every base run of Tree of Life and GBIF. Mean µs, median in brackets. The
means carry the tail of the host's other jobs; the medians are steadier. "Refused" and "maybe" are
absent keys the filter refuses and passes; with no filter every absent key is a maybe. Load: 23 at
no filter and 6:4, 15 at 8:6, 13 at 10:7 to 16:7.

| Bits:hashes | present warm | refused warm | maybe warm | present, pages cold | refused, pages cold | maybe, pages cold | present cold | refused cold | maybe cold |
|---|---|---|---|---|---|---|---|---|---|
| 0 | 1.91 (1.73) | – | 1.68 (1.63) | 616 (159) | – | 497 (160) | 652 (167) | – | 1,048 (185) |
| 6:4 | 1.94 (1.84) | 0.07 (0.06) | 1.87 (1.78) | 142 (140) | 1.41 (0.35) | 152 (142) | 345 (291) | 148 (145) | 315 (289) |
| 8:6 | 1.83 (1.79) | 0.06 (0.06) | 1.79 (1.72) | 698 (167) | 1.38 (0.45) | 459 (147) | 329 (317) | 175 (152) | 330 (324) |
| 10:7 | 2.09 (1.98) | 0.06 (0.06) | 1.91 (1.83) | 292 (172) | 1.49 (0.52) | 204 (167) | 349 (329) | 167 (158) | 604 (320) |
| 12:7 | 1.94 (1.84) | 0.09 (0.08) | 2.17 (1.91) | 757 (175) | 1.58 (0.45) | 899 (174) | 1,229 (341) | 775 (167) | 1,446 (342) |
| 16:7 | 1.94 (1.87) | 0.06 (0.07) | 2.16 (2.04) | 170 (162) | 1.16 (0.37) | 165 (154) | 331 (303) | 168 (146) | 618 (323) |

The 13.5 M, 233 M and spread runs give the same medians to within 50 µs cold and 0.6 µs warm;
they are in `tables.py`'s output.

A batch of 10,000 absent keys against one run, ms; "passed" is how many the filter passed:

| Bits:hashes | 2²⁶ passed | 2²⁶ warm | 2²⁶ pages cold | 2²⁶ cold | 13.5 M warm | 13.5 M pages cold | 13.5 M cold | 233 M warm | 233 M pages cold | 233 M cold |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 10,000 | 11.64 | 100.8 | 78.1 | 3.88 | 49.6 | 30.8 | 12.24 | 94.4 | 95.7 |
| 6:4 | 584 | 0.94 | 6.5 | 990 | 1.07 | 8.8 | 860 | 1.98 | 6.4 | 1,640 |
| 8:6 | 253 | 0.90 | 4.3 | 1,187 | 0.57 | 3.1 | 914 | 0.67 | 3.9 | 1,659 |
| 10:7 | 85 | 0.36 | 2.6 | 1,632 | 0.32 | 7.2 | 1,290 | 0.32 | 1.9 | 1,791 |
| 12:7 | 39 | 0.61 | 2.1 | 2,040 | 0.33 | 1.4 | 1,720 | 0.28 | 1.8 | 1,760 |
| 16:7 | 11 | 0.20 | 1.7 | 1,473 | 0.20 | 1.0 | 1,835 | 0.22 | 0.8 | 1,825 |

## Table 4: a unique lookup over N runs

Modelled from Table 3's medians for the 2²⁶ run: the run holding the key costs the present case, and
each of the other N − 1 costs (1 − p) × refused + p × maybe, p the measured false-positive rate.
Without a filter each other run costs the maybe case. µs. N = 14 is GBIF at a shard size of 2²⁸
with no live runs; N = 128 is GeoNames at 128 shards. A write of a new value pays N misses.

Warm:

| Bits:hashes | N = 1 | N = 4 | N = 14 | N = 32 | N = 128 |
|---|---|---|---|---|---|
| 0 | 1.7 | 6.6 | 22.9 | 52.2 | 208.5 |
| 6:4 | 1.8 | 2.3 | 3.9 | 6.7 | 21.7 |
| 8:6 | 1.8 | 2.1 | 3.0 | 4.8 | 14.0 |
| 10:7 | 2.0 | 2.2 | 2.9 | 4.3 | 11.4 |
| 12:7 | 1.8 | 2.1 | 2.9 | 4.4 | 12.5 |
| 16:7 | 1.9 | 2.1 | 2.7 | 4.0 | 10.5 |

Entry pages cold, filter resident:

| Bits:hashes | N = 1 | N = 4 | N = 14 | N = 32 | N = 128 |
|---|---|---|---|---|---|
| 0 | 159 | 639 | 2,236 | 5,112 | 20,450 |
| 6:4 | 140 | 165 | 251 | 404 | 1,224 |
| 8:6 | 167 | 179 | 218 | 288 | 664 |
| 10:7 | 172 | 178 | 200 | 238 | 443 |
| 12:7 | 175 | 179 | 191 | 212 | 323 |
| 16:7 | 163 | 164 | 169 | 179 | 229 |

Entry pages and filter cold:

| Bits:hashes | N = 1 | N = 4 | N = 14 | N = 32 | N = 128 |
|---|---|---|---|---|---|
| 0 | 167 | 723 | 2,575 | 5,910 | 23,696 |
| 6:4 | 291 | 752 | 2,288 | 5,053 | 19,801 |
| 8:6 | 317 | 786 | 2,350 | 5,165 | 20,177 |
| 10:7 | 329 | 806 | 2,397 | 5,262 | 20,539 |
| 12:7 | 341 | 846 | 2,527 | 5,555 | 21,700 |
| 16:7 | 303 | 741 | 2,201 | 4,828 | 18,840 |

The same model on the 13.5 M run's medians gives, over 128 runs with the entry pages cold, 20.0 ms
without a filter and 0.37 ms at 10 bits.
