//! What one request resolves once: the view it is served from.

use super::*;

/// One request's view, resolved once and passed whole.
///
/// Every field is fixed for the request from the moment the view resolves; a callee that would
/// otherwise take four or more of them takes this instead.
///
/// A struct of borrows, except each side's segment list, which `segments_with_row_bases` built for
/// the request, and an `Arc` of its geometry.
///
/// The composed mask is not a field. A filtered request holds two at once — the pre-filter mask
/// the region and `member_of` resolvers close over, and the narrowed one the sweep reads — so
/// which of them a callee is given stays visible at the call site.
pub(crate) struct ServedView<'a> {
    pub(crate) session: &'a Session,
    pub(crate) generation: &'a Generation,
    pub(crate) name: &'a str,
    /// One per shard holding the view, ascending by shard, whether or not the viewer sees
    /// anything there. A side's index here is its place; every per-side list on the read path is
    /// indexed by place.
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

impl<'a> ServedView<'a> {
    /// The place of `shard`'s side. Panics where the view has none there.
    pub(crate) fn place_of(&self, shard: mosaica_types::ShardId) -> usize {
        self.find_place(shard).expect("a shard holding the view")
    }

    /// The place of `shard`'s side, or `None` where the view has none there.
    pub(crate) fn find_place(&self, shard: mosaica_types::ShardId) -> Option<usize> {
        self.sides
            .binary_search_by_key(&shard, |side| side.shard.id())
            .ok()
    }

    /// The viewer's leaf in the side at `place`, or `None` where `mask` leaves that side out.
    pub(crate) fn leaf<'m>(
        &self,
        mask: &'m ShardedMask,
        place: usize,
    ) -> Option<&'m EffectiveMask> {
        mask.read_leaf(self.sides[place].shard.id())
    }

    /// The sides `mask` reads ([`ShardedMask::filtered_leaves`]), ascending, each with its place
    /// and leaf.
    pub(crate) fn filtered_sides<'s, 'm>(
        &'s self,
        mask: &'m ShardedMask,
    ) -> impl Iterator<Item = (usize, &'s ServedSide<'a>, &'m EffectiveMask)> {
        mask.filtered_leaves().map(move |(shard, leaf)| {
            let place = self.place_of(shard);
            (place, &self.sides[place], leaf)
        })
    }

    /// Each side's locator of its segments' positions, in the order of [`Self::sides`].
    pub(crate) fn locators(&self) -> Vec<crate::derived::RowLocator<'a>> {
        self.sides
            .iter()
            .map(|side| crate::derived::RowLocator::new(side.segments.clone()))
            .collect()
    }
}
