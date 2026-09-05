# 0129 — The ingest wire carries access labels as a list

**Date:** 2026-09-05 · **Status:** Settled (owner ruling) · ⊘ Not built

## What this answers

`/control/ingest`'s `access` column is one UTF-8 string per row, and the passthrough plugin's wire
path (`Plugin::terms_of_label`) splits it on commas. The build reads a list column and takes each
element verbatim (`Plugin::terms_of_labels`). Rung 5 is the first corpus whose compartment keys
contain the separator: 70 of TreeOfLife's 474 publisher names contain a comma. On the wire each
split into fragments and every fragment not already a term was minted — 617 terms against the
declaration's 475. Measured on the folded 50% cell: 2,142,399 hold-out rows visible to no declared
principal, and 73,212 rows of `Natural History Museum, Vienna` landed in `Natural History Museum`'s
compartment, where a principal the data never named can see them. The harness reproduced the
mechanism on `treeoflife-1m` (60 rows, the same publisher) on 2026-09-05.

Three options were put: the wire carries a list; the label grammar gains an escape; a reserved
separator that no key may contain.

## The decision

**`access` on `/control/ingest` is a list of labels, `list<utf8>` (or `large_list<utf8>`), one
element per label, taken verbatim.** The plugin's wire path is `terms_of_labels`; `terms_of_label`
and its comma grammar are deleted (decision 0048: no compatibility, no second spelling kept). A
`utf8` `access` column is a `422` naming the column and the shape it takes. Both entry points then
read one column type and one rule, which is what decision 0091 requires of them. An empty list is
a row with no label, as an empty relation is at the build; the plugin decides what that means.

## Why

A list is what the data is. A grammar over a scalar is the shape of the defect: an escape needs
two encoders and a decoder kept in step and is broken by the next key containing the escape; a
reserved separator is a convention where a type would do. The disclosure was not in the engine —
every masked count was computed inside the mask the labels produced — but the labels were wrong,
and the wire form made them wrong.

## What this changes elsewhere

Contracts §3.4, the `/control/ingest` row: the `access` column's type. `tessera-plugin`'s trait
and `Passthrough`. The ingest handler's decode. The conformance oracle, which implements the same
mapping. `test_corpora/common/ingest_cycle.py`'s `encode_batch`, which joins a list with commas
today. The Python SDK where it builds an ingest batch. After the change, rung 5's 50% cell re-run
should reach 233,055,986 visible under the declared terms alone, and the rung's README's comma
paragraphs become history.
