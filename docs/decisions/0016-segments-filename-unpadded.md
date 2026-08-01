# 0016 — The `SEGMENTS-<n>.json` filename grammar is unpadded

**Date:** 2026-08-01 · **Status:** Settled

## Context

Contracts §2.1 specified `n` as zero-padded decimal. `tessera-build` emitted unpadded. The reader
discovered candidates by parsing whatever decimal followed the prefix — reading `SEGMENTS-01.json`
as `n = 1` — and then reconstructed the path it read from as `SEGMENTS-1.json`: for a padded name,
a different and absent file.

A spec-conforming writer therefore produced manifests the reader stepped **silently** past, on the
fail-closed replica path, carrying the reader past a manifest that may hold a `deny` list.

## Decision

**`n` is unpadded decimal.** A leading zero is not a valid name, and a reader must **refuse** such
a candidate rather than parse it.

## Why

Padding buys nothing this format uses. Identity and order come from manifests, never from filename
lexicography — §2.1 is explicit about that. Padding would also require choosing a width, which
caps `n`; `n` is monotone and never resets across prefixes, so an unlucky width is a future format
break. And it would still require fixing the reader's reconstruction, so it is two changes rather
than one.

The refusal is the more important half. Parsing leniently and then reconstructing canonically is
exactly the combination that turns a malformed name into a silent step-past instead of an error.

## What remains

One code change, on the fail-closed side: the reader must reject a non-canonical name. Tracked as
an issue against the serving-surface epic.

Note the bound on this whole class of failure is §2.3's `readyz` freshness gate, which is
specified and unbuilt — so a replica stepped down for any reason currently serves stale
indefinitely with no signal. That is tracked separately.

## Evidence

Register row S20. `crates/tessera-build/src/lib.rs`, `crates/tessera-store/src/read.rs`, whose own
comments record the hole from the reader's side. Contracts r12 §2.1.
