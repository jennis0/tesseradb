"""Label-set generator: config -> exploded (entity_id, term_id) pair relation.

Entity space only, reads corpus.parquet only. A label set is (seed, config)
and is regenerated, never versioned. Output per run:

  <out>/<name>.pairs.parquet   (entity_id UINTEGER, term_id UINTEGER)
  <out>/<name>.terms.parquet   (term_id UINTEGER, descriptor VARCHAR)

term_ids are dense, 0-based, assigned by descriptor sort — deterministic
for a given (corpus, config, seed). Randomness comes only from blake2b
keyed on the seed (hash config) or numpy default_rng(seed) (noise config),
never from engine-internal hashing.

Also prints the Stage-1 measurement-1 profile (pair counts, terms/item,
posting sizes), the signature count, and the free column: posting run
lengths in entity order.

Configs:
  categories --level subclass|archive
  surnames
  hash       --dist flat|zipf --vocab V [--zipf-s S] [--cross K]
             one term per item; --cross K mints synthetic terms
             (zipf bucket x orthogonal flat bucket), vocabulary up to V*K
             [--terms-per-item T] replicates each item's bucket into T
             terms (T routes to the same item — the minted-DNF shape);
             vocabulary V*T, pairs N*T. Head grants then hit redundancy
             r ~ T, random grants r ~ 1-2: the item-side inflation test
  noise      --level subclass|archive --epsilon E      categories +- one random term
"""

import argparse
from hashlib import blake2b
from pathlib import Path

import duckdb
import numpy as np
import polars as pl

ap = argparse.ArgumentParser()
ap.add_argument("corpus")
ap.add_argument("out")
ap.add_argument("--config", required=True, choices=["categories", "surnames", "hash", "noise"])
ap.add_argument("--level", default="subclass", choices=["subclass", "archive"])
ap.add_argument("--dist", default="flat", choices=["flat", "zipf"])
ap.add_argument("--vocab", type=int, default=10_000)
ap.add_argument("--zipf-s", type=float, default=1.4)
ap.add_argument("--cross", type=int, default=0, metavar="K")
ap.add_argument("--terms-per-item", type=int, default=1, metavar="T")
ap.add_argument("--epsilon", type=float, default=0.1)
ap.add_argument("--seed", type=int, default=0)
args = ap.parse_args()

con = duckdb.connect()


def categories_raw(level):
    descr = "cat" if level == "subclass" else "split_part(cat, '.', 1)"
    con.execute(f"""
    CREATE OR REPLACE TABLE raw AS
    SELECT DISTINCT entity_id, {descr} AS descriptor
    FROM (SELECT entity_id, unnest(string_split(categories, ' ')) AS cat
          FROM read_parquet('{args.corpus}'));
    """)


if args.config == "categories":
    name = f"categories-{args.level}"
    categories_raw(args.level)

elif args.config == "surnames":
    name = "surnames"
    con.execute(f"""
    CREATE TABLE raw AS
    SELECT DISTINCT entity_id, s AS descriptor
    FROM (SELECT entity_id, unnest(surnames) AS s FROM read_parquet('{args.corpus}'))
    WHERE s <> '';
    """)

elif args.config == "hash":
    tag = "flat" if args.dist == "flat" else f"zipf{args.zipf_s}"
    xtag = f"x{args.cross}" if args.cross else ""
    name = f"hash-{tag}-v{args.vocab}{xtag}-seed{args.seed}"
    rows = con.execute(
        f"SELECT entity_id, id FROM read_parquet('{args.corpus}') ORDER BY entity_id").fetchall()
    key = args.seed.to_bytes(8, "little")
    h = np.array([int.from_bytes(blake2b(i.encode(), digest_size=16, key=key).digest(), "big")
                  for _, i in rows], dtype=object)
    u1 = np.array([int(x >> 64) for x in h], dtype=np.float64) / 2.0**64
    u2 = np.array([int(x & (2**64 - 1)) for x in h], dtype=np.float64) / 2.0**64
    if args.dist == "flat":
        k = (u1 * args.vocab).astype(np.int64)
    else:
        w = 1.0 / np.arange(1, args.vocab + 1) ** args.zipf_s
        cdf = np.cumsum(w / w.sum())
        k = np.searchsorted(cdf, u1, side="right")
    width = len(str(args.vocab - 1))
    T = args.terms_per_item
    if T > 1:
        # replica structure built in SQL — term_id = bucket*T + replica; no
        # 10^8-element Python string lists. Shared tail skipped via `direct`.
        name = f"hash-{tag}-v{args.vocab}-t{T}-seed{args.seed}"
        bdf = pl.DataFrame({"entity_id": np.array([e for e, _ in rows], dtype=np.uint32),
                            "b": k.astype(np.uint32)})
        con.register("bdf", bdf)
        con.execute(f"""
        CREATE TABLE pairs AS
        SELECT entity_id, CAST(b * {T} + r AS UINTEGER) AS term_id
        FROM bdf CROSS JOIN range({T}) t(r) ORDER BY term_id, entity_id;
        CREATE TABLE terms AS
        SELECT CAST(b * {T} + r AS UINTEGER) AS term_id,
               printf('t%0{width}dr%04d', CAST(b AS BIGINT), CAST(r AS BIGINT)) AS descriptor
        FROM (SELECT DISTINCT b FROM bdf) CROSS JOIN range({T}) t(r);
        """)
        direct = True
    else:
        if args.cross:
            f = (u2 * args.cross).astype(np.int64)
            fw = len(str(args.cross - 1))
            descrs = [f"t{v:0{width}d}x{c:0{fw}d}" for v, c in zip(k, f)]
        else:
            descrs = [f"t{v:0{width}d}" for v in k]
        raw_df = pl.DataFrame({"entity_id": [e for e, _ in rows], "descriptor": descrs})
        con.execute("CREATE TABLE raw AS SELECT * FROM raw_df")

elif args.config == "noise":
    name = f"categories-{args.level}-noise{args.epsilon}-seed{args.seed}"
    categories_raw(args.level)
    df = con.execute("SELECT entity_id, descriptor FROM raw ORDER BY entity_id, descriptor").pl()
    vocab = sorted(df["descriptor"].unique().to_list())
    ents = df["entity_id"].unique().sort()
    rng = np.random.default_rng(args.seed)
    flags = pl.DataFrame({
        "entity_id": ents,
        "do_add": rng.random(len(ents)) < args.epsilon,
        "do_drop": rng.random(len(ents)) < args.epsilon,
        "add_term": np.array(vocab)[rng.integers(0, len(vocab), len(ents))],
    })
    # drop one uniformly-chosen term from flagged items holding >= 2
    df = (df.with_columns(pl.Series("r", rng.random(len(df))))
            .join(flags, on="entity_id")
            .with_columns(pl.col("r").min().over("entity_id").alias("rmin"),
                          pl.len().over("entity_id").alias("n")))
    kept = df.filter(~(pl.col("do_drop") & (pl.col("n") >= 2) & (pl.col("r") == pl.col("rmin"))))
    adds = (flags.filter(pl.col("do_add"))
                 .select("entity_id", pl.col("add_term").alias("descriptor"))
                 .join(df.select("entity_id", "descriptor"), on=["entity_id", "descriptor"], how="anti"))
    raw_df = pl.concat([kept.select("entity_id", "descriptor"), adds])
    con.execute("CREATE OR REPLACE TABLE raw AS SELECT DISTINCT * FROM raw_df")

out = Path(args.out)
out.mkdir(parents=True, exist_ok=True)
pairs_f, terms_f = out / f"{name}.pairs.parquet", out / f"{name}.terms.parquet"

if not globals().get("direct"):
    con.execute("""
    CREATE TABLE terms AS
    SELECT CAST(row_number() OVER (ORDER BY descriptor) - 1 AS UINTEGER) AS term_id, descriptor
    FROM (SELECT DISTINCT descriptor FROM raw);
    CREATE TABLE pairs AS
    SELECT CAST(r.entity_id AS UINTEGER) AS entity_id, t.term_id
    FROM raw r JOIN terms t USING (descriptor)
    ORDER BY term_id, entity_id;
    """)
con.execute(f"COPY terms TO '{terms_f}' (FORMAT PARQUET); COPY pairs TO '{pairs_f}' (FORMAT PARQUET);")

n_corpus = con.execute(f"SELECT count(*) FROM read_parquet('{args.corpus}')").fetchone()[0]
print(f"== {name} ==")
n_pairs, n_terms, n_ents = con.execute(
    "SELECT (SELECT count(*) FROM pairs), (SELECT count(*) FROM terms),"
    " (SELECT count(DISTINCT entity_id) FROM pairs)").fetchone()
print(f"pairs: {n_pairs:,}   distinct terms: {n_terms:,}   entities covered: {n_ents:,}/{n_corpus:,}")

med, p99, mx, over_cap = con.execute("""
    SELECT median(n), quantile_cont(n, 0.99), max(n), 100.0 * count(*) FILTER (WHERE n > 64) / count(*)
    FROM (SELECT count(*) n FROM pairs GROUP BY entity_id)""").fetchone()
print(f"terms/item: median {med:.0f}, p99 {p99:.0f}, max {mx}, over cap 64: {over_cap:.4f}%")

print("top postings (descriptor, size, % of corpus):")
for d, n in con.execute("""
    SELECT descriptor, count(*) n FROM pairs JOIN terms USING (term_id)
    GROUP BY 1 ORDER BY n DESC LIMIT 8""").fetchall():
    print(f"  {d:<22} {n:>9,}  {100 * n / n_corpus:5.2f}%")

n_sigs, biggest = con.execute("""
    WITH sig AS (SELECT entity_id, hash(list_sort(list(term_id))) s FROM pairs GROUP BY entity_id)
    SELECT count(DISTINCT s), max(n) FROM (SELECT s, count(*) n FROM sig GROUP BY s)""").fetchone()
print(f"permission signatures: {n_sigs:,} distinct, largest group {biggest:,}")

runlen = con.execute("""
    WITH g AS (SELECT term_id, entity_id - row_number() OVER (PARTITION BY term_id ORDER BY entity_id) grp
               FROM pairs)
    SELECT count(*) * 1.0 / count(DISTINCT (term_id, grp)) FROM g""").fetchone()[0]
print(f"posting run length in entity order (free column): mean {runlen:.3f}")
