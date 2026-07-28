"""Build corpus.parquet — the one canonical base artifact for Phase 0.

Dedup + embeddings join + entity-ID assignment happen here, once. Nothing
downstream reads the raw snapshot or thinks about duplicates again.

  entity_id   u32, dense, 0-based, assigned in (v1_created, id) order over
              the joined set. The permanent entity space.
  id          arXiv id (join key to the embeddings parquet when needed)
  categories  space-separated category string, as supplied
  surnames    list of author surnames from authors_parsed
  v1_created  parsed versions[0].created timestamp

Reproducible from (snapshot version, embeddings snapshot) alone; no seed.

Usage: build_corpus.py <snapshot.json> <embed_ids.parquet> <out.parquet>
"""

import sys

import duckdb

SNAPSHOT, EMBED_IDS, OUT = sys.argv[1], sys.argv[2], sys.argv[3]

con = duckdb.connect()
con.execute(f"""
COPY (
  WITH deduped AS (
    SELECT id, categories,
           list_transform(authors_parsed, a -> a[1]) AS surnames,
           strptime(versions[1].created, '%a, %d %b %Y %H:%M:%S GMT') AS v1_created
    FROM read_ndjson('{SNAPSHOT}',
                     columns={{'id':'VARCHAR', 'categories':'VARCHAR',
                               'authors_parsed':'VARCHAR[][]',
                               'versions':'STRUCT(created VARCHAR)[]',
                               'update_date':'VARCHAR'}})
    QUALIFY row_number() OVER (PARTITION BY id ORDER BY update_date DESC, categories) = 1
  )
  SELECT CAST(row_number() OVER (ORDER BY v1_created, id) - 1 AS UINTEGER) AS entity_id,
         id, categories, surnames, v1_created
  FROM deduped
  SEMI JOIN read_parquet('{EMBED_IDS}') e ON deduped.id = e.paper_id
  ORDER BY entity_id
) TO '{OUT}' (FORMAT PARQUET);
""")

n, n_id, lo, hi = con.execute(
    f"SELECT count(*), count(DISTINCT id), min(entity_id), max(entity_id) FROM read_parquet('{OUT}')"
).fetchone()
assert n == n_id, f"duplicate ids survived dedup: {n} rows, {n_id} distinct"
assert (lo, hi) == (0, n - 1), f"entity_id not dense: [{lo}, {hi}] over {n} rows"
print(f"{OUT}: {n:,} rows, entity_id 0..{hi:,}, ids distinct")
