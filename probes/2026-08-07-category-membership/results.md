# Category membership sets — cost and contiguity

**Date:** 2026-08-07 · **Harness:** `cargo run --release -p tessera-bench --bin category_membership -- <bundle> <column>`

## The result

**An entity-space membership set per category value costs about what the render column it indexes
costs, and a full legend evaluates in under a millisecond.** Across the contiguity spectrum the
artefact ranges from **0.31× to 1.01×** the hot column's bytes, and the whole 171-value
per-principal evaluation runs in **0.013–1.10 ms**. Nothing here argues against building it.

This settles the open question in the vocabulary-visibility design: whether per-value membership
in entity space is a modest artefact or a serious one. It is modest, and it is modest *even in
the deliberately adversarial case*.

| | `categories-subclass` | `hash-flat` (orthogonal control) |
|---|---|---|
| containers, all values | 3,008 | 6,162 |
| run-optimised size | 1.44 MiB | 4.67 MiB |
| **× the render column** | **0.31×** | **1.01×** |
| mean members/container | 805 | 393 |
| legend evaluation | 0.057–1.10 ms | 0.013–0.60 ms |

Both bundles are the 2,422,486-item arXiv corpus with the same five-column attribute tail
(`probes/build_attributes.py`), differing only in label set.

## Why the two label sets bracket the answer

The cost model is that bitmap operations cost O(containers touched), not O(cardinality), so the
question is how contiguous a category's members are in **entity** space. Entity ids are assigned
in signature-sorted order (§11.1) — by *auth labels* — so a category is contiguous only insofar as
it correlates with the label set. That correlation is a property of the deployment, not of the
design, which is why one measurement would not have been enough.

- **`categories-subclass` is the favourable end, and unusually so.** Its terms are derived from
  arXiv categories, and `primary_category` is the first arXiv category — so the sort key and the
  attribute are near-duplicates of each other. `hep-th` packs 99,877 members into **3** containers
  (33,292 per container against a 65,536 maximum). No real deployment should expect this.
- **`hash-flat` is the designed orthogonal control** — 10,000 all-global terms, maximal scatter,
  chosen by `probes/dataset.md` §4.4 precisely to decorrelate. Membership doubles its container
  count and triples its size.

**That factor of three is the whole spread**, and it lands at parity with the render column rather
than at some multiple of it. A deployment whose categories are unrelated to its access labels pays
about one extra render column; one whose categories *are* its access structure pays a third of
one.

## §3.3's sparse-principal prediction: supported, not confirmed

The design predicts the intersection is cheapest exactly where the principal is least privileged —
the opposite of I7's usual asymmetry — and says so as a prediction rather than a measurement.

On `categories-subclass`, where masks span 16 to 2,152,994 entities, it holds:

| principal | mask cardinality | visible values | evaluate |
|---|---|---|---|
| tail w=1 | 16 | 6/171 | 0.057 ms |
| tail w=8 | 760 | 61/171 | 0.189 ms |
| head w=1 | 181,900 | 146/171 | 0.634 ms |
| head w=64 | 2,152,994 | 171/171 | 0.700 ms |

A principal seeing 16 items evaluates the legend 11× faster than one seeing everything.

**One row contradicts the naive reading, and it is the cost model asserting itself.** `tail w=64`
costs 1.104 ms against `head w=64`'s 0.700 ms despite a mask **15× smaller** (146,784 against
2,152,994). Sixty-four narrow terms are scattered across many containers; sixty-four wide ones are
dense and contiguous. Cost tracks containers, not cardinality — so "sparse principals are cheapest"
is true of *coverage* and false of *term count*, and a deployment granting many narrow terms is the
expensive shape.

**NOT tested on `hash-flat`** — all its masks are tiny (190 to 18,361) because its terms are
uniform, so head and tail barely differ there (0.013 ms against 0.022 ms) and that run says
nothing either way about the prediction. Do not read it as a second confirmation.

## What is measured, and what is not

**Measured:** everything above, at 2,422,486 items, on this machine, against the two bundles named.

**Modelled, not measured — the 10⁹ extrapolation.** No attribute fixture exists above 2.4M
(`build_attributes.py` covers replica 0 only, the one scale whose attributes are real). If the
measured ratio held, membership at 10⁹ with a `u16` column would be roughly 0.6–2 GB against the
column's ~2 GB. **The ratio is not known to hold**: containers scale with entity *span* while
members scale with count, so a corpus 413× larger is not simply 413× these figures. Building a
10⁹ attribute fixture is what would settle it, and nothing here should be quoted as if it had.

**Not measured — the composed verdict.** §3.3 requires visibility to be evaluated against the
fragment *with the overlay applied*, because a suppression never touches postings (Rule S) and a
fragment alone still contains suppressed items. This harness unions raw postings: the cost shape,
not the correctness shape. Composition adds an `andnot` per generation rather than per value, so it
should not move these numbers — but that is an argument, not a measurement, and the figures here
are a floor.

**Not measured — build and fold cost.** These sets were built in 0.14–0.28 s from an existing
bundle, which says nothing about what emitting them costs inside a build or a fold, where they
would join the postings the fold already rewrites (decision 0050).

## Consequences for the design

- The artefact is affordable at 2.4M across the contiguity spectrum. The visibility design can
  proceed on measured cost rather than on the assumption the memo had to make.
- **No attribute dictionary is needed for a category.** These sets are keyed by the code the
  system already assigned, so nothing caller-supplied is interned and §3.5's collision hazard —
  an attribute descriptor byte-equal to a satisfied auth descriptor — does not arise. A separate
  postings *file* is still wanted, for the mundane reason that the fragment builder must never
  read these.
- A masked count is the same `and_cardinality` these timings already measure, so deferring
  per-viewport counts to the filter contract (#43) costs nothing later — and argues against
  building a cheaper visibility-only structure that would be thrown away.
- The expensive principal shape is **many narrow terms**, not a wide mask. Worth carrying into
  §8's authorise arm, which sweeps grant shape for the fragment path and would otherwise not
  think to sweep it here.

## Reproducing

```bash
reference/.venv/bin/python probes/build_attributes.py --data=<repo>/data
# then, per label set:
tessera build --points data/scaled/attrs/points.parquet \
              --pairs  data/scaled/pairs/<set>.pairs.parquet \
              --schema data/scaled/attrs/schema.toml \
              --values archive=data/scaled/attrs/archive.parquet \
              --values primary_category=data/scaled/attrs/primary_category.parquet \
              --extent 0,65536,0,65536 --view s0 --limit 2422486 \
              --mint-external-ids --id-key 000102030405060708090a0b0c0d0e0f --idset 1 \
              --out /tmp/tessera-attrs-2m4-<set>
cargo run --release -p tessera-bench --bin category_membership -- /tmp/tessera-attrs-2m4-<set>
```
