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

with θ anchored on two quantities of the viewer's own **composed** mask over the view: `P_d =
⌊m_target · N_occ(d) · 2⁶⁴ / V_total⌋`, saturating. `V_total` is `|M_auth ∩ rows(view)|`, composed
*and counted in row space* (r24). Both qualifiers matter: *composed* is the I2 requirement below,
and *row space* is §11.2's rule that an entity with no row contributes to no count whatever `L`
says — which under group-commit allocation is the normal steady state of an ingesting deployment,
not a transient, since a batch is acked and so in `M_auth` for a whole commit window before flush
gives it rows. [`visible_total`] counts segment rows, which is that reading.

`N_occ(d)` is the number of depth-*d* tiles holding at least one row the viewer can see (r62),
counted over the same composed mask and over the whole view. [`Selection`] already buckets every
visible row by its depth-*d* tile, so `N_occ(d)` here is the number of buckets — the definition
read straight off the structure the counts come from.

One clause arrives from outside §7.2: a request carrying a filter evaluates the definition with
the threshold **saturated** (§8.5's match-layer rule — every match served, up to the cap), which
[`Selection.served_rows`] exposes as `filtered=True`. The anchor above is untouched by it.

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
key.

**Where those checks are run, and where they are not.**
`conformance/tests/test_mask_catalogue.py` runs all three against the catalogue bundle, so on that
bundle a `Selection` takes nothing about the column on trust. `reference/tests/test_differential.py`
runs the §7.2 differential over the 250k `--mint-id-key` fixture and calls **none** of them, and it
cannot call the third: that fixture's key is a build *output* rather than a fixture input, so there
is nothing independent to compare the manifest's key against. On that bundle the shared artefact is
open, and a wrong-but-self-consistent `tessera_id` column is agreed with rather than caught.
`conformance.md` §4.6's I7 evidence is the catalogue differential, where the hole is closed, so no
coverage row rests on the open half — a reader need not re-derive that.

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
from . import occupancy
from .bundle import Bundle


SATURATED = None
"""θ_d ≥ 1: the threshold admits every identity. A distinct state, never a value.

Not `2**64 - 1`, and not `2**64`: at θ ≥ 1 the threshold must admit *every* identity, and the
comparison is strict (`tessera_id < P_d`), so any clamp to a representable cut wrongly excludes
the single row whose `tessera_id` is `2**64 - 1`. Python's unbounded integers would let this
module get away with `2**64` as a sentinel; it does not, because §7.2 has a saturated *state* and
this file's job is to look like §7.2.
"""


def theta_cut(v_total: int, m_target: int, n_occ: int) -> int | None:
    """`P_d` as a cut point over the identity space, or [`SATURATED`].

    §7.2 (r62) states θ at depth *d* as one product over the viewer's own composed mask:

        θ_d = m_target · N_occ(d) / V_total
        P_d = ⌊θ_d · 2⁶⁴⌋                          saturated at θ_d ≥ 1, and when V_total = 0

    Written as the definition writes it: form the whole product, floor once, and answer
    [`SATURATED`] where the threshold reaches everything. Python's unbounded integers mean no
    intermediate value has to be checked for width, which is where the engine's construction
    differs — it must test θ_d ≥ 1 before it forms `numer << 64` so the shift cannot lose a bit.

    **Where the independence between the two implementations now sits.** It was in the arithmetic:
    §7.2 stated θ as a recurrence and this module wrote one where the engine wrote a shift. The
    closed form has no recurrence left to differ over, so the two now compute one expression. What
    is independent is `N_occ(d)`'s **input**: [`Selection`] recomputes each row's tile from the
    source geometry and counts the distinct ones, where the engine gallops the stored Morton
    column. A build that wrote a wrong Morton column fails that comparison rather than passing it.
    Above one segment the engine estimates that count instead, and this side transcribes the
    estimator rather than deriving it — `oracle/occupancy.py` says so, and says why no bundle this
    oracle opens takes that route.

    **The rounding and the zero case are the spec's, not this module's** *(r24, `188d961`)*. Both
    were unstated until that revision and both are observable — the differential demands exact
    equality, so an implementation that rounded or took a ceiling would disagree on roughly half of
    all anchors. §7.2 settles them: `P_d` **floors** (a smaller `P_d` is a stricter threshold, so
    rounding down errs toward fewer marks, never more, and the floor clause guarantees
    non-emptiness regardless), and it is **saturated** when `V_total = 0`. `//` and the
    `v_total <= 0` branch below are therefore transcriptions of a rule, not this file's choice. A
    negative `v_total` is impossible rather than specified; it is folded into the zero branch
    because a count cannot be negative and this module refuses to invent a fourth behaviour for a
    state that cannot arise.

    **The floor is taken once, over the whole product** (r62). Flooring `m_target · 2⁶⁴ / V_total`
    first and multiplying by `N_occ` afterwards floors twice and gives a different cut for most
    inputs; the engine forms the same single product.

    `v_total` is the viewer's **composed** visible total over the view, counted in **row space**
    (r24), and `n_occ` is counted over that same composed mask — the mask after the overlay diff,
    not the raw fragment (I2; see the module doc).
    """
    if v_total <= 0:
        return SATURATED
    p_d = (m_target * n_occ << 64) // v_total
    if p_d >= 1 << 64:
        return SATURATED
    return p_d


class Selection:
    """§7.2 evaluated over one `(bundle, mask, view, depth)`.

    Constructed once per request and asked about each of that request's tiles, because **the count
    and the selection must read the same `vis(T)`**. §7.1 discloses the tile's exact masked count
    and §7.2 selects from that same set; computing them by two different routes would let a
    disagreement between them hide inside the oracle instead of surfacing as a differential
    failure.

    The constructor is the one literal pass described in the module doc. Nothing is cached across
    instances: a mask is a value, and an oracle that memoised on one would start answering a
    question it was not asked the first time a `ChangeSet` composed a new mask.
    """

    def __init__(self, bundle: Bundle, mask: set[int], view_id: str, depth: int):
        self.bundle = bundle
        self.view_id = view_id
        self.depth = depth
        self.segment = bundle.segment(view_id)

        if self.segment.tessera_id is None:
            raise ValueError("segment has no stored tessera_id column (pre-r6 bundle)")

        # The full 64-bit positions, recomputed from the source geometry: the cell half decides
        # the tile, and the whole thing is what the wire comparison is against. One derivation,
        # so the tile a row is placed in and the position it is served with cannot disagree.
        self.position_codes = bundle.row_position_codes(view_id)
        codes = [code >> 32 for code in self.position_codes]
        entities = bundle.row_entity_ids(view_id)
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

        # `N_occ(d)`, resolved on first use and kept. The ladder is a pass over the tile set and
        # `served_rows` asks for the anchor once per tile; recomputing it per tile would be the
        # same answer at a hundred times the cost.
        self._n_occ: int | None = None

    @property
    def n_occ(self) -> int:
        """`N_occ(d)` — θ's second anchor (§7.2 r63): how many depth-*d* tiles hold at least one
        row this mask admits.

        A bucket exists in `rows_by_tile` exactly when a visible row fell in that tile, so the
        bucket keys **are** `N_occ(d)`'s input set, over the whole view and over the composed mask.
        Turning that set into a number is `oracle/occupancy.py`'s job and not this property's,
        because the engine has two routes for it and the differential must take the same one: a
        **count** at one segment, a sketch estimate above it.

        **`segments=1` is a fact about this oracle's bundles, not an assumption.**
        `oracle/bundle.py` opens exactly one segment per view — `bundle.segment(view_id)` is
        singular — so the counted route is the one that runs here, and `occupancy.ladder` is handed
        the segment count rather than being told which route to take.

        **Over the view, never over a request's tiles.** θ is viewport-invariant, so this counts
        every occupied tile in the view and not the ones a bbox happens to name. Narrowing it to a
        request would make θ move on a pan.

        The whole ladder `0..=depth` is built, not just this depth, because the `4^d` ceiling and
        the running maximum that make θ monotone are over the rungs at and below the one asked for.
        """
        if self._n_occ is None:
            self._n_occ = occupancy.ladder(self.rows_by_tile.keys(), self.depth, segments=1)[
                self.depth
            ]
        return self._n_occ

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
        filtered: bool = False,
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

        `filtered=True` is §8.5's match-layer count rule: a request carrying a filter evaluates
        this same definition over `M_sel` with the threshold **saturated**, so `C_θ = |vis(T)|`
        and every match is served up to the cap. θ's *anchor* never re-anchors on `M_sel`
        (filter-surface §5.2 forbids a threshold that moves as the viewer types); the filtered
        selection simply does not consult it. The caller passes `vis(T)` already filtered — this
        module holds one visible set per `Selection` and no filter machinery of its own.
        """
        identities = self.segment.tessera_id
        rows = sorted(self.rows_by_tile.get(tile, ()), key=lambda row: int(identities[row]))

        cut = SATURATED if filtered else theta_cut(v_total, m_target, self.n_occ)
        if cut is None:
            c_theta = len(rows)
        else:
            c_theta = len([row for row in rows if int(identities[row]) < cut])

        floor = min(k_min, cap)
        m = min(cap, max(floor, c_theta))
        return rows[: min(m, len(rows))]

    def served_points(self, tile: int, **params) -> list[int]:
        """[`served_rows`] as position **codes** in served order, which is what the wire carries.

        The differential compares by position rather than by identity deliberately: agreement then
        never depends on either side *interpreting* an identifier, only on both selecting the same
        items. It compares the **list**, not a set or a multiset — both sides are in ascending
        `tessera_id` order and contracts §2.6 makes that order part of the payload contract (see
        [`served_rows`]), so list equality is strictly stronger for free.

        A `u64` code, not a rounded `(x, y)` pair: the comparison is now exact. The old one carried
        an `f32` tolerance, which is what an integer identity of position removes.
        """
        return [self.position_codes[row] for row in self.served_rows(tile, **params)]

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

        cut = theta_cut(v_total, m_target, self.n_occ)
        if cut is None:
            c_theta = len(ordered)
        else:
            c_theta = len([row for row in ordered if int(identities[row]) < cut])
        m = min(cap, max(min(k_min, cap), c_theta))
        return rows[: min(m, len(rows))]

    def first_k_points(self, tile: int, **params) -> list[int]:
        return [self.position_codes[row] for row in self.first_k_rows(tile, **params)]


# ---------------------------------------------------------------------------------------------
# Free functions — the single-tile entry points, and θ's anchor
# ---------------------------------------------------------------------------------------------


def counts(
    bundle: Bundle,
    mask: set[int],
    view_id: str,
    zoom: int,
    bbox: tuple[float, float, float, float],
) -> dict[int, int]:
    """`{tile: count of mask-visible rows}` for every depth-`zoom` tile overlapping `bbox`.

    The frame is the **view's** (decision 0040), asked for by id: a bbox decoded against another
    view's extent names different ground, and on a multi-view bundle `bundle.extent` has no answer
    to give at all.
    """
    selection = Selection(bundle, mask, view_id, zoom)
    return selection.counts_for(morton.tiles_for_bbox(bbox, zoom, bundle.extent_of(view_id)))


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


def visible_total(bundle: Bundle, mask: set[int], view_id: str) -> int:
    """The viewer's total visible count over the whole view — θ's anchor input.

    Computed independently from the segment and the pairs-derived mask, deliberately **not** read
    back from a server response (see the module doc).
    """
    return sum(1 for entity in bundle.row_entity_ids(view_id) if entity in mask)
