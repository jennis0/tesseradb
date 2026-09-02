# PaperSeek + OpenAlex — the label track

**What this half of rung 4 is.** PaperSeek publishes 102,117,343 OpenAlex works with embeddings and
no labels; OpenAlex publishes the labels and no embeddings. This track is the join between them:
one pass over OpenAlex `works`, a six-column extract keyed by the work id, and the module the
vectors track calls to turn a slice of PaperSeek ids into a publication year, a work type, an OA
flag, a **licence** — the corpus's compartment — and a **topic** in OpenAlex's four-level tree,
which is the `topics/openalex` layer.

The interface between the two tracks is fixed in
`.superpowers/sdd/2026-09-02-rung-4-paperseek/interface.md`. This track delivers `extract.py` and
`openalex.py`; the vectors track delivers everything else, and folds this file into the package
README at merge.

```bash
export TESSERA_LADDER=/home/joe/code/tessera/data/ladder
P=~/venvs/projection/bin/python

$P -m test_corpora.paperseek.extract           # the id set, the scan, the extract — once
$P -m test_corpora.paperseek.openalex          # the tree's and the extract's figures
```

Every figure below is **measured** unless it says otherwise, on 2026-09-02, against
`paperseek-openalex/2026-08-27` and `openalex/2026-08-27` on the SMB share, with another track
staging 209 GB off the same share concurrently — so the throughput figures are contended and are a
floor rather than the share's capability.

## 1. The join, and what it matched

PLACEHOLDER_JOIN

## 2. The scan

PLACEHOLDER_SCAN

## 3. The licence — the corpus's compartment

PLACEHOLDER_LICENCE

## 4. The topics

PLACEHOLDER_TOPICS

## 5. The layer, and the calls that shaped it

PLACEHOLDER_LAYER

## 6. The smoke build

PLACEHOLDER_SMOKE

## 7. What this track did not do

PLACEHOLDER_OPEN
