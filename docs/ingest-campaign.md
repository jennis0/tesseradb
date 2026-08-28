# The ingest campaign — status

**Status:** Working status record, never normative. **This is a status document and is expected to
be edited in place** as rungs land; it is not a dated memo. The plan it executes is
[`evidence/memos/2026-08-27-ingest-campaign-plan.md`](evidence/memos/2026-08-27-ingest-campaign-plan.md),
which is a *plan* and has already been departed from in several places — where the two disagree,
this document records what was actually done and why.

⊘ **This tracker has no pointer in `CLAUDE.md`.** It follows the convention
[`artifact-delivery.md`](artifact-delivery.md) and [`client-delivery.md`](client-delivery.md) use,
both of which are named there by owner direction. Whether the campaign is tracked here or on issues
is the owner's to settle.

**Last updated:** 2026-08-28.

---

## 1. Where the campaign is

| # | Rung | Points | State |
|---|---|---|---|
| — | arXiv | 2,422,486 | **Have, and now on the campaign's convention.** The pipeline that produces it was a notebook outside `test_corpora/`; ported to [`../test_corpora/arxiv/`](../test_corpora/arxiv/README.md) on 2026-08-28 as `prepare.py` plus an optional `toponymy.py`, and the notebook deleted. It is the ladder's only embedding corpus and the only one whose source is derived rather than staged |
| 0 | Re-run the 5×10⁷ artifact tier | — | **Deferred, deliberately.** It confirms W1 and W2, which bite at rung 2 and not at rung 1, and it costs a ~45 GB build. Take it before rung 2, not before rung 1 |
| **1** | **GeoNames** | **13,463,857** | **Built, verified and served.** Not done against §7.1's bar — see §2 |
| 2 | Overture places + divisions | 7.4×10⁷ | Not started. Staged |
| 3 | MedCPT / PubMed | 3.6×10⁷ | Not started. Staged; MeSH is **not** staged and is a prerequisite |
| 4 | PaperSeek + OpenAlex | 1.02×10⁸ | Not started. Staged |
| 5 | TreeOfLife | 2.33×10⁸ | Not started. Staged |
| 6 | GBIF | 3.50×10⁹ | Not started. Staged; needs a second local volume |
| 7 | Overture buildings | 2.53×10⁹ | Not started. Staged; needs a second local volume |

**All eight datasets are staged** at `/mnt/nas/joe/tessera/datasets/<name>/<vintage>/`, 2.5 TB, each
with a README stating what was verified at acquisition and what is the publisher's claim.

**`data/` is already mirrored** to `arxiv-tessera/2026-07-27/`, so the plan's §5 cleanup is a
verification rather than a copy. It has **not** been verified and nothing has been deleted; there is
no space pressure at rung 1 (117 GB free, GeoNames needs ~5 GB end to end).

**The second volume (plan §4) is not built.** It is rung 6/7 work and nothing before then needs it.

## 2. Rung 1 — GeoNames, against §7.1's bar

The plan's bar for *done* is six things. Two are met.

| | |
|---|---|
| ✅ declaration passes `tessera check` | 6 sources, 1 view, 8 vocabularies, 13 attributes, 2 layers |
| ✅ bundle exists, frame report recorded | 1,329,553,710 bytes; report in `frame.json` and the rung README |
| ❌ decision 0091's build-vs-ingest test on real data | not attempted |
| ❌ masked-count census exact against an oracle | not attempted |
| ❌ one full write cycle (suppress → delete → re-ingest → fold → re-census) | not attempted |
| ⚠️ a results row | build wall, peak RSS and bundle bytes yes; **ingest rows/s, p99 at three zooms and a screenshot all absent** |

**Figures so far**, local NVMe, 47 GB machine, no `--memory-budget` set:

```
prepare.py       ~2 min          tessera build   6:05 wall, 4.2 GB peak RSS
bundle           1.33 GB         verify          0.94 s
                 98.7 B/point    artifacts       465,343 minted
resolution       85.7% of points have a cell of their own
```

Neither wall the plan expects — W1's Roaring round trip at 5×10⁷ members, W2's peak RSS ignoring
its budget — is near being reached at this scale.

**What the rung is served by:** [`../test_corpora/geonames/`](../test_corpora/geonames/README.md),
which carries the preprocessing, the declaration and the full account of what the source turned out
to be.

## 3. The machinery this campaign built

- **[`../test_corpora/`](../test_corpora/README.md)** — one directory per rung, in git: `prepare.py`,
  `corpus.toml`, `README.md`. Derived files go to `$TESSERA_LADDER/<rung>` (default
  `data/ladder/<rung>`), so the second volume is one environment variable rather than an edit to
  every script.
- **`test_corpora/common/projection.py`** — the frozen WGS84 → Web Mercator transform, unit square,
  **y south**. Checked against published values, against XYZ tile addresses (the only real test of
  the y direction), and against DuckDB, which agrees bit-for-bit. Its `TEST_VECTORS` are written as
  data so the eventual Rust can be checked against them.
- **`~/venvs/ingest`** — DuckDB and PyArrow. `spatial` waits until rung 2's point-in-polygon join.
- **`run_demo.sh --terms / --ranks / --label`**, and `custom` on ports of its own — see §5.

## 4. Cross-cutting findings

Ordered by how much they matter beyond this rung.

**A tiered layer returns every level whatever the zoom, and at the opening view that is 49 MB.**
464,655 artifacts and 2.9 s per viewport request for a broad principal at zoom 0, against 4 KiB and
67 ms for the points alone. The two bounds that work — the mask and the tile index — both bound
*which artifacts are in range*; neither bounds *which levels the client wanted*, and at whole-world
zoom 0 nothing is out of range, so the level is the only axis left and it is the one a request
cannot name. The corpus already declares the answer: its zoom→level map says level 0 alone applies
at zoom 0, 254 artifacts against 464,655 served. The full account, and the questions a design pass
has to answer, are in
[`evidence/memos/2026-08-28-artifact-response-volume.md`](evidence/memos/2026-08-28-artifact-response-volume.md).
**This is the campaign's first real finding and it arrived at rung 1**, on the serving side, where
the plan expected its first walls at rung 2 on the build side.

**A geographic corpus is reproducible, and an embedding corpus is not.** A projection is a pure
function, so a geographic rung built on a frame that later changes costs a rerun rather than the loss
`data/geometry.parquet` would be. This is why rung 1 did not wait on native projection, and it does
not transfer to rungs 3–5.

**Declare a width from a measured range, never from a maximum.** `population` was declared `u64`
from a census that measured only the maximum; the build refused on a **-12** two reefs in Kiribati
carry. `prepare.py` now prints every numeric's full range for exactly this reason. The build
refusing rather than truncating is the system behaving correctly, and it is a slow way to learn it.

**`default = "public"` makes unlabelled rows universally visible, and their attribute values leak
into every principal's derived listing.** GeoNames' 6,997 blank-country rows are public by
declaration, so their `admin1` values appear for every principal — 32 of GB's 37. Correct given the
declaration, and worth deciding deliberately at each rung rather than inheriting.

**A published hierarchy's codes are only unique within their parent.** GeoNames' `admin1` has 823
distinct codes standing for 4,823 real regions; keying on the bare code would merge Scotland with a
Brazilian state. Qualify by the full path. Expect the same at Overture, GBIF and MeSH.

**`parent_edges` conflates two different nulls.** For a clustering, a null entry means *noise at
this resolution* and reading across it would state a containment no row makes — which is why
`parent_edges` is `windows(2)`. For a gazetteer it means *no code was recorded*, and the containment
is not in doubt. GeoNames is the first corpus where the two come apart, and the surface has one
spelling for both. Routed around here by materialising the hole as an explicit artifact (1,373 of
them, against 464,000 real); **not raised as an issue and not designed**.

## 5. Problems found in tooling, and what was done

**`run_demo.sh` reported a scale ready when another process held the port.** The readiness poll asks
the *port*, not the process it started, so a stale server answered, the script declared success, and
everything downstream talked to a different bundle — surfacing as "no candidate term is visible to
anyone", which names neither the port nor the cause. **Fixed**: a port already in use is now a
refusal that says so.

**`--bundle` could not serve any corpus with its own dictionary.** It hardcoded the demo fixtures'
synthetic `0..200` terms, so every principal measured empty. **Fixed**: `--terms`, `--ranks` and
`--label`, and `custom` now has ports of its own rather than sharing `2m4`'s.

**The projections work was committed under an unrelated message.** HEAD moved from `37e43a8` to
`b1cb81b` mid-session and the edits to [`design/projections.md`](design/projections.md) were swept
into `dd195c9`, a commit about the rings track. Content intact, provenance misleading.

**`test_corpora/` is untracked** and needs its own commit.

## 6. Open, and what is next

**Owner calls outstanding**

- The design pass on artifact response volume (§4, and the memo it points at).
- Whether `parent_edges`' two nulls need separating, and whether that is worth an issue.
- Whether this tracker is the campaign's status record or the campaign moves to issues.

**Rung 1 work not done**

- `places/containment` — the third layer, from `hierarchy.txt`, as a `nested` lineage. Needs a DAG
  walk and will meet genuine multiple parents, which is the polyhierarchy refusal for real rather
  than as the keying artefact rung 1 already dissolved.
- Everything in §2's ❌ rows: the 0091 test, the oracle census, the write cycle, p99 and a screenshot.

**Before rung 2**

- Rung 0, to confirm W1 and W2 reproduce and whether the pre-flight now refuses rather than being
  killed.
- The `spatial` extension, for the `division_area` point-in-polygon join.
- A decision on whether the artifact volume finding blocks a rung whose boundary layer is larger
  still.
