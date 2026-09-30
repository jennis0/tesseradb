# Authorising over a shared label DAG

**Date:** 2026-09-30 · **Harness:** [`labeldag/`](labeldag/) · **Raw:** [`raw/`](raw/) ·
**Machine:** 4 cores of an Intel Xeon @ 2.10GHz (cloud VM, one thread per core, 8 MiB L2 per core),
16 GB of memory, Linux 6.18, rustc 1.94.1, release build with debug off
([`raw/machine.txt`](raw/machine.txt))

[users-and-access.md](../../docs/users-and-access.md) proposes Accumulo visibility expressions as
access labels. Each distinct label gets a label id, each item carries exactly one, the labels are
compiled into one hash-consed DAG, and authorise propagates the credential's terms upwards and
unions the postings of every label whose root became true. Its section on authorising asks for this
measurement before the design is committed.

Nothing here is Tessera code. The probe is a model: its own parser, normaliser, DAG and postings,
depending on no workspace crate. The only shared piece is the Roaring library, `croaring` 2.6.0
(the workspace pins 2.7.0, which needs a newer rustc than this machine has). Every figure below is
measured on that model. Where a statement is modelled rather than measured, it says so.

## Results

Milliseconds per authorise, median / p99, one thread. "Drawn" is 25 credentials drawn to hit
labels, 8 passes each (200 samples). "Worst" is one credential holding the terms that the most
labels mention, 100 passes. Propagate includes looking up the credential's terms. Union builds the
authorised set as a Roaring bitmap. The last two columns answer the same question with the same
result, checked equal on every credential:

- **today's scheme**: one posting per term, and the union of the held terms' postings. It can
  express only labels that are disjunctions of terms, so it runs on corpora B and C.
- **clause postings**: a variant described under [What would send the design back](#c-what-would-send-the-design-back).

| corpus | credential | terms | propagate ms | union ms | total ms | nodes visited | labels true | authorised items | today's scheme, total ms | clause postings, total ms |
|---|---|---|---|---|---|---|---|---|---|---|
| A100k | drawn | 10 | 1.61 / 4.00 | 1.61 / 3.88 | 3.19 / 8.41 | 129,722 | 12,652 | 8,889,801 | n/a | 9.54 / 30.1 |
| A100k | worst | 10 | 3.37 / 4.46 | 2.83 / 3.89 | 6.22 / 8.05 | 172,163 | 34,990 | 31,042,778 | n/a | 24.7 / 33.0 |
| A100k | drawn | 100 | 7.02 / 10.6 | 3.55 / 5.25 | 10.6 / 15.1 | 209,669 | 60,120 | 59,189,735 | n/a | 51.1 / 85.9 |
| A100k | worst | 100 | 8.35 / 12.5 | 3.36 / 4.96 | 11.7 / 17.3 | 224,800 | 76,989 | 80,417,389 | n/a | 76.0 / 109 |
| A100k | drawn | 1000 | 13.2 / 20.5 | 1.86 / 3.08 | 15.1 / 23.0 | 238,993 | 100,000 | 99,999,986 | n/a | 130 / 186 |
| A100k | worst | 1000 | 12.7 / 17.9 | 1.89 / 2.53 | 14.6 / 19.8 | 238,993 | 100,000 | 99,999,986 | n/a | 130 / 168 |
| A500k | drawn | 10 | 9.23 / 18.7 | 10.7 / 28.9 | 19.8 / 46.0 | 531,904 | 39,791 | 6,121,414 | n/a | 40.5 / 106 |
| A500k | worst | 10 | 19.3 / 26.8 | 27.7 / 40.9 | 47.6 / 66.3 | 759,616 | 175,131 | 28,883,494 | n/a | 135 / 172 |
| A500k | drawn | 100 | 41.1 / 63.5 | 26.6 / 42.4 | 68.3 / 100 | 908,156 | 291,300 | 51,963,267 | n/a | 249 / 377 |
| A500k | worst | 100 | 49.2 / 65.0 | 20.5 / 31.3 | 70.0 / 94.6 | 984,336 | 385,872 | 69,859,465 | n/a | 341 / 411 |
| A500k | drawn | 1000 | 84.0 / 109 | 10.7 / 15.3 | 95.0 / 124 | 1,051,393 | 500,000 | 100,001,376 | n/a | 639 / 790 |
| A500k | worst | 1000 | 81.4 / 149 | 10.6 / 20.3 | 92.0 / 170 | 1,051,393 | 500,000 | 100,001,376 | n/a | 604 / 992 |
| B | drawn | 10 | 38.4 / 79.7 | 60.9 / 110 | 102 / 177 | 1,434,131 | 1,434,121 | 1,556,508 | 1.38 / 3.32 | 1.44 / 3.62 |
| B | worst | 10 | 115 / 182 | 121 / 167 | 239 / 344 | 4,090,457 | 4,090,447 | 4,440,532 | 3.12 / 4.30 | 3.21 / 4.64 |
| B | drawn | 100 | 121 / 186 | 134 / 195 | 257 / 365 | 3,525,058 | 3,524,958 | 3,808,847 | 8.95 / 12.4 | 8.96 / 14.5 |
| B | worst | 100 | 260 / 415 | 232 / 304 | 499 / 669 | 6,582,639 | 6,582,539 | 7,107,914 | 18.3 / 25.5 | 18.4 / 25.1 |
| B | drawn | 1000 | 244 / 387 | 186 / 247 | 431 / 615 | 5,275,996 | 5,274,996 | 5,675,116 | 43.2 / 62.3 | 43.5 / 66.1 |
| B | worst | 1000 | 493 / 755 | 304 / 409 | 798 / 1133 | 8,091,704 | 8,090,704 | 8,698,416 | 94.6 / 148 | 95.4 / 155 |
| C | drawn | 10 | 0.000 / 0.001 | 0.011 / 0.025 | 0.012 / 0.026 | 10 | 10 | 12,435,905 | 0.014 / 0.025 | 0.015 / 0.032 |
| C | worst | 10 | 0.001 / 0.001 | 0.023 / 0.037 | 0.023 / 0.038 | 10 | 10 | 24,226,079 | 0.030 / 0.058 | 0.028 / 0.050 |
| C | drawn | 100 | 0.003 / 0.018 | 0.035 / 0.064 | 0.041 / 0.068 | 100 | 100 | 29,419,803 | 0.072 / 0.117 | 0.078 / 0.122 |
| C | worst | 100 | 0.003 / 0.006 | 0.056 / 0.085 | 0.058 / 0.089 | 100 | 100 | 42,905,830 | 0.102 / 0.158 | 0.105 / 0.133 |
| C | drawn | 1000 | 0.067 / 0.193 | 0.196 / 0.355 | 0.269 / 0.439 | 1,000 | 1,000 | 50,721,129 | 1.29 / 3.14 | 1.25 / 3.38 |
| C | worst | 1000 | 0.065 / 0.112 | 0.225 / 0.311 | 0.294 / 0.504 | 1,000 | 1,000 | 61,913,820 | 1.22 / 2.04 | 1.24 / 1.75 |

Size of the DAG and of the indexes, from the same runs. Graph bytes count what authorise reads:
node kinds and heights, children and parents in CSR form, each node's label, each label's root, and
each term's leaf. The hash-consing index is what ingest needs to find an existing node, and
authorise does not read it. Scratch is per concurrent authorise.

| corpus | distinct labels | items | nodes | edges | build, µs per label | graph | B per node | B per label | hash-cons index | scratch | today's postings | clause postings | peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A100k | 100,000 | 100.0M | 238,993 | 964,059 | 2.46 | 11.5 MB | 48.0 | 115 | 8.8 MB | 2.9 MB | n/a | 7.9 MB | 66 MB |
| A500k | 500,000 | 100.0M | 1,051,393 | 4,485,988 | 3.31 | 52.6 MB | 50.0 | 105 | 39.6 MB | 12.6 MB | n/a | 36.7 MB | 308 MB |
| B | 9,326,962 | 10.0M | 10,374,721 | 47,154,049 | 2.22 | 564 MB | 54.4 | 60.5 | 317 MB | 124 MB | 251 MB | 334 MB | 5.4 GB |
| C | 100,000 | 100.0M | 100,000 | 0 | 0.24 | 2.2 MB | 22.0 | 22.0 | 0.5 MB | 1.2 MB | 1.5 MB | 2.3 MB | 54 MB |

Nodes per label, the distinct nodes under a label's root including its leaves:

| corpus | p50 | p90 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| A100k | 9 | 25 | 57 | 66 | 66 |
| A500k | 9 | 25 | 57 | 66 | 66 |
| B | 6 | 9 | 9 | 9 | 9 |
| C | 1 | 1 | 1 | 1 | 1 |

These maxima are set by the generator's shapes, so they say what these corpora need and nothing
about the longest label a real deployment writes.

The cost of writing one long label, median of 9, from [`raw/limit.txt`](raw/limit.txt). Adding is
parse, normalise and intern, into a DAG that already holds 100,000 A-shaped labels. Evaluating is a
top-down evaluation of the label alone, the check a masked write makes.

| shape | operands | nodes | add ms | evaluate µs |
|---|---|---|---|---|
| OR of terms | 256 | 257 | 0.021 | 0.04 |
| OR of terms | 1,024 | 1,025 | 0.091 | 0.12 |
| OR of terms | 16,384 | 16,385 | 3.0 | 0.32 |
| OR of 3-term ANDs sharing two terms | 256 | 515 | 0.79 | 2.4 |
| OR of 3-term ANDs sharing two terms | 1,024 | 2,051 | 15.7 | 8.8 |
| OR of 3-term ANDs sharing two terms | 4,096 | 8,195 | 321 | 36 |
| OR of 3-term ANDs sharing two terms | 16,384 | 32,771 | 8,089 | 142 |

## The corpora

All three are generated from fixed seeds and are the sizes the brief asked for. Nothing was scaled
down. Items are numbered in label-id order, so each label's items are one contiguous run, as a
build that sorts by label id would store them.

- **A, compartmented.** 1,000 terms: 5 classifications, 20 compartments, 600 teams, 200 release
  regions and 175 projects. Labels are drawn from eight shapes, such as
  `cls:1&(team:3|team:40)`, `cls:0&cmp:2&(team:1|team:9)`, `(cls:2&team:5)|(proj:4&rel:7)`, a
  classification with up to 30 release regions, and an OR of up to 20 `cls&team&rel` triples.
  Within each category a term is drawn by Zipf popularity (s = 1). Operands are printed in shuffled
  order, so normalisation does the sorting. 100,000 and 500,000 distinct labels, each carrying a
  Zipf-distributed (s = 1) share of 100 million items. A drawn credential of k terms holds every
  classification up to a random level, k/10 compartments, then teams, regions and projects in the
  ratio 5:3:2 drawn by popularity. At 1,000 terms a credential holds the whole vocabulary.
- **B, per-document sharing.** 10 million items. Each takes a new label of 2 to 8 principals,
  `user:N` with probability 0.7 drawn from 1 million users (Zipf, s = 0.9), otherwise `group:N`
  from 50,000 groups (Zipf, s = 1); 5% of items reuse an earlier item's label. That gives 9,326,962
  distinct labels. A drawn credential is one user and k − 1 groups drawn by popularity.
- **C, single terms.** 100,000 labels that are each one term, over 100 million items (Zipf,
  s = 1). A drawn credential is k terms drawn by the number of items carrying them.

The worst credential of size k holds the k terms that the most labels mention, ties broken by the
number of items those labels carry.

## What is modelled

- The parser accepts the grammar in users-and-access.md: bare terms of `[A-Za-z0-9_-.:/]`, quoted
  terms with `\"` and `\\`, `&` and `|`, brackets required when they mix, no negation. `public`
  is not treated specially. Nesting deeper than 256 brackets is refused.
- Normalisation flattens nested same-operator nodes, sorts and removes duplicate operands, and
  applies absorption in both directions. A unit test checks, on 5,000 random expressions over four
  terms, that normalising changes no decision for any of the 16 credentials and is idempotent.
- The DAG is hash-consed on (operator, sorted child ids). Children are interned before parents, so
  node ids are in topological order. Parents are held in CSR form, built once when the DAG is
  frozen. Not modelled: adding labels to a frozen DAG, which a running service needs. The
  appendable form would cost more per node than the figures above.
- The pass is the one in the design: an OR becomes true on its first true child, an AND when a
  counter reaches its child count. The scratch arrays are stamped with a pass number, so a pass
  never clears them and costs what it visits. On the first three credentials of every row the set
  of true labels is checked against a top-down evaluation of every label, and the authorised set's
  size against the sum of those labels' items.
- The union orders the true label ids through a Roaring bitmap, which was faster than sorting the
  vector (ordering took 15.7 ms against 19.2 ms at 2 million B items, in a smoke run not kept in `raw/`), coalesces
  adjacent runs, adds runs of 32 items or more as ranges and the rest as single values.
- Items ingested after a build are not in label-id order, so their label runs are not contiguous.
  Not modelled.
- One thread, one pass at a time. Other sessions shared the machine: the load average was between
  1.0 and 1.9 when each corpus started. The p99 columns carry that noise. Some medians that must be
  equal, such as today's scheme and clause postings on B, differ by up to 10% between rows.

## What the numbers imply

### (a) Is bottom-up authorise fast enough?

For labels that are single terms it is as fast as today's scheme, and faster at 1,000 terms: on C,
0.27 ms against 1.29 ms, because a label's items are one run and a run is cheaper to add than a
posting is to OR.

For compartmented labels (A) it costs 3 to 15 ms at 100,000 labels and 20 to 95 ms at 500,000,
with p99 up to 170 ms. Today's scheme cannot express these labels, so there is no baseline.
Propagation visits far more nodes than become true. A 10-term credential over A500k visits 531,904
nodes, half the DAG, to find 39,791 true labels. The reason is the conjuncts that most labels
share. Every label carries a classification, so a credential holding `cls:0` touches the AND node
of about a third of all labels, true or not. The cost grows with the DAG, not with the answer:
from A100k to A500k, the drawn 10-term pass visits 4.1 times as many nodes for 0.7 times as many
authorised items.

Whether 20 to 95 ms is acceptable depends on how often authorise runs. It runs once per session,
and again for each session when the background refresh re-evaluates labels. That decision is
Joe's. The figures are single-threaded, and the pass has no parallelism in it.

For per-document labels (B) it is not fast enough. It is 8 to 77 times slower than today's scheme
on the same corpus and the same answer: 102 ms against 1.38 ms for a drawn 10-term credential, 798
ms against 94.6 ms for the worst 1,000-term one. The gap is widest for small credentials. The design says the pass "visits only the labels
that name it", and it does. But in B nearly every label that names a held term is true and carries
one item, so the pass does per-label work (a random read of the parent list, a counter, a queue
entry, a random read of the label's run) once per authorised item. Today's scheme ORs sorted
containers and costs about 1 to 11 ns per item. The pass costs about 65 to 90 ns per item, roughly
half in propagation and half in the union.

A variant that watches each AND only through its rarest child, as boolean expression indexing
does, was also measured on A. It cut nodes visited by up to 3.5 times and did not cut time: A500k
drawn 10 terms visits 154,076 nodes instead of 531,904 and takes 9.51 ms instead of 9.23 ms. The
probe does not explain why; the per-node cost of checking an AND's children and bucketing by height
roughly cancels the saving. It is not worth its complexity on these corpora.

| corpus | credential | terms | counters: propagate ms | counters: nodes visited | watched: propagate ms | watched: nodes visited |
|---|---|---|---|---|---|---|
| A500k | drawn | 10 | 9.23 / 18.7 | 531,904 | 9.51 / 23.0 | 154,076 |
| A500k | worst | 10 | 19.3 / 26.8 | 759,616 | 21.3 / 32.2 | 512,139 |
| A500k | drawn | 100 | 41.1 / 63.5 | 908,156 | 41.1 / 67.1 | 746,923 |
| A500k | worst | 100 | 49.2 / 65.0 | 984,336 | 43.4 / 60.8 | 805,207 |
| A500k | drawn | 1000 | 84.0 / 109 | 1,051,393 | 69.5 / 105 | 1,051,393 |
| A500k | worst | 1000 | 81.4 / 149 | 1,051,393 | 63.5 / 108 | 1,051,393 |

The same comparison for A100k is in [`raw/A100k.txt`](raw/A100k.txt), rows `LW`.

### (b) A node limit per expression

The limit protects the write path, not authorise. A pass costs what it visits across the whole
DAG, and one long label adds its nodes once. Writing a label is where length costs: absorption
compares every pair of operands under an OR or an AND, so it is quadratic. An OR of 1,024 ANDs
(2,051 nodes) takes 15.7 ms to add, 4,096 ANDs take 321 ms and 16,384 take 8.1 s. An OR of plain
terms is close to linear: 16,384 terms take 3.0 ms, because an operand that is a term cannot be
absorbed and is skipped.

The recommendation is **1,024 nodes**. It admits the longest generated label (66 nodes) 15 times
over, and a releasability list of 1,000 regions or principals. Its worst write measured is under
16 ms and its evaluation under 9 µs. A limit of 4,096 would allow a write of about 0.3 s, which
stalls ingest.

If longer labels are needed, making absorption sub-quadratic (grouping operands by their first
element, for instance) would move the limit. Not built in the probe and not measured.

### (c) What would send the design back

**One label id per item makes per-document sharing 8 to 77 times slower to authorise than today,
and uses more memory.** This is the case the design names as the one the DAG helps most. On B the
index is also larger: 564 MB of graph plus 317 MB of hash-consing index, against 251 MB of term
postings today, and each concurrent authorise needs 124 MB of scratch. The disjoint, one-entry-per-item
postings in the design are what force per-label work at authorise.

A variant measured alongside, **clause postings**, removes that cost where it arises. Each item is
posted under every top-level disjunct (clause) of its label, and the pass propagates only up to the
clauses and unions their postings. For a label that is a disjunction of terms, the clauses are the
terms, so on B and C this is today's index exactly and costs the same (B drawn 10 terms: 1.44 ms).
On A it is worse than the design as written: 2 to 9 times slower (A500k drawn 100 terms: 249 ms
against 68.3 ms), because it ORs hundreds of thousands of small bitmaps where the design adds
contiguous runs. Neither design is faster on every corpus.

A hybrid follows from the two: post an item under its terms when its label is a disjunction of
terms (including a single term), and under its label id otherwise. On A, where no label is a pure
disjunction, the hybrid is the design as written. On B and C it is today's index. Its cost on a
corpus that mixes the two shapes is modelled from these figures and was not measured. It gives up
"each item carries exactly one label id" for postings. Label ids would still exist for the item
card, masked writes and compaction.

Two smaller points:

- The pass on A visits roughly half the DAG for any credential that holds a classification term.
  If compartmented corpora grow past a few hundred thousand labels, authorise grows with them,
  whatever the credential. At 500,000 labels it is 20 to 95 ms.
- The hash-consing index costs 30 to 38 bytes per node on A and B, which is 56% of the graph on B. Ingest
  needs it to share nodes, so it cannot be dropped after a build.

## Reproducing

```bash
cd probes/2026-09-30-label-dag-authorise/labeldag
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --release
cargo build --release
for c in A100k A500k C B; do ./target/release/labeldag $c > ../raw/$c.txt; done
./target/release/labeldag limit > ../raw/limit.txt
```

B took 14 minutes and peaked at 5.4 GB, A500k 13 minutes, A100k 3 minutes and C 2 seconds. In the raw tables,
`L` is the design as written, `LW` the same with watched ANDs, `C` clause postings, `CW` clause
postings with watched ANDs, and `T` today's scheme. For `T` the "hits" column counts the credential
terms found in the dictionary.
