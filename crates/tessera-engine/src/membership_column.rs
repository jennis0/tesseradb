//! The per-point membership column (D12): for each served point and each layer the response
//! served artifacts from, the `tessera_id` of the **deepest served** artifact the point belongs
//! to — in *this* response — or null (`client-components.md` §5.10, decision 0099).
//!
//! **Bounded to the response's own artifacts frame, structurally.** The resolver is handed the
//! served set after it is settled — after the verdicts, the cut and the dependent drop — and
//! nothing else: it holds no route to an artifact the response withheld, so the column can name
//! only an identifier the artifacts frame already carries. That is the same construction the
//! parent identifier takes (architecture Appendix C, C29), inverted from artifact→artifact to
//! point→artifact. Deepest served rather than the leaf is what keeps finer structure out: a point
//! whose leaf cluster was cut to its parent names the parent, and says nothing about the leaf.
//!
//! **Both axes of the value are already disclosed.** The point is one the selection served — after
//! masking, as every point is (I7) — and the artifact passed its own criterion against this
//! principal's mask. What the column adds is the relation between them, which the served hull or
//! box already draws for a layer declaring one; for a layer declaring neither it is the first
//! time a viewer learns which of two served clusters a served point sits in.
//!
//! # Two routes, one answer
//!
//! A level is served in one of two families of layout (decision 0094), and the column is read off
//! whichever the level has, so a request never pays a second structure for it:
//!
//! - **Row-major** ([`crate::row_column`]): the column holds each row's leaf label (or list of
//!   labels), so a served point's leaf is one read, and the walk up the lineage stops at the
//!   first served ancestor — a few steps per point.
//! - **Artifact-major** and **spatial** ([`crate::artifacts::MembershipRows`],
//!   [`crate::ranges`]): only artifact→rows exists. The inversion is done **by bitmap
//!   intersection, never by testing points against artifacts**: the response's gathered rows
//!   become one bitmap, each served artifact's rows are intersected with it — O(containers
//!   touched), not O(points × artifacts) — and every row hit takes the ordinal if it is deeper
//!   than what it holds. Served is bounded by the cut and the budget; rows by the viewport and `k`.
//!
//! The two agree ordinal for ordinal wherever a level's memberships nest along its lineage, which
//! a treed layer's do by construction; `tests/membership_column.rs` asserts it over the same
//! corpus under both layouts.
//!
//! **"Deepest" across levels is the finest level first.** A tiered layer's edges run from a
//! coarser level to a finer one and a stacked layer's levels are independent analyses; in both,
//! a higher level index is the finer resolution, so a point held by served artifacts at two
//! levels names the one at the higher index, and within a level the one deeper in the lineage.
//! A treed layer sits entirely at level 0 and has only the second half.
//!
//! **Rows above a level's base** — points ingested since the last fold — resolve through a
//! row-major label column's live tail and are null on an artifact-major level, whose row form is
//! the base projection. That is the same edge the masked count already has on that layout, and
//! it closes at the fold.

use std::collections::HashMap;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_types::TesseraId;

use crate::artifacts::ArtifactRows;
use crate::cut::Lineage;

/// One layer's membership column for a chunk of points: parallel to the chunk's `tessera_ids`,
/// `None` where no served artifact of the layer holds the point.
#[derive(Debug, Clone, PartialEq)]
pub struct MembershipColumn {
    pub layer: String,
    pub ids: Vec<Option<u64>>,
}

/// One level this response served artifacts from, as the resolver reads it.
pub(crate) struct ServedLevel {
    pub level: u32,
    pub rows: Arc<ArtifactRows>,
    pub lineage: Arc<Lineage>,
    /// Ordinal → identifier, for **exactly** the artifacts of this level in the response's
    /// artifacts frame. Filled after the dependent drop, so a label whose target went is not here.
    pub served: HashMap<u32, TesseraId>,
}

/// One layer this response served artifacts from, with its served levels.
pub(crate) struct ServedLayer {
    pub name: String,
    pub levels: Vec<ServedLevel>,
}

/// The resolved column for every served layer, aligned to one ascending row list.
pub(crate) struct Resolved {
    /// The response's gathered rows, ascending and distinct — the index the columns are aligned to.
    rows: Vec<u32>,
    /// Per served layer, per row of `rows`: the deepest served artifact's identifier.
    columns: Vec<(String, Vec<Option<u64>>)>,
}

impl Resolved {
    /// Resolve the response's rows against its served layers.
    ///
    /// `rows` is every row the emit pass will gather, in any order; it is sorted here once. Layers
    /// with no served level contribute no column — the wire's rule that an absent column and an
    /// all-null one say the same thing.
    pub fn new(mut rows: Vec<u32>, layers: &[ServedLayer]) -> Self {
        rows.sort_unstable();
        rows.dedup();
        let mut bitmap = Bitmap::new();
        bitmap.add_many(&rows);
        let columns = layers
            .iter()
            .filter(|layer| layer.levels.iter().any(|l| !l.served.is_empty()))
            .map(|layer| (layer.name.clone(), resolve_layer(&rows, &bitmap, layer)))
            .collect();
        Resolved { rows, columns }
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// The membership columns for one chunk of gathered rows, in the resolver's layer order.
    pub fn columns_for(&self, chunk_rows: &[u32]) -> Vec<MembershipColumn> {
        self.columns
            .iter()
            .map(|(layer, ids)| MembershipColumn {
                layer: layer.clone(),
                ids: chunk_rows
                    .iter()
                    .map(|row| {
                        // Every gathered row was in the list `new` was given, so a miss here is
                        // a caller bug; null is the fail-closed reading of it.
                        self.rows
                            .binary_search(row)
                            .ok()
                            .and_then(|i| ids[i])
                    })
                    .collect(),
            })
            .collect()
    }

    /// Empty columns in the resolver's layer order — the seed for a chunk with no points yet.
    pub fn empty_columns(&self) -> Vec<MembershipColumn> {
        self.columns
            .iter()
            .map(|(layer, _)| MembershipColumn {
                layer: layer.clone(),
                ids: Vec::new(),
            })
            .collect()
    }
}

/// The rank a candidate wins by: the finest level first, then the deepest in its lineage.
type Rank = (u32, u32);

fn resolve_layer(rows: &[u32], bitmap: &Bitmap, layer: &ServedLayer) -> Vec<Option<u64>> {
    let mut best: Vec<Option<(Rank, u64)>> = vec![None; rows.len()];
    let mut consider = |i: usize, rank: Rank, id: TesseraId| match best[i] {
        Some((held, _)) if held >= rank => {}
        _ => best[i] = Some((rank, id.raw())),
    };
    for level in &layer.levels {
        if level.served.is_empty() {
            continue;
        }
        let depth = |ordinal: u32| level.lineage.depth(ordinal);
        match level.rows.column() {
            // Row-major: the point's leaf, then up to the first served ancestor.
            Some(column) => {
                for (i, &row) in rows.iter().enumerate() {
                    column.for_each_label(row, |leaf| {
                        if let Some(hit) = level
                            .lineage
                            .nearest(leaf, |o| level.served.contains_key(&o))
                        {
                            consider(i, (level.level, depth(hit)), level.served[&hit]);
                        }
                    });
                }
            }
            // Artifact-major or spatial: each served artifact's rows against the response's,
            // one intersection per served artifact.
            None => {
                for (&ordinal, &id) in &level.served {
                    let members = match level.rows.ranges() {
                        Some(ranges) => ranges.rows(ordinal),
                        None => match level.rows.get(ordinal) {
                            Some(members) => members.clone(),
                            None => continue,
                        },
                    };
                    let rank = (level.level, depth(ordinal));
                    for row in members.and(bitmap).iter() {
                        if let Ok(i) = rows.binary_search(&row) {
                            consider(i, rank, id);
                        }
                    }
                }
            }
        }
    }
    best.into_iter().map(|b| b.map(|(_, id)| id)).collect()
}
