"""What both stages of the arXiv rung read, and the one expensive read they share.

**This rung's source is derived rather than staged.** Every other rung reads a publisher's own
bytes off the share; arXiv reads `data/` in this checkout, which `probes/build_corpus.py` and
`probes/build_embeddings.py` produced from the arXiv metadata snapshot and a BGE pass over it. The
share holds a mirror of that directory at `arxiv-tessera/2026-07-27/`, which is a backup and not
the source — see `../README.md`.

The expensive read is the embedding matrix: 2,422,486 x 1024 float32 is 9.9 GB, and both stages
need it — `prepare` for PCA and UMAP, `toponymy` for the exemplars and the keyphrase ranking. It is
streamed and filtered to the sample so that memory is the sample's rows rather than the corpus's,
and it is re-read by the second stage rather than kept: 63 seconds of streaming against 9.9 GB of
derived file is the cheaper side of that trade, and it is what keeps the two stages independent.
"""

from __future__ import annotations

import os
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

_REPO_ROOT = Path(__file__).resolve().parents[2]

#: The checkout holding `data/`. A worktree has none of its own, so this is a variable.
DATA = Path(os.environ.get("TESSERA_DATA", _REPO_ROOT / "data"))

#: BGE-large-en-v1.5, which is what `probes/build_embeddings.py` used and therefore what the
#: keyphrase step has to use for its rankings to mean anything.
EMBED_DIM = 1024


def load_metadata(data: Path = DATA):
    """The corpus and its prose, checked to be in one entity order.

    Both tables are in `entity_id` order over the same dense `0..n` space, which
    `build_corpus.py` assigns in `(v1_created, id)` order. Asserted rather than assumed: a silent
    misalignment here would attach every paper's title to a different paper's position.
    """
    corpus = pq.read_table(
        data / "corpus.parquet", columns=["entity_id", "id", "categories", "v1_created"]
    )
    prose = pq.read_table(data / "demo" / "prose.parquet", columns=["entity_id", "title", "abstract"])
    assert prose.num_rows == corpus.num_rows
    assert np.array_equal(
        corpus.column("entity_id").to_numpy(), prose.column("entity_id").to_numpy()
    ), "corpus and prose disagree about entity order"
    return corpus, prose


def sample_rows(n_full: int, sample: int | None, seed: int) -> np.ndarray:
    """The rows this run takes, drawn uniformly and then sorted.

    **Uniform rather than a prefix.** The corpus is in submission order, so a prefix takes the
    oldest papers and changes both the category skew and the density structure the clusterings
    depend on. Sorting afterwards keeps the output in a stable order whatever the sample size.
    """
    if sample is None or sample >= n_full:
        return np.arange(n_full)
    return np.sort(np.random.default_rng(seed).choice(n_full, sample, replace=False))


def load_embeddings(corpus, take: np.ndarray, data: Path = DATA) -> np.ndarray:
    """The sample's embeddings, L2-normalised, streamed rather than loaded.

    The full matrix is 9.9 GB and this machine has 47 GB, so a sample of a few hundred thousand
    read whole would spend most of the box on rows it then discards. Batches are filtered on the
    way past instead.
    """
    ids = np.asarray(corpus.column("id")).tolist()
    ent_of_id = dict(zip(ids, corpus.column("entity_id").to_numpy().tolist()))
    n_full, n = corpus.num_rows, len(take)
    row_of_entity = np.full(n_full, -1, dtype=np.int64)
    row_of_entity[take] = np.arange(n)

    out = np.empty((n, EMBED_DIM), dtype=np.float32)
    seen = np.zeros(n, dtype=bool)
    reader = pq.ParquetFile(data / "arxiv_papers_embeds.parquet")
    for batch in reader.iter_batches(batch_size=65536, columns=["paper_id", "embedding"]):
        batch_ids = batch.column("paper_id").to_pylist()
        rows = row_of_entity[
            np.fromiter((ent_of_id[i] for i in batch_ids), dtype=np.int64, count=len(batch_ids))
        ]
        hit = np.flatnonzero(rows >= 0)
        if len(hit) == 0:
            continue
        block = np.asarray(batch.column("embedding").flatten(), dtype=np.float32)
        out[rows[hit]] = block.reshape(len(batch_ids), EMBED_DIM)[hit]
        seen[rows[hit]] = True
    assert seen.all(), f"{(~seen).sum()} sampled papers have no embedding"

    # BGE is cosine-conventional, so L2-normalise before any Euclidean step. In blocks, because
    # the whole-matrix form allocates a second copy of it.
    for lo in range(0, n, 200_000):
        block = out[lo : lo + 200_000]
        block /= np.linalg.norm(block, axis=1, keepdims=True)
    return out


def rows_for_ids(corpus, arxiv_ids) -> np.ndarray:
    """The corpus rows a written points file came from, recovered from its `arxiv_id` column.

    The second stage needs the same embeddings the first one projected, and the sample that
    selected them is not written anywhere — it is a draw from a seed. Recovering it from the ids
    the points file carries is exact and costs one pass, where writing the 9.9 GB matrix out for
    the second stage to read would cost the disk.
    """
    index = {i: r for r, i in enumerate(np.asarray(corpus.column("id")).tolist())}
    missing = [i for i in arxiv_ids if i not in index]
    assert not missing, f"{len(missing)} written papers are not in {DATA / 'corpus.parquet'}"
    return np.fromiter((index[i] for i in arxiv_ids), dtype=np.int64, count=len(arxiv_ids))
