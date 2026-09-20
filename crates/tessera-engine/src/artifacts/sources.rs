//! Where a predicate level's membership comes from.

use std::sync::Arc;





/// **Where a predicate level's membership comes from**, resolved against this generation — the
/// pieces [`ArtifactProjections::get_or_build`](super::ArtifactProjections::get_or_build) needs and cannot reach itself.
///
/// **`None` is an enumerated level**, and that is not a fallback: such a level's membership is
/// stored, so there is no rule to evaluate and nothing here to supply.
pub enum PredicateSource<'a> {
    /// `membership = { attribute = f }` — the indexed column `f`, addressed by entity.
    Attribute(AttributeSource<'a>),
    /// `membership = "spatial"` — the level's held shapes and the segments whose resolutions the
    /// form is assembled from, once (`crate::shapes`).
    Spatial(SpatialSource<'a>),
}

/// The indexed column an attribute layer's predicate reads, and the rule that turns one of its
/// values into one of the layer's artifacts.
pub struct AttributeSource<'a> {
    /// The column's value layers, base first (`crate::filter::ValueLayers`). The **base** answers
    /// for the rows the row space's base covers; the **extents** answer for everything a flush has
    /// published since, which is what makes an ingested point count on the next request.
    pub values: crate::filter::ValueLayers<'a>,
    /// The code an artifact's key stands for — a vocabulary binding where the column has one, and
    /// the key's own decimal spelling where it has not.
    ///
    /// **The inverse of the rule the mint uses** (`tessera_types::layer::attribute_value_key`), and
    /// it is a closure rather than a map because the two callers hold different things: the build
    /// holds a schema and the serving path holds a live generation's bindings.
    pub code_of_key: &'a dyn Fn(&str) -> Option<u32>,
}

/// The held structures a spatial level's form is assembled from when it is built, and what it is
/// assembled over. Built once; from then on the form is maintained by the publications that move
/// it, as a stored level's is.
pub struct SpatialSource<'a> {
    /// The level's shapes, index and staged pieces, at this generation's level version.
    pub level: Arc<crate::shapes::ShapeLevel>,
    /// This generation's segments and their row bases, base first.
    pub segments: &'a [(&'a tessera_store::read::SegmentData, u32)],
    /// The generation's whole row count, base and extents — what the joined form is sized to.
    pub total_rows: u32,
}
