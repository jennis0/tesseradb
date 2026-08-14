"""Extract each corpus item's title and abstract — the two prose columns the demo searches.

`corpus.parquet` carries the metadata the geometry pipeline needed (categories, surnames,
timestamp) and no prose, because nothing before the text family could index any. This joins the
snapshot's `title` and `abstract` back onto the entity space that file fixed, so a `text` column
can be declared over real writing rather than over generated strings — which is the only kind of
input that tells you anything about an analyser, a term distribution or a posting's shape.

**The join is by `id`, and it is total.** `probes/dataset.md` §1 records that all 2,422,486
embedding ids match a snapshot id verbatim; the assert here is that fact restated as a check,
because a partial join would silently give some fraction of the corpus an empty title and read as
a recall bug in whatever measured it later.

**Whitespace is normalised and nothing else is.** The snapshot wraps both fields at ~80 columns
with embedded newlines, and an abstract additionally opens with two spaces. Runs of whitespace
collapse to one; the text is otherwise carried verbatim, LaTeX and all — the analyser's handling
of `$\\alpha$` is a real property of this corpus and not something to launder here.

Usage:
    reference/.venv/bin/python probes/build_prose.py \\
        ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \\
        data/corpus.parquet data/demo/prose.parquet
"""

import sys
import time
from pathlib import Path

import duckdb

SNAPSHOT, CORPUS, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
Path(OUT).parent.mkdir(parents=True, exist_ok=True)

t0 = time.time()
con = duckdb.connect()
con.execute(f"""
COPY (
  WITH prose AS (
    SELECT id,
           regexp_replace(trim(title),    '\\s+', ' ', 'g') AS title,
           regexp_replace(trim(abstract), '\\s+', ' ', 'g') AS abstract,
           row_number() OVER (PARTITION BY id ORDER BY update_date DESC, categories) AS rn
    FROM read_ndjson('{SNAPSHOT}',
                     columns={{'id':'VARCHAR', 'title':'VARCHAR', 'abstract':'VARCHAR',
                               'categories':'VARCHAR', 'update_date':'VARCHAR'}})
  )
  SELECT c.entity_id, prose.title, prose.abstract
  FROM read_parquet('{CORPUS}') c
  JOIN prose ON c.id = prose.id AND prose.rn = 1
  ORDER BY c.entity_id
) TO '{OUT}' (FORMAT PARQUET, COMPRESSION ZSTD);
""")

# The dedup rule is `build_corpus.py`'s, so the row count must land on the corpus's exactly. A
# short join means some entity would carry no prose at all, which the demo would render as a
# search that quietly cannot find things.
n_out, lo, hi, empty_title, empty_abstract = con.execute(f"""
    SELECT count(*), min(entity_id), max(entity_id),
           count(*) FILTER (title IS NULL OR title = ''),
           count(*) FILTER (abstract IS NULL OR abstract = '')
    FROM read_parquet('{OUT}')""").fetchone()
n_corpus = con.execute(f"SELECT count(*) FROM read_parquet('{CORPUS}')").fetchone()[0]
assert n_out == n_corpus, f"join dropped rows: {n_out:,} of {n_corpus:,}"
assert (lo, hi) == (0, n_out - 1), f"entity_id not dense: [{lo}, {hi}] over {n_out:,} rows"

chars = con.execute(f"""
    SELECT avg(length(title)), avg(length(abstract)) FROM read_parquet('{OUT}')""").fetchone()
print(f"{OUT}: {n_out:,} rows in {time.time() - t0:.0f}s")
print(f"  mean title {chars[0]:.0f} chars, mean abstract {chars[1]:.0f} chars")
print(f"  empty: {empty_title:,} titles, {empty_abstract:,} abstracts")
