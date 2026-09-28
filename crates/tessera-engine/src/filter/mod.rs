//! Attribute filtering: the entity-space operand, and the candidate it is evaluated under.
//!
//! This module holds the bundle's filter columns and turns a request's operand into an entity-space
//! bitmap, or a routed tree for `viewport.rs` where a leaf answers over the request's own rows.
//!
//! # The candidate
//!
//! A scan returns whatever its candidate contained. The session's fragment is not that set: a
//! suppression never touches the artefact and a deletion does not until the fold executes it, so
//! the fragment still holds entities the viewer must not see. [`candidate`] composes the fragment
//! with the denied set removed, plus the buffered entities whose verdict passes, and every scan
//! runs under that composed set.
//!
//! # No timing channel
//!
//! A scan walks the candidate and tests the values it selects, so the work is a function of
//! `(candidate, column)` and never of the value sought: a needle no dictionary resolves is still
//! scanned for, and a keyword ordinal is resolved and used inside one scan rather than compared
//! across layers.
//!
//! # The route
//!
//! A category's derived postings answer `eq` and `in` only where the column's vocabulary is
//! `public`, since postings work is a function of the value named. The route is fixed at open from
//! the declaration, never per request or per statistic. Postings cover the base build only; every
//! extent layer is scanned and unioned with them.
//!
//! # The keyword dictionary
//!
//! A keyword column carries an ordinal per present entity and, beside it, that layer's own sorted
//! dictionary of the distinct values the layer holds. An ordinal is a position in one dictionary
//! and means nothing outside it, so the resolve runs inside each layer against its own dictionary,
//! and the result is never carried to another layer, stored or served.
//!
//! # Layers and extents
//!
//! The batch build writes a column covering the entities present at build time, and every flush
//! since appends an extent covering the entities it added. A column here is the base plus every
//! live extent, scanned in turn and unioned. The layers are disjoint in entity space, checked
//! rather than assumed: two layers claiming one entity would make it match both values. A buffered
//! entity is in the candidate and in no layer until its flush, so it matches no predicate and
//! under-reports rather than over-reports.
//!
//! # The row route
//!
//! A column with `render = true` may also be filtered over the request's own rows, against the hot
//! column. [`FilterColumns::evaluate_routed`] routes each leaf by the column's [`Placement`]; a
//! tree with any row-space leaf comes back as a [`RowExpr`] for `viewport.rs` to cross with the
//! request's tile ranges. A row-space result enters a request only by intersection with the
//! composed mask.

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

use crate::compose::verdict_of;
use tessera_authz::fragment::FrozenFragment;
use tessera_lifecycle::buffer::IngestBuffer;
use tessera_lifecycle::overlay::Overlay;

pub use columns::successor::{
    CoalescedTextWindow, CoalescedWindow, OpenedExtent, TextExtentPaths,
};
pub use columns::{FilterColumns, PartitionExtents, ValueLayers};
pub(crate) use columns::{open_entity_terms_stack, open_record_stack};
pub(crate) use declared::{
    blob_resident, carries_live_view, owes_postings, owes_value_column, scoped_owes_postings,
    scoped_visibility_of,
};
pub use declared::{
    extent_column_name, is_filterable, operands_of, scoped_column_name, scoped_has_value_column,
    unique_only,
    FieldHomes, scoped_is_filterable, Family, Placement, PIN,
};
pub use error::{ComposeError, FilterError};
pub use expr::{
    FilterExpr, FilterOperand, MemberOfLeaf, MemberResolver, RegionLeaf, RegionResolver,
    RoutedFilter, RowExpr, RowLeafResolvers, UniqueResolver, MAX_FILTER_DEPTH, MEMBER_OF_COLUMN,
    REGION_COLUMN, UNRESOLVABLE_VALUE,
};
pub use membership::{
    CategoryCounts, CategoryMembership, CountCodes, EntityCodes, VisitBuffered, VisitCodes,
};

/// The operand value types, re-exported so a caller building a [`FilterOperand`] needs no
/// dependency on the filter crate: `check-layers.sh` denies `tessera-server` that edge, and an
/// operand's values are part of this crate's API surface even though the column they are compared
/// against is not.
pub use tessera_filter::{Endpoint, Scalar};
/// How a bound is narrowed to the column's own type, owned by the filter crate and called by both
/// routes: the entity-space scan there, the row-space scan in [`crate::viewport`].
pub(crate) use tessera_filter::{as_f64, narrow_hi, narrow_lo, NativeBound, Narrowed};

/// The entity-space set a filter may be evaluated over: the session's fragment with the deny state
/// composed in, plus the buffered entities whose verdict passes.
///
/// Not the fragment: the fragment still contains suppressed and deleted-but-unfolded entities, and
/// a scan returns whatever its candidate held.
///
/// The buffer walk is bounded by the buffer, not by the corpus, and `verdict` is the same function
/// the row-space composition calls, so the two cannot disagree about a given entity's disposition.
pub fn candidate(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
) -> Bitmap {
    let mut live = fragment.view().andnot(&overlay.denied());
    add_buffered(&mut live, satisfied, overlay, buffer, |_| true);
    live
}

/// [`candidate`]'s members among `within`, at the cost of `within` and the buffer rather than of
/// the whole fragment.
pub fn candidate_within(
    fragment: &FrozenFragment,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    within: &Bitmap,
) -> Bitmap {
    let mut live = fragment.view().and(within);
    live.andnot_inplace(&overlay.denied());
    add_buffered(&mut live, satisfied, overlay, buffer, |entity| within.contains(entity));
    live
}

/// Add to `live` each buffered entity `keep` names whose verdict passes.
fn add_buffered(
    live: &mut Bitmap,
    satisfied: &FxHashSet<TermId>,
    overlay: &Overlay,
    buffer: &IngestBuffer,
    keep: impl Fn(u32) -> bool,
) {
    for (&entity, item) in buffer.iter() {
        // Entity ids are bounded by the allocator, so this cannot truncate; asserting it here
        // rather than casting keeps the cap a checked property at the one place entity space
        // meets a bitmap.
        let raw = u32::try_from(entity.raw()).expect("entity ids are bounded by the allocator");
        if keep(raw) && verdict_of(overlay, satisfied, entity, Some(item)) == Some(true) {
            live.add(raw);
        }
    }
}
