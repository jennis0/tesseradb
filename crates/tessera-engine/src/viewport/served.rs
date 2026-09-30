//! What one request resolves once: the view it is served from.

use super::*;

/// One request's view, resolved once and passed whole.
///
/// Every field is fixed for the request from the moment the view resolves: the session asking, the
/// generation loaded at request start, the view's name and data, its segments with their row bases,
/// its deny mask rows, and what this request's composed mask is. A callee that would otherwise take
/// four or more of them takes this instead.
///
/// A struct of borrows: it owns only the segment list, which is the one `segments_with_row_bases`
/// already built for the request; nothing here is cloned or reference-counted a second time.
///
/// The composed mask is not a field. A filtered request holds two at once — the pre-filter mask
/// the region and `member_of` resolvers close over, and the narrowed one the sweep reads — so
/// which of them a callee is given stays visible at the call site.
pub(crate) struct ServedView<'a> {
    pub(crate) session: &'a Session,
    pub(crate) generation: &'a Generation,
    pub(crate) name: &'a str,
    pub(crate) data: &'a tessera_store::read::ViewData,
    /// Every segment of the view, each with where its rows begin in the view's row space, ascending.
    pub(crate) segments: Vec<(&'a SegmentData, u32)>,
    /// This view's `deleted ∪ suppressed` in row space.
    pub(crate) denied: &'a croaring::Bitmap,
    pub(crate) mask_identity: crate::histogram::MaskIdentity,
}
