# What shape should a served artifact have

The family survey behind [`docs/design/artifact-shapes.md`](../../docs/design/artifact-shapes.md).
Most of the figures that document cites are produced here, and this directory is the place to re-run
before quoting one.

**Two of them are not, and deliberately.** The figures a *ruling* rests on are Rust against Rust in
one release-mode process, because a Qhull-in-C against numpy-in-Python column compares
implementations rather than algorithms — which is why this probe could not settle ruling C and said
so. Those live with the engine and are re-run from there:

| | |
|---|---|
| `crates/tessera-engine/tests/hull_triangulation.rs` | **Ruling C**: a Rust Delaunay against the Rust dig, over the same 197 memberships; the χ-peel it would have bought; the grid grouping against exact single-linkage at α |
| `crates/tessera-engine/tests/hull_geometry.rs` | What the **served** shape costs — vertices, rings, wire bytes, area, containment — over any layer of a built bundle |

```bash
TESSERA_HULL_BUNDLE=<bundle root> \
  cargo test --release -p tessera-engine --test hull_triangulation -- --ignored --nocapture
```

**Corpus.** `notebook-2m4`, read from `data/notebook-2m4-live/*.parquet` — `clusters/hdbscan`
(197 artifacts, 6,146 … 2,422,484 distinct member positions), `clusters/kmeans` (64, the convex-ish
control) and `points.parquet` (2,422,484 distinct positions, the denominator for precision).
Positions are quantised exactly as the build quantises, against the extent the built bundle's
`MANIFEST.json` records, so a shape computed here is over the same integer lattice the engine's is.

**Nothing subsamples in the survey.** A shape over a sample is a different object from a shape over
`membership ∩ M_auth` (decision 0099),
so the families that cannot be run over 2.4M members in Python — the k-NN hull and the buffered
union — are drawn and labelled as sampled, never measured. `mask_sweep.py` samples on purpose and
says so.

## The reproduction is checked before anything is compared to it

`shapes.alpha_dig` is a Python reimplementation of `crates/tessera-engine/src/derived.rs`, not a
call into it. `validate.py` runs it over the whole layer and compares against the figures the
engine's own measurement published
([`2026-08-26-concave-hulls.md`](../../docs/evidence/memos/2026-08-26-concave-hulls.md)): 3,278 wrap
vertices, 12,388 shape vertices, area 0.870 median / 0.858 mean / 0.290 minimum, 108 of 197 at the
budget. **All six agree exactly.** The two known differences are stated in `shapes.py`'s module
doc — `float64` where the engine uses `i128`, and a scan where the engine prunes with buckets.

## Running it

```bash
python3 load.py hdbscan kmeans toponymy   # build the position caches (~15 s, 173 MB under cache/)
python3 validate.py                       # the reproduction check above (~40 s)
python3 measure.py hdbscan                # the family survey (~5 min) -> results-hdbscan.json
python3 measure.py kmeans                 # the control (~45 s)
python3 report.py hdbscan                 # the tables -> report-hdbscan.md
python3 overlap.py                        # cross-branch overlap by family (~1 min)
python3 alpha_sweep.py                    # alpha as a parameter (~20 min)
python3 budget_sweep.py                   # the dig's budget against the chi-shape (~3 min)
python3 dp_sweep.py                       # what Douglas-Peucker costs in containment (~15 s)
python3 mask_sweep.py                     # modality under a uniform random mask (~15 s)
python3 knn_scaling.py                    # where the k-NN walk stops terminating (~2 min)
python3 figures.py                        # the rendered candidates (~6 min) -> figures/
```

`pyarrow`, `numpy`, `scipy`, `shapely`, `matplotlib`. `cache/` and `rings-*.pkl` are derived and not
committed; the `results-*.json` files and the `*.md` tables are the measurement record.

## What is in each file

| | |
|---|---|
| `load.py` | Parquet to per-artifact member positions on the engine's 32-bit grid, plus the corpus |
| `shapes.py` | The families. One entry point each, all returning a **list of rings** |
| `validate.py` | The reproduction check against the engine's published figures |
| `measure.py` | The survey: vertices, wire bytes, area, fill, containment, precision, modality |
| `report.py` | The survey rolled into the tables the design cites |
| `overlap.py` | How much of the layer's drawn area is two unrelated artifacts on top of each other |
| `alpha_sweep.py` | Every α-driven family at α ∈ {1, 1.5, 2, 3, 5, 8} × the median wrap edge |
| `budget_sweep.py` | The dig at budgets 64 … 1,024 against the χ-shape, and the Delaunay's own time |
| `dp_sweep.py` | Douglas–Peucker at α/32 … α/2: bytes saved against members left outside |
| `mask_sweep.py` | Whether multi-modality grows as a mask narrows (uniform random masks only) |
| `knn_scaling.py` | At what `k` the Moreira–Santos walk closes, by sample size |
| `figures.py` | The rendered candidates |

## The figures

| | |
|---|---|
| `figures/layer-*.png` | Every artifact of the layer in one family, over the corpus. The picture the *"really ugly"* was about |
| `figures/side-by-side.png` | Six artifacts, three constructions. **The one to look at**: on the large artifacts the shape `main` serves is nearly its convex wrap |
| `figures/families-*.png` | One artifact, every family, with the invented-vertex ones marked |
| `figures/multimodal.png` | The three artifacts whose members are in separated components, and what one ring does with them |
| `figures/alpha-*.png` | One artifact at five values of α, for the dig and the peel |

## Negative results

- **Multi-modality is rare on this layer**: 3 of 197 artifacts have two components holding 5% of
  members at full membership, and 1 has two at 10%. It does not grow under uniform random masks
  down to 0.2% of members. A *correlated* mask is not measured and should not be claimed either way.
  **This measurement answered the wrong question and was overruled** (owner, 2026-08-27): the wire is
  not shaped by one corpus's statistics, and the design carries several rings whatever the frequency
  here. It is left standing because it is true, and because what it does *not* cover — a correlated
  mask, a clustering that is not density-based — is the shape of the argument that overrode it.
- **A Rust triangulation is too expensive to carry**: 1.4–1.5 s for the 2,422,484-member artifact's
  Delaunay alone, against 0.16 s for the whole dig, and 7.6–8.1× over the layer. The Python column in
  this probe suggested the opposite and was refused as evidence for the right reason.
- **No grid can compute single-linkage at α exactly.** Joining occupied cells within a fixed
  neighbourhood is complete only if the neighbourhood's own diameter exceeds α, so it always joins
  members further apart than α; the best such a rule can do is √2·α as the cell shrinks. At the cell
  side the engine uses it agrees with the exact partition on 192 of 197 artifacts and coarsens the
  rest.
- **The flush-flank limitation the engine's memo records is not what binds it.** 13 digs were
  refused over the whole layer; 108 of 197 artifacts stop at the vertex budget with a bridging edge
  still live.
- **α is not a tightness knob for the dig.** Its fill is *worse* at α = 1 (0.846) than at α = 3
  (0.976), because a finer α finds more bridges than 64 vertices can dig.
- **The k-NN concave hull does not reliably terminate here.** On one 16,929-member cluster the walk
  needs `k = 40` at 400 sampled members, `k = 120` at 1,500, and dead-ends at every `k` up to 120 at
  3,000. Whether that is the algorithm or this transcription of it is **not established**.
- **Douglas–Peucker never preserves containment.** Even at α/32 it leaves 0.10% of member rows
  outside their own artifact's shape.
- **The time column comparing the dig against the χ-shape is not evidence.** Qhull is C and this
  dig is a numpy loop. A Rust triangulation against the Rust dig is the measurement that would
  settle it, and it was not taken.
