"""Phase 0 pre-flight: join probe between the two arXiv sources.

Sources
  A. AliMaatouk/arXiv-Topics-Embeddings (HF) — 2,422,486 paper_ids, one BGE
     embedding each. Only the paper_id column is consulted here.
  B. arxiv-metadata-oai-snapshot.json (Kaggle Cornell-University/arxiv) —
     the metadata snapshot: id, categories, versions[...].

Questions this answers, in order of consequence for the generator seam:
  1. Do the two sources format old-style IDs the same way (subject-class
     suffix `math.GT/9906001` vs bare archive `math/9906001`)?
  2. What fraction of embedding IDs join to the snapshot, before and after
     any normalisation the answer to (1) requires?
  3. What misses remain, and are they explainable (withdrawn, newer than the
     embeddings snapshot) rather than symptomatic?
  4. Does the joined subset preserve the category skew §3 depends on?

Stdout is the deliverable; nothing here is serving code.
"""

import sys
from pathlib import Path

import duckdb

EMBED_IDS = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("embed_paper_ids.parquet")
SNAPSHOT = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("arxiv-metadata-oai-snapshot.json")

OLD_STYLE = r"^[a-z-]+(\.[A-Za-z-]+)?/[0-9]{7}$"
NEW_4 = r"^[0-9]{4}\.[0-9]{4}$"
NEW_5 = r"^[0-9]{4}\.[0-9]{5}$"

con = duckdb.connect()
con.execute(f"""
CREATE VIEW embeds AS SELECT paper_id FROM read_parquet('{EMBED_IDS}');
CREATE TABLE meta AS
SELECT id,
       categories,
       len(versions)                    AS n_versions,
       versions[1].created              AS v1_created
FROM read_ndjson('{SNAPSHOT}',
                 columns={{'id': 'VARCHAR', 'categories': 'VARCHAR',
                           'versions': 'STRUCT(created VARCHAR)[]'}});
""")


def section(title):
    print(f"\n{'=' * 70}\n{title}\n{'=' * 70}")


def style_breakdown(table, col):
    return con.execute(f"""
        SELECT CASE
                 WHEN regexp_matches({col}, '{NEW_5}') THEN 'new-5digit'
                 WHEN regexp_matches({col}, '{NEW_4}') THEN 'new-4digit'
                 WHEN regexp_matches({col}, '{OLD_STYLE}') THEN 'old-style'
                 ELSE 'OTHER'
               END AS style, count(*) AS n
        FROM {table} GROUP BY 1 ORDER BY n DESC
    """).fetchall()


section("1. Row counts and ID-style breakdown per source")
n_embed = con.execute("SELECT count(*), count(DISTINCT paper_id) FROM embeds").fetchone()
n_meta = con.execute("SELECT count(*), count(DISTINCT id) FROM meta").fetchone()
print(f"embeddings : {n_embed[0]:>10,} rows, {n_embed[1]:>10,} distinct")
print(f"snapshot   : {n_meta[0]:>10,} rows, {n_meta[1]:>10,} distinct")
for src, col in (("embeds", "paper_id"), ("meta", "id")):
    print(f"\n  {src}:")
    for style, n in style_breakdown(src, col):
        print(f"    {style:<12} {n:>10,}")
    if src == "meta":
        others = con.execute(f"""
            SELECT id FROM meta
            WHERE NOT regexp_matches(id, '{NEW_4}')
              AND NOT regexp_matches(id, '{NEW_5}')
              AND NOT regexp_matches(id, '{OLD_STYLE}') LIMIT 10
        """).fetchall()
        if others:
            print("    OTHER samples:", [r[0] for r in others])

section("2. Old-style formatting: subject-class suffix present?")
for src, col in (("embeds", "paper_id"), ("meta", "id")):
    with_class, bare = con.execute(f"""
        SELECT count(*) FILTER (WHERE regexp_matches({col}, '^[a-z-]+\\.[A-Za-z-]+/')),
               count(*) FILTER (WHERE {col} LIKE '%/%'
                                AND NOT regexp_matches({col}, '^[a-z-]+\\.[A-Za-z-]+/'))
        FROM {src}
    """).fetchone()
    print(f"  {src:<8} archive.CLASS/nnnnnnn: {with_class:>9,}   bare archive/nnnnnnn: {bare:>9,}")

section("3. Join, raw IDs (no normalisation)")
raw = con.execute("""
    SELECT count(*) FROM embeds e JOIN meta m ON e.paper_id = m.id
""").fetchone()[0]
print(f"  matched: {raw:,} / {n_embed[0]:,}  ({100 * raw / n_embed[0]:.3f}% of embeddings)")

# Normalisation candidate: strip subject class from old-style metadata IDs
# (math.GT/9906001 -> math/9906001) if section 2 shows the sources disagree.
section("4. Join with subject-class-stripped snapshot IDs")
norm = con.execute(r"""
    SELECT count(*) FROM embeds e
    JOIN (SELECT regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid FROM meta) m
      ON e.paper_id = m.nid
""").fetchone()[0]
print(f"  matched: {norm:,} / {n_embed[0]:,}  ({100 * norm / n_embed[0]:.3f}% of embeddings)")

# Collision check: does stripping the class ever merge two snapshot IDs?
collisions = con.execute(r"""
    SELECT count(*) FROM (
      SELECT regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid
      FROM meta GROUP BY 1 HAVING count(*) > 1)
""").fetchone()[0]
print(f"  normalised-ID collisions inside snapshot: {collisions:,}")

section("5. Misses: embedding IDs absent from the (normalised) snapshot")
con.execute(r"""
    CREATE TABLE misses AS
    SELECT e.paper_id FROM embeds e
    LEFT JOIN (SELECT regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid FROM meta) m
      ON e.paper_id = m.nid
    WHERE m.nid IS NULL
""")
n_miss = con.execute("SELECT count(*) FROM misses").fetchone()[0]
print(f"  {n_miss:,} embedding IDs unmatched ({100 * n_miss / n_embed[0]:.4f}%)")
for style, n in style_breakdown("misses", "paper_id"):
    print(f"    {style:<12} {n:>8,}")
print("  samples:", [r[0] for r in con.execute("SELECT paper_id FROM misses LIMIT 15").fetchall()])

section("6. Reverse direction: snapshot IDs without an embedding, by year")
print("  (expected: bulk of misses are papers newer than the embeddings snapshot)")
rows = con.execute(r"""
    WITH m AS (
      SELECT id, regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid,
             CASE
               WHEN id LIKE '%/%' THEN
                 CASE WHEN CAST(substr(split_part(id, '/', 2), 1, 2) AS INT) > 50
                      THEN 1900 + CAST(substr(split_part(id, '/', 2), 1, 2) AS INT)
                      ELSE 2000 + CAST(substr(split_part(id, '/', 2), 1, 2) AS INT) END
               ELSE 2000 + CAST(substr(id, 1, 2) AS INT)
             END AS yr
      FROM meta)
    SELECT yr, count(*) AS total,
           count(*) FILTER (WHERE e.paper_id IS NULL) AS unmatched
    FROM m LEFT JOIN embeds e ON m.nid = e.paper_id
    GROUP BY yr ORDER BY yr
""").fetchall()
for yr, total, unmatched in rows:
    print(f"    {yr}: {total:>9,} papers, {unmatched:>9,} without embedding ({100 * unmatched / total:5.1f}%)")

section("7. Category skew on the joined subset (top 20 primary categories)")
rows = con.execute(r"""
    WITH m AS (
      SELECT split_part(categories, ' ', 1) AS primary_cat,
             regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid
      FROM meta)
    SELECT primary_cat, count(*) AS joined_n
    FROM m JOIN embeds e ON m.nid = e.paper_id
    GROUP BY 1 ORDER BY joined_n DESC LIMIT 20
""").fetchall()
joined_total = norm
for cat, n in rows:
    print(f"    {cat:<22} {n:>9,}  ({100 * n / joined_total:5.2f}%)")
n_cats = con.execute(r"""
    WITH m AS (SELECT split_part(categories, ' ', 1) AS c,
               regexp_replace(id, '^([a-z-]+)\.[A-Za-z-]+/', '\1/') AS nid FROM meta)
    SELECT count(DISTINCT c) FROM m JOIN embeds e ON m.nid = e.paper_id
""").fetchone()[0]
print(f"    distinct primary categories in joined set: {n_cats}")

section("8. versions[0].created sanity (entity-ID assignment order)")
rows = con.execute("""
    SELECT count(*) FILTER (WHERE v1_created IS NULL) AS null_created,
           count(*) FILTER (WHERE n_versions = 0) AS no_versions,
           min(strptime(v1_created, '%a, %d %b %Y %H:%M:%S GMT')) AS earliest,
           max(strptime(v1_created, '%a, %d %b %Y %H:%M:%S GMT')) AS latest
    FROM meta
""").fetchone()
print(f"  null v1.created: {rows[0]:,}   zero-version rows: {rows[1]:,}")
print(f"  v1.created range: {rows[2]} .. {rows[3]}")
dupes = con.execute("""
    SELECT count(*) FROM (SELECT id FROM meta GROUP BY id HAVING count(*) > 1)
""").fetchone()[0]
print(f"  duplicate snapshot ids: {dupes:,}")
