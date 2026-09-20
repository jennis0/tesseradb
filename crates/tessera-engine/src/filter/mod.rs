//! Attribute filtering: the entity-space operand, and the candidate it is evaluated under.
//!
//! `filter-index.md` owns the artefact and `filter-surface.md` the query surface. This module is
//! the seam between them — it holds the bundle's filter columns and turns a request's operand into
//! an entity-space bitmap.
//!
//! # The candidate is the composed verdict, and that is the whole safety argument
//!
//! A scan returns whatever its candidate contained. The session's **fragment** is not that set: a
//! suppression never touches the artefact (write-path §5.4, Rule S) and a deletion does not until
//! the fold executes it, so the fragment still holds entities the viewer must not see. Scanning
//! under it would resurrect every one of them, silently and with the right-looking shape.
//!
//! So [`candidate`] composes first — `fragment ∖ denied`, plus the buffered entities whose verdict
//! passes — and the scan runs under *that*. An earlier design reached the same place from the other
//! side, composing the *result* because the operand had been evaluated unmasked. Masked evaluation
//! moves the obligation earlier, which is both simpler and stricter: a result that was never
//! scanned cannot be forgotten to be composed.
//!
//! # Why the work carries no timing channel — and the one column where it does, by declaration
//!
//! A scan walks the candidate and tests the values it selects, so the work is a function of
//! `(candidate, column)` and never of the value sought. A value the principal cannot see costs what
//! a value that does not exist costs — per-point-attributes §3.8's requirement that the two be
//! indistinguishable *in work*, obtained structurally rather than by padding.
//!
//! **A category's derived postings answer `eq` and `in` where, and only where, the column's
//! vocabulary is `visibility = "public"`** (decision 0063). Postings resolve over the whole corpus and
//! are then intersected with the candidate, where the scan takes the candidate as its input — so
//! their work is a function of the *value named*. `probes/2026-08-08-filter-layout/` arm 9 measures
//! a hidden, scattered 10⁷-member value at **2.1 ms** intersected where an absent value costs
//! **0.000 ms**: a scattered value's members meet every container even when no bits do. Under
//! `derived` that difference is a disclosure of exactly what the declaration withholds, so a
//! `derived` column keeps the scan. Under `public` the value set is served to every principal
//! alike by `/v1/categories`, so the timing distinguishes only a fact the client already holds —
//! registered as leak-register row **C24**.
//!
//! **The route is fixed at open from the declaration**, never chosen per request, per principal or
//! from a statistic: §8.2 forbids a statistics-driven route because it makes execution time a
//! function of how much the principal can see. [`crate::filter::columns::Layers::route`] is therefore a field, not an
//! argument.
//!
//! **Postings cover the base build and nothing since**, so the routed answer is
//! `postings ∩ candidate` unioned with a *scan* of every extent layer. Answering from the postings
//! alone would omit every entity ingested since the build — narrower, safe under **I12**, and
//! indistinguishable from a correct answer, which is the failure this subsystem exists to avoid.
//!
//! # The keyword family: one dictionary per layer, and the needle resolved inside it
//!
//! A `keyword` column stores a `u32` **ordinal** per present entity and, beside it, that layer's own
//! front-coded sorted dictionary of the distinct values the layer holds (records §4.3). Every fast
//! operator is then an ordinal question, answered by the same fixed-width scan the numeric families
//! use: `eq` resolves the needle to one ordinal, `in` to a list of them, and `prefix` — sortedness
//! being the reason the dictionary is sorted at all — to a contiguous ordinal range the range scan
//! already knows how to test.
//!
//! **The resolve is per layer, against that layer's own dictionary.** An ordinal is a position in
//! one dictionary and means nothing outside it: the base build and every flush extent number their
//! own keys, so one string is a different ordinal in each, and reading one layer's ordinals against
//! another's dictionary is a recolouring with no symptom (`tessera_filter::SortedDict`'s module
//! doc). [`crate::filter::columns::Layer`] therefore holds the value column and its dictionary together, and an ordinal
//! produced by a resolve never outlives the single scan it was made for — it is never stored, never
//! served, and never compared against an ordinal from elsewhere.
//!
//! **A needle no dictionary resolves is scanned for anyway, and that is security-bearing.** The
//! miss becomes [`crate::filter::scan::keyword::NO_SUCH_ORDINAL`], which no slot can hold, and the scan runs over the whole
//! candidate exactly as it would for a needle that resolved. Returning early instead would make
//! *no item has this value* measurably cheaper than *some do* — a timing channel about content the
//! principal cannot see (records §4.3; per-point-attributes §3.8, whose rule is that the two be
//! indistinguishable in outcome **and in work**). [`crate::filter::scan::keyword::OrdinalPredicate`] is the shape that keeps the
//! early return out: it is total, it has no "matches nothing, so skip the scan" variant, and every
//! arm of [`crate::filter::scan::keyword::scan_ordinals`] runs a scan.
//!
//! The shape is not the whole assurance, because a short circuit can be added above it and still
//! answer correctly. So the rule is also **asserted in work**: `keyword_tests` compares the slots a
//! resolving needle traverses against a non-resolving one over the same candidate and layers, which
//! records §10 names as the conformance suite's one deliberate work assertion. It catches the
//! version no answer-level test can — a resolve that skips a layer whose dictionary does not hold
//! the needle, which returns exactly the right entities for less work.
//!
//! **`contains` has two routes and a crossover that reads no data.** The broad route walks the
//! dictionary, decodes and substring-searches **every** key whatever the needle — front coding
//! elides shared prefixes, so a substring can span an elided one — and scans for the ordinals it
//! collected. The narrow route takes each candidate entity's ordinal and probes the dictionary for
//! that one key. [`crate::filter::scan::keyword::contains_route`] chooses between them from the candidate's cardinality and the
//! layer's dictionary size and nothing else: the first is the principal's own quantity, which they
//! can compute for themselves, and the second is a property of the bundle, identical for every
//! principal — §8.2's admissible class, never a statistic about what the principal's data contains.
//!
//! # A column is layers, because the corpus grows and the build's column does not
//!
//! The batch build writes a column covering `[0, entity_id_high_water)`, and every flush since has
//! published entities above it. Each flush therefore appends an **extent** — its own entities'
//! values with its own presence bitmap (`filter-index.md` §2.1, §2.5) — and a column here is the
//! base plus every live extent, scanned in turn and unioned.
//!
//! **The layers are disjoint in entity space and that is checked, not assumed.** Entity ids are
//! permanent and issued from the high-water (**I9**), so a flush can only add entities no earlier
//! layer holds; [`FilterColumns::compose`] refuses an extent that overlaps what is already
//! composed, because two layers claiming one entity would make it match both values, and a filter
//! naming either would return it. That is a wrong answer with no symptom, so it is a refusal at
//! open rather than a comment.
//!
//! Composition is per **generation**, not per request: a published flush builds the next
//! `FilterColumns` from the live one by pushing a pointer, and the per-request cost is one scan per
//! layer over a candidate that has already been intersected with the layer's presence. So the work
//! stays a function of `(candidate, column)` — the number of layers is a property of the bundle,
//! not of what is being asked for.
//!
//! # What is still answered short, and why that one is the design
//!
//! A **buffered** entity — accepted, acked, not yet flushed — is in the candidate and in no layer,
//! so it matches no predicate. `filter-index.md` §5 rules on that directly: a buffered entity has
//! no row, the entity-space verbs under-report until its flush, and under-reporting narrows `M_sel`
//! and is safe under **I12**. It is a bounded lag measured in one flush interval, not a coverage
//! cliff that never closes, which is what the refusal this composition replaced was answering.
//! The row-space route below under-reports the same entities for the same reason — a buffered
//! entity has no row for the hot column to hold — so the two routes cannot disagree about them.
//!
//! # The row-space operand (decision 0068), and how this module routes a tree
//!
//! A column with `render = true` is filterable **over the request's own rows**, against the hot
//! column in `columns.arrow`. Such a leaf produces no entity-space bitmap at all; it is evaluated
//! in `viewport.rs` over the request's merged tile ranges, and the answer is exact only over that
//! domain (`FilterRows::Viewport`).
//!
//! **Two families reach that route and they say "no value" differently.** A category reserves code
//! 0 out of its vocabulary, so the hot column itself carries the absence. A number, a datetime and
//! a bool have no spare value — the hot column is non-nullable and an absent one is written as the
//! type's zero, which is an ordinary value — so their absence is decision 0064's presence bitmap
//! beside the column, read by the scan and never inferred from the stored bytes. Which rule
//! applies is the column's [`Placement::family`], carried into the routed tree rather than guessed
//! from the width, because a rendered `u8` category and a rendered `u8` number are the same bytes.
//!
//! [`FilterColumns::evaluate_routed`] is the seam. It routes each leaf by the column's
//! [`Placement`] — entity space, row space, or both — and where a column affords both, by the
//! caller's route preference, which `viewport.rs` derives from 0068's rule:
//! **row space while `rows_in_ranges ≤ |M_auth|`, entity space past it** — both quantities the
//! caller could compute, never a statistic about the principal's data (§8.2). A tree whose every
//! leaf routes entity-space evaluates here exactly as [`FilterColumns::evaluate`] always has; a
//! tree with any row-space leaf comes back as a [`RowExpr`]: its maximal entity-space sub-trees
//! already evaluated to bitmaps **under the composed candidate**, its row-space leaves left for
//! the per-tile evaluation, to be crossed once and combined in row space (0062's tree, one
//! crossing per request — placement memo §2.2).
//!
//! **The candidate is the composed verdict — the fragment with the overlay applied — never the
//! raw fragment, and for the row-space half that is a property of consumption, stated here
//! bindingly.** Suppressions touch no artefact (write-path §5.4, Rule S), so the hot column still
//! holds a suppressed entity's row and value, and a row-space leaf tests it like any other row.
//! What keeps it out of every viewport, count and record is that a row-space result enters the
//! request **only** through `EffectiveMask::with_filter`, whose every consumer intersects it with
//! the composed mask — the filter is applied last, by intersection, on top of
//! `base ∖ minus ∪ plus` (`compose.rs`) — and the entity-space sub-trees are evaluated under
//! [`candidate`], which subtracts the overlay before any scan runs. A route evaluated under
//! anything less would silently resurrect a suppressed entity (records §6, review N2);
//! `tests/filtering.rs`'s suppression differential is the proof.

mod columns;
mod declared;
mod error;
mod expr;
mod membership;
mod scan;
#[cfg(test)]
mod test_support;

use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tessera_types::TermId;

use crate::compose::verdict;
use tessera_authz::fragment::FrozenFragment;
use tessera_lifecycle::buffer::IngestBuffer;
use tessera_lifecycle::overlay::Overlay;

pub use columns::successor::{
    CoalescedTextWindow, CoalescedWindow, PublishedExtent, TextExtentPaths,
};
pub use columns::{FilterColumns, ValueLayers};
pub(crate) use declared::{
    blob_resident, carries_live_view, owes_postings, owes_value_column, scoped_owes_postings,
    scoped_visibility_of,
};
pub use declared::{
    extent_column_name, is_filterable, scoped_column_name, scoped_has_value_column,
    scoped_is_filterable, Family, Placement, PIN,
};
pub use error::{ComposeError, FilterError};
pub use expr::{
    FilterExpr, FilterOperand, MemberOfLeaf, MemberResolver, RegionLeaf, RegionResolver,
    RoutedFilter, RowExpr, RowLeafResolvers, MAX_FILTER_DEPTH, MEMBER_OF_COLUMN, REGION_COLUMN,
    UNRESOLVABLE_VALUE,
};
pub use membership::CategoryMembership;

/// The operand value types, re-exported so a caller building a [`FilterOperand`] needs no
/// dependency on the filter crate — `check-layers.sh` denies `tessera-server` that edge, to keep the
/// server on engine API types only, and an operand's *values* are part of this crate's API surface
/// even though the column they are compared against is not.
pub use tessera_filter::{Endpoint, Scalar};
/// How a bound is narrowed to the column's own type, owned by the filter crate and called by both
/// routes: the entity-space scan there, the row-space scan in [`crate::viewport`].
pub(crate) use tessera_filter::{as_f64, narrow_hi, narrow_lo, NativeBound, Narrowed};

/// The entity-space set a filter may be evaluated over: the session's fragment with the deny state
/// composed in, plus the buffered entities whose verdict passes.
///
/// **Not the fragment.** See this module's header: the fragment still contains suppressed and
/// deleted-but-unfolded entities, and a scan returns whatever its candidate held.
///
/// The buffer walk is bounded by the buffer, not by the corpus — a buffered entity is one accepted
/// since the last flush — and `verdict` is the *same* function the row-space composition calls, so
/// the two cannot drift about what a given entity's disposition is.
pub fn candidate(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
) -> Bitmap {
    let mut live = fragment.view().andnot(&overlay.denied());
    for (&entity, _) in buffer.iter() {
        if let Some(true) = verdict(overlay, buffer, satisfied, entity) {
            // The allocator caps entity ids at `u32::MAX` (I9's assignment is a position in the
            // signature order), so this cannot truncate; asserting it here rather than casting
            // keeps the cap a checked property at the one place entity space meets a bitmap.
            let raw = u32::try_from(entity.raw()).expect("entity ids are bounded by the allocator");
            live.add(raw);
        }
    }
    live
}
