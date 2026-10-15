//! Where a predicate level's membership comes from.

use std::sync::Arc;

use mosaica_lifecycle::membership::{ArtifactRecord, ArtifactStore};

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
    /// **The inverse of the rule the mint uses** (`mosaica_types::layer::attribute_value_key`), and
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
    pub segments: &'a [(&'a mosaica_store::read::SegmentData, u32)],
    /// The generation's whole row count, base and extents — what the joined form is sized to.
    pub total_rows: u32,
}

/// One point shard's slice of a level, walked over every artifact the level holds. The open seeds
/// a slice record only where the shard holds a member or a generating-set member of the artifact,
/// or where the artifact borrows its target's membership; any other artifact is walked here with
/// an empty membership and empty generating sets, which is what its slice record would hold.
pub(crate) struct LevelSlice<'a> {
    slice: &'a ArtifactStore,
    records: &'a ArtifactStore,
    layer: &'a str,
    level: u32,
    /// Per number of contents, an artifact of which the shard holds nothing.
    empty: Vec<ArtifactRecord>,
}

impl<'a> LevelSlice<'a> {
    /// `slice` is a shard's slice of `records`, or `records` itself.
    pub(crate) fn new(
        slice: &'a ArtifactStore,
        records: &'a ArtifactStore,
        layer: &'a str,
        level: u32,
    ) -> Self {
        let empty = match std::ptr::eq(slice, records) {
            true => Vec::new(),
            false => {
                let most = records
                    .level(layer, level)
                    .map(|(_, record)| record.contents.len())
                    .max()
                    .unwrap_or(0);
                (0..=most).map(nothing_held).collect()
            }
        };
        LevelSlice {
            slice,
            records,
            layer,
            level,
            empty,
        }
    }

    /// The shard's slice itself.
    pub(crate) fn store(&self) -> &'a ArtifactStore {
        self.slice
    }

    /// Every artifact of the level, as [`ArtifactStore::level`] walks it.
    pub(crate) fn level(&self) -> impl Iterator<Item = (u32, &ArtifactRecord)> {
        self.held(self.records.level(self.layer, self.level))
    }

    /// The artifacts of the level drawn in `view`, as [`ArtifactStore::level_in_view`] walks them.
    pub(crate) fn in_view<'v>(
        &'v self,
        view: &'v str,
    ) -> impl Iterator<Item = (u32, &'v ArtifactRecord)> {
        self.held(self.records.level_in_view(self.layer, self.level, view))
    }

    fn held<'v>(
        &'v self,
        records: impl Iterator<Item = (u32, &'v ArtifactRecord)>,
    ) -> impl Iterator<Item = (u32, &'v ArtifactRecord)> {
        let slots = self.slice.slots(self.layer, self.level);
        let whole = self.empty.is_empty();
        records.map(move |(ordinal, record)| match whole {
            true => (ordinal, record),
            false => (
                ordinal,
                slots
                    .get(ordinal as usize)
                    .and_then(Option::as_deref)
                    .unwrap_or(&self.empty[record.contents.len()]),
            ),
        })
    }
}

/// An artifact with `contents` contents of which a shard holds nothing. Only its membership and
/// generating sets are read: a slice's form takes the level's records from the whole store.
fn nothing_held(contents: usize) -> ArtifactRecord {
    ArtifactRecord {
        entity: mosaica_types::EntityId::new(0),
        key: None,
        view: None,
        incarnation: mosaica_types::view::DECLARED_INCARNATION,
        members: croaring::Bitmap::new().into(),
        declares_members: None,
        contents: (0..contents)
            .map(|_| mosaica_lifecycle::membership::ContentSet {
                values: None,
                digest: [0; 32],
                generated_from: croaring::Bitmap::new(),
                cardinality: 0,
            })
            .collect(),
        attached_to: None,
        parents: Vec::new(),
        access: Vec::new(),
    }
}
