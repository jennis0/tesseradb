"""The record blob's addressing self-consistency — the one artefact-level check records §10
licenses (review B7): rank, offsets, block bounds, discriminants, walked over the catalogue's
built `attrs/record/` by `oracle.record_blob`.

Why an artefact check exists at all in a suite whose relation is the fixture's inputs: the
addressing is the one property the served surface cannot exhibit. A mis-addressed blob still
serves *some* value for every entity — a neighbour's — and B6's fail-closed refusals (bounds,
discriminants) only mean something if the artefact's addressing is independently checkable. The
walk is structure-only: `oracle/record_blob.py`'s module doc states the licence, and nothing
here reads a field's value out of the artefact.

**What is deliberately NOT here, and where it is instead** — the blob-resident *values*:

- at build level, `crates/tessera-build/tests/record_blob.rs` reads every value back through the
  Rust `RecordBlob` reader and compares against its fixture's own generation functions;
- at the served surface, the record-always-exists differential (records §10; the epic's gate 3)
  asserts `/v1/items/{tessera_id}` returns every declared field equal to
  `oracle.catalogue.record_of` — that test lands when drill-down assembles the record from its
  three homes, and writing it against the artefact instead now would be the weaker relation
  records §3 declines.

Until then this suite's value coverage for `note` and `pages` is exactly: the fixture builds
green with them declared, the has-row bitmap matches the generation functions' presence, and
the addressing that will serve them is self-consistent.
"""

from __future__ import annotations

from oracle import catalogue as cat
from oracle import record_blob as rb


def _blob_tags(manifest: dict) -> set[int]:
    """The manifest positions of the blob-resident columns — a row's `tag` is the column's
    position in `declared_scalars` (an index internal, resolved server-side, never on the
    wire), and only a blob-resident column may appear in a row.

    **Two rules, not one.** A column with neither placement key is blob-resident because it has
    nowhere else to be; a **`text`** column is blob-resident *whatever its flags say*, because its
    index is postings over words and no drill-down can rebuild a sentence from the set of words it
    contained (records §4.4). That second rule is the family's defining property, and a predicate
    carrying only the first reads an indexed text column's own prose as a leak.
    """
    return {
        position
        for position, declared in enumerate(manifest["declared_scalars"])
        if declared["arrow_type"] == "text"
        or (not declared["index"] and not declared["render"])
    }


def test_the_blob_declares_exactly_the_fixtures_blob_columns(catalogue_bundle):
    """The placement half, from the manifest: `note` and `pages` are blob-resident, and nothing
    else is — in particular not the render-only category (`shelf`), because a category is never
    blob-resident (records §4.2: its entity-space structures are the constant floor)."""
    by_name = {d["name"]: d for d in catalogue_bundle.manifest["declared_scalars"]}
    neither_key = {
        name for name, d in by_name.items() if not d["index"] and not d["render"]
    }
    assert neither_key == {"note", "pages"}
    assert by_name["shelf"]["render"] and not by_name["shelf"]["index"]
    # **And `abstract` is blob-resident *as well as* indexed** — the only family with two homes.
    # Its terms answer `match`; its prose is a blob row, because postings reconstruct nothing.
    assert by_name["abstract"]["arrow_type"] == "text"
    assert by_name["abstract"]["index"] and not by_name["abstract"]["render"]
    assert _blob_tags(catalogue_bundle.manifest) == {
        position
        for position, d in enumerate(catalogue_bundle.manifest["declared_scalars"])
        if d["name"] in {"note", "pages", "abstract"}
    }


def test_the_blob_addressing_is_self_consistent(catalogue_bundle_root, catalogue_bundle):
    """The whole walk, one call: blocks tile the file, ranks tile the rank space, rows tile
    their blocks, discriminants agree with has-row's rank order, fields frame exactly, tags are
    blob-resident columns only — and has-row's membership is what the generation functions
    planted, which is the fixture-input half of the relation."""
    failures = rb.self_check(
        rb.record_dir_of(catalogue_bundle_root),
        expected_entities=cat.blob_entities_expected(catalogue_bundle),
        allowed_tags=_blob_tags(catalogue_bundle.manifest),
    )
    assert not failures, "the blob's addressing has drifted:\n" + "\n".join(failures)


def test_an_oversized_row_gets_an_oversized_block_of_its_own(catalogue_bundle_root, catalogue_bundle):
    """Records §3's "a target, not a cap", on the artefact: the planted > 256 KiB note must land
    in a block above the target holding exactly that one row — never split across blocks — and
    the corpus must cut enough ordinary blocks that the first/last-of-block drill-down cases
    (records §10's catalogue) are non-degenerate when they land."""
    from pyroaring import BitMap  # noqa: PLC0415

    record_dir = rb.record_dir_of(catalogue_bundle_root)
    headers = rb.block_headers(record_dir)
    hasrow = list(BitMap.deserialize((record_dir / rb.HASROW_FILE).read_bytes()))

    assert len(headers) > 2, (
        f"{len(headers)} blocks — too few for block-boundary cases to mean anything"
    )
    # The target is measured over a block's rows, the header it carries being addressing rather
    # than content (records §3).
    oversized = [h for h in headers if h["rows_len"] > rb.BLOCK_TARGET]
    assert oversized, "no block exceeds the target, so the oversize rule is untested"
    for block in oversized:
        assert block["row_count"] == 1, (
            f"an oversized block holds {block['row_count']} rows — only a single row "
            "larger than the target may pass it"
        )
    # The planted oversize entity is the one carrying such a row. `NOTE_OVERSIZE_ID` is the
    # **source** id the note was planted on, so it crosses to entity space here rather than being
    # compared as though the two were one number.
    oversize_entity = catalogue_bundle.entity_of_source(cat.NOTE_OVERSIZE_ID)
    oversize_entities = {b["first_entity"] for b in oversized}
    assert oversize_entities == {oversize_entity}, (
        f"the oversized rows belong to {sorted(oversize_entities)}, not entity "
        f"{oversize_entity} (source {cat.NOTE_OVERSIZE_ID}), which is where the note was planted"
    )
    # The block's own statement of its first entity and the has-row bitmap's member at that rank
    # are two files' answers to one question, and the walk above rests on their agreeing.
    for block in headers:
        assert hasrow[block["first_rank"]] == block["first_entity"]
