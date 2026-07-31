"""Design §7.2, written as a **definition**.

**Pinned to §7.2 as of design r24** (`188d961`; the anchor's row-space reading arrived in
`a2db35d`, the floor and zero-case rules in `188d961`). A §7.2 revision is a **required edit
here**, and that is a standing obligation rather than a courtesy: this module's whole value is
being visibly the spec, so prose that has quietly fallen a revision behind is worse than no prose
— a reader checks the module against the document precisely by reading this, and undetectable
drift is the failure mode. Anything below tagged with a revision was checked against that
revision's text.

This module is the second implementation of record for one thing: what the service is *supposed*
to serve. Its job is to be obviously the spec, so that when it disagrees with the engine a reader
can tell which one is wrong by reading it. It is therefore written the slow, literal way on
purpose — if it ever looks clever, that is the bug.

§7.2's definition, verbatim, for a tile *T* at depth *d* with `vis(T)` its visible set ordered
ascending by `tessera_id`:

```
cap       = min(k, K_max)
C_θ(T)    = |{ i ∈ vis(T) : tessera_id(i) < P_d }|
m(T)      = min(cap, max(min(k_min, cap), C_θ(T)))
served(T) = the min(m(T), |vis(T)|) smallest members of vis(T) by tessera_id
```

with θ anchored on the viewer's own **composed** visible total over the slice: `P_0 = ⌊m_target ·
2⁶⁴ / V_total⌋`, `P_{d+1} = 4·P_d`, saturating — and `V_total` is `|M_auth ∩ rows(slice)|`,
composed *and counted in row space* (r24). Both qualifiers are load-bearing: *composed* is the I2
requirement below, and *row space* is §11.2's rule that an entity with no row contributes to no
count whatever `L` says — which under group-commit allocation is the normal steady state of an
ingesting deployment, not a transient, since a batch is acked and so in `M_auth` for a whole
commit window before flush gives it rows. [`visible_total`] counts segment rows, which is that
reading.

## What is literal here, and what is not — the line is drawn deliberately

**Literal, and required to stay so:** the selection itself. A literal sort of the tile's visible
identities by `tessera_id`; a literal count of how many fall below `P_d`; literal `min`/`max`
arithmetic for *m*; a literal take of the first *m*. No bitmap, no bounded heap, no partial sort,
no early exit, no prefix comparison. The engine reaches the same answer with a single pass and a
bounded heap, and a differential between two transcriptions of the same algorithm proves only that
copy-paste works — so the two constructions must differ, and only the *definition* may be shared.

**Not literal, and said plainly so a reader is not misled:** finding a tile's rows. [`Selection`]
makes one pass over every row of the segment, computes that row's depth-*d* tile from its geometry,
and buckets it. That is `tile_of` applied to every row — the definition of "the tile a row is in" —
rather than a Morton range search over the stored, sorted `morton.u32` column, which is the
engine's own construction and would import the engine's assumption. The pass is amortised across
the tiles of one request because it answers all of them at once, not because a faster route was
preferred; per tile it is still O(rows in the segment).

Two things follow from recomputing the tile from `(x, y)` rather than reading `morton.u32`. It is
**stronger** — a build that wrote a wrong Morton column and then sorted and served consistently by
its own wrong values fails this differential rather than passing it. And it removes the *tiling*
side of the path from the set of artefacts the two implementations share.

## The one artefact that IS shared, and what closes it

`served_rows` sorts by `self.segment.tessera_id` — **the stored column**, which is also what the
engine sorts and serves by. That is a shared artefact on the one §7.2 quantity this differential
exists to referee, and it is stated here because an earlier draft of this doc claimed the opposite
("oracle and engine share no stored artefact anywhere on this path"), which was false. A build
writing a wrong-but-self-consistent `tessera_id` column — say one still correlated with signature
order, the exact r21 disclosure the negative control's docstring invokes — would be agreed with,
not caught.

It is not re-derived here, for the reason the rest of this module is written the way it is: the
re-derivation (`identity.forward` over `(shard_id, entity_id)`) is a keyed Feistel permutation,
which is emphatically not a literal transcription of anything §7.2 says, and putting it in the
selection path would trade an auditable definition for a cryptographic one. The check belongs
where it can be made once and read: `Bundle.verify_identity_cross_check` proves the stored column
*is* `forward(key, shard, entity_of_row)` for a sample of rows, `Bundle.derive_row_order` proves
the rows are stored in the order that key implies, and the fixture — not the bundle — supplies the
key. `conformance/tests/test_mask_catalogue.py` runs all three against the catalogue bundle, so by
the time a `Selection` reads the column, nothing about it is being taken on trust.

## θ's anchor is computed here, never read back

[`visible_total`] derives `V_total` from the segment and the pairs-derived mask. It is deliberately
not taken from a server response: θ's anchor is the one input the oracle would otherwise have to
take on trust, and taking it from the thing under test makes the differential circular for every
density assertion. §7.2 is also explicit that the anchor must be the **composed** total (after the
overlay diff) and not the frozen fragment's — anchoring on the pre-overlay projection would let a
viewer aggregate a few hundred tiles, solve for the anchor, and recover a running count of how many
of its own items have been denied, which Appendix C admits nowhere. The composition happens in
`mask.ChangeSet`, so what arrives here is already `M_auth`.
"""

from __future__ import annotations

from . import morton
from .bundle import Bundle


SATURATED = None
"""θ_d ≥ 1: the threshold admits every identity. A distinct state, never a value.

Not `2**64 - 1`, and not `2**64`: at θ ≥ 1 the threshold must admit *every* identity, and the
comparison is strict (`tessera_id < P_d`), so any clamp to a representable cut wrongly excludes
the single row whose `tessera_id` is `2**64 - 1`. Python's unbounded integers would let this
module get away with `2**64` as a sentinel; it does not, because §7.2 has a saturated *state* and
this file's job is to look like §7.2.
"""


def theta_cut(v_total: int, m_target: int, depth: int) -> int | None:
    """`P_d` as a cut point over the identity space, or [`SATURATED`].

    §7.2 states θ as a **recurrence**, and this is written as one:

        P_0     = ⌊m_target · 2⁶⁴ / V_total⌋      (r24: floors; saturated when V_total = 0)
        P_{d+1} = 4 · P_d                          saturating at 2⁶⁴

    The engine (`crates/tessera-engine/src/select.rs`) evaluates the same thing in closed form, as
    `P_0 << 2d` with a `leading_zeros` overflow test. **The two constructions differ on purpose**
    (controller ruling, fix round 1): this module's premise is that "a differential between two
    transcriptions of the same algorithm proves only that copy-paste works", and a transcribed
    `p0 << (2 * depth)` was exactly that — the same expression, the same saturation test, on both
    sides. Integer arithmetic makes the recurrence and the shift identical in *result* for every
    input, so writing them differently costs no false failures and buys a genuinely independent
    derivation of the one quantity a θ-live differential turns on.

    **The rounding and the zero case are the spec's, not this module's** *(r24, `188d961`)*. Both
    were unstated until that revision and both are observable — the differential demands exact
    equality, so an implementation that rounded or took a ceiling would disagree on roughly half of
    all anchors. §7.2 now settles them: `P_0` **floors** (a smaller `P_0` is a stricter threshold,
    so rounding down errs toward fewer marks, never more, and the floor clause guarantees
    non-emptiness regardless), and `P_0` is **saturated** when `V_total = 0`. `//` and the
    `v_total <= 0` branch below are therefore transcriptions of a rule, not this file's choice —
    which is what they were when the rule was unwritten, and what the fix-round-1 brief still
    recorded them as. A negative `v_total` is impossible rather than specified; it is folded into
    the zero branch because a count cannot be negative and this module refuses to invent a fourth
    behaviour for a state that cannot arise.

    `v_total` is the viewer's **composed** visible total over the slice, counted in **row space**
    (r24) — the mask after the overlay diff, not the raw fragment (I2; see the module doc).
    """
    if v_total <= 0:
        return SATURATED
    p_d = (m_target << 64) // v_total
    if p_d >= 1 << 64:
        return SATURATED
    # `P_{d+1} = 4·P_d`, one depth at a time, testing saturation at each step — §7.2's own
    # progression rather than its closed form. The test is before the multiply, so no value beyond
    # the identity space is ever formed (Python would happily form one; the definition would not).
    for _ in range(depth):
        if p_d >= (1 << 64) // 4:
            return SATURATED
        p_d = 4 * p_d
    return p_d


class Selection:
    """§7.2 evaluated over one `(bundle, mask, slice, depth)`.

    Constructed once per request and asked about each of that request's tiles, because **the count
    and the selection must read the same `vis(T)`**. §7.1 discloses the tile's exact masked count
    and §7.2 selects from that same set; computing them by two different routes would let a
    disagreement between them hide inside the oracle instead of surfacing as a differential
    failure.

    The constructor is the one literal pass described in the module doc. Nothing is cached across
    instances: a mask is a value, and an oracle that memoised on one would start answering a
    question it was not asked the first time a `ChangeSet` composed a new mask.
    """

    def __init__(self, bundle: Bundle, mask: set[int], slice_id: str, depth: int):
        self.bundle = bundle
        self.slice_id = slice_id
        self.depth = depth
        self.segment = bundle.segment(slice_id)

        if self.segment.tessera_id is None:
            raise ValueError("segment has no stored tessera_id column (pre-r6 bundle)")

        codes = bundle.row_morton_codes(slice_id)
        entities = bundle.row_entity_ids(slice_id)
        shift = 32 - 2 * depth

        # The literal pass: for every row, which depth-`depth` tile is it in, and is its entity
        # visible? Rows are collected in row order; §7.2's order is imposed at selection time by
        # an explicit sort and never inherited from storage order. The engine stores rows in
        # `(morton, tessera_id)` order, so inheriting it here would silently assume the very
        # ordering the differential exists to check.
        self.rows_by_tile: dict[int, list[int]] = {}
        for row in range(self.segment.row_count):
            if entities[row] in mask:
                self.rows_by_tile.setdefault(codes[row] >> shift, []).append(row)

    # -- §7.1 -----------------------------------------------------------------------------------

    def visible_count(self, tile: int) -> int:
        """`|vis(T)|` — the tile's exact masked count."""
        return len(self.rows_by_tile.get(tile, ()))

    def counts_for(self, tiles: list[int]) -> dict[int, int]:
        """`{tile: |vis(T)|}` over `tiles`, omitting the empty ones.

        Omission rather than a zero, because a zero-count tile is never emitted on the wire — and
        that omission is itself a masked count, which Appendix C records rather than hides.
        """
        return {t: len(rows) for t in tiles if (rows := self.rows_by_tile.get(t))}

    # -- §7.2 -----------------------------------------------------------------------------------

    def served_rows(
        self,
        tile: int,
        *,
        k_min: int,
        cap: int,
        v_total: int,
        m_target: int,
    ) -> list[int]:
        """§7.2's `served(T)`, as row ids in served order (ascending `tessera_id`).

        Four steps, one per line of the definition, in the definition's own order:

            vis(T) ordered ascending by tessera_id            -- a literal sort
            C_θ(T) = |{ i ∈ vis(T) : tessera_id(i) < P_d }|   -- a literal count
            m(T)   = min(cap, max(min(k_min, cap), C_θ(T)))   -- literal arithmetic
            served = the min(m, |vis(T)|) smallest            -- a literal slice

        Ascending `tessera_id` is also the order the payload must arrive in (contracts §2.6: the
        points batch is ordered ascending by `tessera_id` within each tile). The nesting argument's
        client-truncation clause depends on the served set being a prefix, so the order is
        contract, not presentation.
        """
        identities = self.segment.tessera_id
        rows = sorted(self.rows_by_tile.get(tile, ()), key=lambda row: int(identities[row]))

        cut = theta_cut(v_total, m_target, self.depth)
        if cut is None:
            c_theta = len(rows)
        else:
            c_theta = len([row for row in rows if int(identities[row]) < cut])

        floor = min(k_min, cap)
        m = min(cap, max(floor, c_theta))
        return rows[: min(m, len(rows))]

    def served_points(self, tile: int, **params) -> list[tuple[float, float]]:
        """[`served_rows`] as `(x, y)` pairs **in served order**, which is what the wire carries.

        The differential compares by coordinate rather than by identity deliberately: agreement
        then never depends on either side *interpreting* an identifier, only on both selecting the
        same items. It compares the **list**, not a set or a multiset — both sides are in ascending
        `tessera_id` order and contracts §2.6 makes that order part of the payload contract (see
        [`served_rows`]), so list equality is strictly stronger for free.
        """
        seg = self.segment
        return [(float(seg.x[row]), float(seg.y[row])) for row in self.served_rows(tile, **params)]

    def served_identities(self, tile: int, **params) -> list[int]:
        """[`served_rows`] as `tessera_id`s — the key §7.2's nesting property is stated over.

        Used where the question is *which items*, not *which coordinates*: the overlay differential
        asks whether a denied item's identity appears in the payload, and two entities can share
        rounded coordinates inside one tile, so a coordinate answer to that question would be
        approximate where an exact one is available.
        """
        return [int(self.segment.tessera_id[row]) for row in self.served_rows(tile, **params)]

    # -- the negative control ---------------------------------------------------------------------

    def first_k_rows(
        self,
        tile: int,
        *,
        k_min: int,
        cap: int,
        v_total: int,
        m_target: int,
    ) -> list[int]:
        """**A deliberately wrong selection**: §7.2's *count*, drawn in storage order.

        This is not an alternative definition and must never be used as one. It exists so the I7
        differential can be shown to be *live*: a differential that passes against a deliberately
        wrong implementation is testing nothing, and this is the cheapest possible check that it is
        not. It reproduces the pre-2026-07-30 placeholder sampler — take the first `m` rows the
        segment happens to hold — which keeps every count identical and gets the *membership*
        wrong, precisely the class of bug §7.2's ordering exists to exclude.

        `m` is computed by the real definition on purpose. A stub that also got the count wrong
        would be caught by the tile batch's `served` column, so the differential could disagree
        with it while still blind to membership; matching the count forces the point-set
        comparison to do the work.
        """
        rows = self.rows_by_tile.get(tile, [])
        identities = self.segment.tessera_id
        ordered = sorted(rows, key=lambda row: int(identities[row]))

        cut = theta_cut(v_total, m_target, self.depth)
        if cut is None:
            c_theta = len(ordered)
        else:
            c_theta = len([row for row in ordered if int(identities[row]) < cut])
        m = min(cap, max(min(k_min, cap), c_theta))
        return rows[: min(m, len(rows))]

    def first_k_points(self, tile: int, **params) -> list[tuple[float, float]]:
        seg = self.segment
        return [(float(seg.x[row]), float(seg.y[row])) for row in self.first_k_rows(tile, **params)]


# ---------------------------------------------------------------------------------------------
# Free functions — the single-tile entry points, and θ's anchor
# ---------------------------------------------------------------------------------------------


def counts(
    bundle: Bundle,
    mask: set[int],
    slice_id: str,
    zoom: int,
    bbox: tuple[float, float, float, float],
) -> dict[int, int]:
    """`{tile: count of mask-visible rows}` for every depth-`zoom` tile overlapping `bbox`."""
    selection = Selection(bundle, mask, slice_id, zoom)
    return selection.counts_for(morton.tiles_for_bbox(bbox, zoom, bundle.extent))


# (There is deliberately no single-tile `served()` here. It existed, had no call site, and was a
# trap: one tile per call is one whole pass over the segment per call, and every caller has
# several tiles. Build one `Selection` and ask it — same definition, one pass.)


def params_from_meta(meta_selection: dict, *, k: int, v_total: int) -> dict:
    """The four parameters §7.2's definition takes, from `GET /v1/meta` plus the request's `k`.

    The oracle cannot know `k_min`, `K_max` or `m_target` — they are deployment configuration —
    and `/v1/meta` publishes them precisely so an independent implementation can reproduce the
    served set (contracts §3.2). `v_total` is **not** among them: θ's anchor is computed by
    [`visible_total`] and never read back from the service (see the module doc).

    `cap = min(k, K_max)` is taken from §7.2 itself. Note what it implies and what the design says
    about it: the proportionality window is `min(k, K_max)/k_min`, **not** `K_max/k_min`, so a
    request `k` below the deployment's `K_max` silently narrows the density window. That is a
    property of the definition, so the oracle reproduces it rather than correcting it.
    """
    return {
        "k_min": meta_selection["k_min"],
        "cap": min(k, meta_selection["k_max_marks"]),
        "m_target": meta_selection["theta_target_marks"],
        "v_total": v_total,
    }


def visible_total(bundle: Bundle, mask: set[int], slice_id: str) -> int:
    """The viewer's total visible count over the whole slice — θ's anchor input.

    Computed independently from the segment and the pairs-derived mask, deliberately **not** read
    back from a server response (see the module doc).
    """
    return sum(1 for entity in bundle.row_entity_ids(slice_id) if entity in mask)
