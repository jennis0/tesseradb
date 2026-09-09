"""Where rung 6's one staged source is, and the constants the pipeline shares.

**GBIF occurrence, vintage 2026-06-01.** 8,369 parts under `occurrence.parquet/`, named `NNNNNN`
with no suffix, **one row group apiece** and between 26,717 and 1,039,405 rows each —
3,654,488,638 rows, 258.5 GiB compressed over 50 columns
(`probes/2026-09-09-gbif-census/README.md`, measured).

A part is the unit of work because it is one row group: a reader takes it whole or not at all.
Parts are read in name order, so an entity id is a function of the part index and of the rows the
run kept, and a prefix of the parts is a prefix of the entity space.

Rung 5 joins TreeOfLife against these same parts and reads them through `parts()` below, so the
directory listing that decides part order is written once.
"""

from __future__ import annotations

from pathlib import Path

from ..common.paths import ladder, staged

RUNG = "gbif"

DATASET, VINTAGE = "gbif", "2026-06-01"

#: Measured at the census, and asserted against the footers at every whole-corpus run.
PART_COUNT = 8_369
TOTAL_ROWS = 3_654_488_638

#: The three taxonomic ranks the tiered layer walks. **It starts at family**, not at kingdom:
#: `merge_member_runs` (`crates/tessera-build/src/layers.rs`) holds the largest single artifact's
#: members resident as `u64` while it sorts them, and kingdom Animalia is 2,809,414,577 members —
#: 22.5 GB on a 47 GB box, against Anatidae's 177,354,694 and 1.4 GB (owner ruling, 2026-09-09).
#: `kingdom` rides as a rendered category column instead.
RANKS = ["family", "genus", "species"]

#: What `prepare.py` projects off the share. Ten of fifty columns, and the corpus is read once:
#: the census measured 200 parts with eight columns in 70.7 s at eight threads, so a whole-corpus
#: pass of this width is on the order of an hour (modelled from that rate).
#:
#: ⊘ **`gbifid` and `occurrenceid` are not here.** Both are unique per row at 3.65×10⁹, which is a
#: ~100 GB keyword dictionary and the pathology `probes/2026-09-08-keyword-spill/` was written
#: about. ⊘ **`locality` is not here** either: 48.45 GiB compressed, the corpus's largest column,
#: and the text index over it is deferred past the first build.
COLUMNS = [
    "kingdom",
    *RANKS,
    "scientificname",
    "specieskey",
    "year",
    "countrycode",
    "decimallatitude",
    "decimallongitude",
]


def parts() -> list[Path]:
    """The 8,369 parts, in name order. Refuses an empty directory rather than reading nothing."""
    base = staged(DATASET, VINTAGE) / "occurrence.parquet"
    found = sorted(p for p in base.iterdir() if p.is_file())
    assert found, f"no parts under {base}"
    return found


def rung() -> Path:
    return ladder(RUNG)
