//! What one request resolves once: the view it is served from.

use super::*;

/// One request's view, resolved once and passed whole.
///
/// Every field is fixed for the request from the moment the view resolves; a callee that would
/// otherwise take four or more of them takes this instead.
///
/// A struct of borrows: it owns only each side's segment list, which is the one
/// `segments_with_row_bases` already built for the request, and a count on the geometry each side
/// was composed over.
///
/// The composed mask is not a field. A filtered request holds two at once — the pre-filter mask
/// the region and `member_of` resolvers close over, and the narrowed one the sweep reads — so
/// which of them a callee is given stays visible at the call site.
pub(crate) struct ServedView<'a> {
    pub(crate) session: &'a Session,
    pub(crate) generation: &'a Generation,
    pub(crate) name: &'a str,
    /// One per shard holding the view, ascending by shard, whether or not the viewer sees
    /// anything there: every side's geometry was resolved from one generation.
    pub(crate) sides: Vec<ServedSide<'a>>,
    pub(crate) mask_identity: crate::figures::MaskIdentity,
    /// The request's cancellation, which a wait on another request's build observes.
    pub(crate) cancel: Option<crate::CancelToken>,
    /// Whether the request is drawing points, which a masked-count build gives way to.
    pub(crate) turn: crate::figures::DrawingTurn,
}

/// One shard of a request's view.
pub(crate) struct ServedSide<'a> {
    pub(crate) shard: &'a crate::Shard,
    pub(crate) data: &'a mosaica_store::read::ViewData,
    /// Every segment of the view in this shard, each with where its rows begin in the shard's row
    /// space for the view, ascending.
    pub(crate) segments: Vec<(&'a SegmentData, u32)>,
    /// This view's `deleted ∪ suppressed` in the shard's row space.
    pub(crate) denied: &'a croaring::Bitmap,
    /// The grant's fragment and row projection the side's leaf was composed over.
    pub(crate) geometry: Arc<SessionGeometry>,
    /// What a cache keyed on the request's mask reads of this side.
    pub(crate) versions: crate::figures::SideVersions,
}

impl ServedSide<'_> {
    /// The fragment the side's projection was taken over, which answers for the session's
    /// fragment in the shard where it is at the shard's identity and watermark.
    pub(crate) fn fragment(&self) -> &Arc<FrozenFragment> {
        &self.geometry.fragment
    }
}

impl<'a> ServedView<'a> {
    /// The place in [`Self::sides`] of `shard`'s side. Panics where the view has none there.
    pub(crate) fn side_of(&self, shard: mosaica_types::ShardId) -> usize {
        self.sides
            .binary_search_by_key(&shard, |side| side.shard.id())
            .expect("a shard holding the view")
    }

    /// Each side's locator of its segments' positions, in the order of [`Self::sides`].
    pub(crate) fn locators(&self) -> Vec<crate::derived::RowLocator<'a>> {
        self.sides
            .iter()
            .map(|side| crate::derived::RowLocator::new(side.segments.clone()))
            .collect()
    }

    /// The one side of a view of one shard, for a route that does not yet read every shard.
    /// Panics where the view has a second shard.
    pub(crate) fn only_side(&self) -> &ServedSide<'a> {
        assert!(
            self.sides.len() == 1,
            "a route read one side of view '{}' over {} shards",
            self.name,
            self.sides.len()
        );
        &self.sides[0]
    }
}
