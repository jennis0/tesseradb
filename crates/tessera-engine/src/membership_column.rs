//! The per-point membership column (D12): for each served point and each layer the response
//! served artifacts from, the `tessera_id` of the **deepest served** artifact the point belongs
//! to — in *this* response — or null (`client-components.md` §5.10, decision 0099).
//!
//! **Bounded to the response's own artifacts frame, structurally.** The resolver is handed the
//! served set after it is settled — after the verdicts, the cut and the dependent drop — and
//! nothing else: it holds no route to an artifact the response withheld, so the column can name
//! only an identifier the artifacts frame already carries. That is the same construction the
//! parent identifiers take (architecture Appendix C, C29), inverted from artifact→artifact to
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
//!   labels), so a served point's leaf is one read, and the search up the lineage over the
//!   served ancestors — every path of them, on a DAG — is a few steps per point.
//! - **Artifact-major** ([`crate::artifacts::MembershipRows`] — a spatial level's resolved
//!   membership arrives in the same form): only artifact→rows exists. The inversion is done **by
//!   bitmap intersection, never by testing points against artifacts**: the response's gathered
//!   rows become one bitmap, each served artifact's rows are intersected with it — O(containers
//!   touched), not O(points × artifacts) — and every row hit takes the artifact if it outranks
//!   what it holds. Served is bounded by the cut and the budget; rows by the viewport and `k`.
//!
//! The two agree ordinal for ordinal wherever a level's memberships nest along its lineage, which
//! a treed layer's do by construction; `tests/membership_column.rs` asserts it over the same
//! corpus under both layouts, and the tests below over a flat layer with overlapping artifacts
//! and over a DAG.
//!
//! **"Deepest" across levels is the finest level first.** A tiered layer's edges run from a
//! coarser level to a finer one and a stacked layer's levels are independent analyses; in both,
//! a higher level index is the finer resolution, so a point held by served artifacts at two
//! levels names the one at the higher index, and within a level the one deeper in the lineage.
//! A treed layer sits entirely at level 0 and has only the second half.
//!
//! **"Deeper" is the response-local rung, never the stored depth.** The stored depth counts every
//! node of the level, withheld ones included, so ranking by it would order two served artifacts
//! by an artifact the viewer cannot see — `R → X → A` and `R → B`, `X` withheld, a point in all
//! three: by stored depth `A` outranks `B`; with `X` never having existed they tie and the lower
//! identifier wins. The rung is counted over the response's own links (decision 0117 E), so the
//! answer is the same in both worlds.
//!
//! **A tie is broken by the lowest `tessera_id`** (`dag-hierarchies.md` §6). On a tree the deepest
//! served artifact holding a point is unique; on a DAG, and on a flat layer with multi-membership,
//! two served artifacts may hold the point at one depth, and a rule that took whichever the
//! artifact-major route's map iterated first would answer differently from one run to the next
//! and differently from the row-major route. The identifier is the key the frame already
//! carries, rather than an ordinal (C8), and it discloses nothing: both artifacts are in the
//! response and both hold the point.
//!
//! **Rows above a level's base** — points ingested since the last fold — resolve through a
//! row-major label column's live tail and are null on an artifact-major level, whose row form is
//! the base projection. That is the same edge the masked count already has on that layout, and
//! it closes at the fold.

use std::cmp::Reverse;
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
    /// Ordinal → identifier and response-local rung, for **exactly** the artifacts of this level
    /// in the response's artifacts frame. Filled after the dependent drop and after the rungs are
    /// settled, so a label whose target went is not here and the rank reads nothing stored.
    pub served: HashMap<u32, (TesseraId, u32)>,
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
                        self.rows.binary_search(row).ok().and_then(|i| ids[i])
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

/// The rank a candidate wins by: the finest level first, then the deepest **rung** in the
/// response's own forest, then the **lowest** identifier — a total order, so the answer is the
/// same whichever route found the candidates and in whatever order it met them.
type Rank = (u32, u32, Reverse<u64>);

fn resolve_layer(rows: &[u32], bitmap: &Bitmap, layer: &ServedLayer) -> Vec<Option<u64>> {
    let mut best: Vec<Option<Rank>> = vec![None; rows.len()];
    let mut consider = |i: usize, rank: Rank| {
        if best[i].is_none_or(|held| rank > held) {
            best[i] = Some(rank);
        }
    };
    let mut scratch: Vec<u32> = Vec::new();
    for level in &layer.levels {
        if level.served.is_empty() {
            continue;
        }
        let rank_of = |ordinal: u32| -> Option<Rank> {
            level
                .served
                .get(&ordinal)
                .map(|&(id, rung)| (level.level, rung, Reverse(id.raw())))
        };
        match level.rows.column() {
            // Row-major: the point's leaf, then the best-ranked served ancestor-or-self of it
            // over every path. The stored lineage is climbed only to *find* the served
            // ancestors; what ranks them is the response's own rung.
            Some(column) => {
                for (i, &row) in rows.iter().enumerate() {
                    column.for_each_label(row, |leaf| {
                        if let Some((_, rank)) =
                            level.lineage.select_ancestor(leaf, &mut scratch, rank_of)
                        {
                            consider(i, rank);
                        }
                    });
                }
            }
            // Artifact-major: each served artifact's rows against the response's, one
            // intersection per served artifact.
            None => {
                for (&ordinal, &(id, rung)) in &level.served {
                    let Some(members) = level.rows.get(ordinal) else {
                        continue;
                    };
                    let rank = (level.level, rung, Reverse(id.raw()));
                    for row in members.and(bitmap).iter() {
                        if let Ok(i) = rows.binary_search(&row) {
                            consider(i, rank);
                        }
                    }
                }
            }
        }
    }
    best.into_iter()
        .map(|b| b.map(|(_, _, Reverse(id))| id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_column::RowColumn;
    use tessera_types::layer::ServingLayout;

    /// Served artifacts with their response-local rung: the longest chain to each through the
    /// served set alone, which is what the serving path computes over the response's links.
    fn with_rungs(lineage: &Lineage, served: &[(u32, u64)]) -> Vec<(u32, u64, u32)> {
        fn rung(o: u32, lineage: &Lineage, served: &[(u32, u64)]) -> u32 {
            let mut scratch = Vec::new();
            let mut best = 0;
            // The served parents in the response's forest are the nearest served ancestors on
            // every path — what `parent_ids` carries after the cut.
            for &up in lineage.parents_of(o) {
                let mut stack = vec![up];
                while let Some(at) = stack.pop() {
                    if scratch.contains(&at) {
                        continue;
                    }
                    scratch.push(at);
                    if served.iter().any(|&(s, _)| s == at) {
                        best = best.max(rung(at, lineage, served) + 1);
                    } else {
                        stack.extend_from_slice(lineage.parents_of(at));
                    }
                }
            }
            best
        }
        served
            .iter()
            .map(|&(o, id)| (o, id, rung(o, lineage, served)))
            .collect()
    }

    /// Both routes over one level: the artifact-major form of `sets`, and the row-major form
    /// composed from the same memberships — a label column where every row is in one artifact,
    /// a list column otherwise.
    fn both_routes(
        sets: &[Option<&[u32]>],
        row_count: u32,
        lineage: &Arc<Lineage>,
        served: &[(u32, u64)],
    ) -> (ServedLayer, ServedLayer) {
        let served_map = |_: ()| -> HashMap<u32, (TesseraId, u32)> {
            with_rungs(lineage, served)
                .into_iter()
                .map(|(o, id, rung)| (o, (TesseraId::new(id), rung)))
                .collect()
        };
        let artifact_major = Arc::new(ArtifactRows::synthetic(sets, None));
        let overlapping = (0..row_count).any(|row| {
            sets.iter()
                .flatten()
                .filter(|members| members.contains(&row))
                .count()
                > 1
        });
        let layout = if overlapping {
            ServingLayout::RowMajorList
        } else {
            ServingLayout::RowMajorLabel
        };
        let column = RowColumn::compose(artifact_major.membership(), row_count, layout)
            .expect("the memberships compose in the form chosen for them");
        let row_major = Arc::new(ArtifactRows::synthetic(sets, Some(Arc::new(column))));
        let layer = |rows: Arc<ArtifactRows>| ServedLayer {
            name: "layer".into(),
            levels: vec![ServedLevel {
                level: 0,
                rows,
                lineage: Arc::clone(lineage),
                served: served_map(()),
            }],
        };
        (layer(artifact_major), layer(row_major))
    }

    /// The rule, written the obvious way: over the served artifacts holding the row, the deepest
    /// by response-local rung, then the lowest identifier.
    fn expected(
        sets: &[Option<&[u32]>],
        lineage: &Lineage,
        served: &[(u32, u64)],
        row: u32,
    ) -> Option<u64> {
        with_rungs(lineage, served)
            .into_iter()
            .filter(|&(o, _, _)| sets[o as usize].is_some_and(|m| m.contains(&row)))
            .map(|(_, id, rung)| (rung, Reverse(id), id))
            .max()
            .map(|(_, _, id)| id)
    }

    /// **The column does not depend on a withheld artifact.** `R → X → A` and `R → B`, a point
    /// held by all three, `X` withheld, `R`, `A` and `B` served with `id(B) < id(A)`: by stored
    /// depth `A` would win (2 over 1); in the world where `X` never existed `A` and `B` tie at
    /// rung 1 and `B` wins. The same answer in both worlds, on both routes.
    #[test]
    fn a_withheld_node_does_not_rank_a_served_artifact_deeper() {
        const R: u32 = 0;
        const X: u32 = 1;
        const A: u32 = 2;
        const B: u32 = 3;
        let with_x = Arc::new(Lineage::new([
            (R, None::<u32>),
            (X, Some(R)),
            (A, Some(X)),
            (B, Some(R)),
        ]));
        let without_x = Arc::new(Lineage::new([(R, None::<u32>), (A, Some(R)), (B, Some(R))]));
        let sets: [Option<&[u32]>; 4] = [
            Some(&[0, 1, 2]),
            Some(&[0, 1]),
            Some(&[0, 1]),
            Some(&[0, 2]),
        ];
        let sets_without: [Option<&[u32]>; 4] =
            [Some(&[0, 1, 2]), None, Some(&[0, 1]), Some(&[0, 2])];
        let served = [(R, 10), (A, 30), (B, 20)];
        assert_both_routes_agree(&sets, 3, &with_x, &served);
        assert_both_routes_agree(&sets_without, 3, &without_x, &served);
        let rows: Vec<u32> = (0..3).collect();
        let column = |sets: &[Option<&[u32]>], lineage: &Arc<Lineage>| {
            let (artifact_major, row_major) = both_routes(sets, 3, lineage, &served);
            (
                Resolved::new(rows.clone(), &[artifact_major]).columns_for(&rows)[0]
                    .ids
                    .clone(),
                Resolved::new(rows.clone(), &[row_major]).columns_for(&rows)[0]
                    .ids
                    .clone(),
            )
        };
        let (a1, r1) = column(&sets, &with_x);
        let (a2, r2) = column(&sets_without, &without_x);
        assert_eq!(
            a1,
            vec![Some(20), Some(30), Some(20)],
            "row 0 is in A and B at one rung and names the lower id; row 1 is in A alone"
        );
        assert_eq!(a1, a2, "the same answer with and without X, artifact-major");
        assert_eq!(r1, r2, "and row-major");
        assert_eq!(a1, r1);
    }

    fn assert_both_routes_agree(
        sets: &[Option<&[u32]>],
        row_count: u32,
        lineage: &Arc<Lineage>,
        served: &[(u32, u64)],
    ) {
        let (artifact_major, row_major) = both_routes(sets, row_count, lineage, served);
        let rows: Vec<u32> = (0..row_count).collect();
        let a = Resolved::new(rows.clone(), &[artifact_major]);
        let r = Resolved::new(rows.clone(), &[row_major]);
        let a = &a.columns_for(&rows)[0].ids;
        let r = &r.columns_for(&rows)[0].ids;
        let want: Vec<Option<u64>> = rows
            .iter()
            .map(|&row| expected(sets, lineage, served, row))
            .collect();
        assert_eq!(a, &want, "the artifact-major route disagrees with the rule");
        assert_eq!(r, &want, "the row-major route disagrees with the rule");
    }

    /// **A flat layer with overlapping artifacts ties by the lowest identifier**, on both routes.
    /// The artifact-major route iterates a map of the served set, so without the rule the answer
    /// on the overlap was whichever it met first.
    #[test]
    fn a_flat_overlap_names_the_lowest_identifier_on_both_routes() {
        let sets: [Option<&[u32]>; 3] = [Some(&[0, 1, 2, 3]), Some(&[2, 3, 4]), Some(&[3, 5])];
        let lineage = Arc::new(Lineage::new([(0, None::<u32>), (1, None), (2, None)]));
        // Identifiers deliberately out of ordinal order: the lowest id is ordinal 1, not 0.
        let served = [(0, 300), (1, 100), (2, 200)];
        assert_both_routes_agree(&sets, 6, &lineage, &served);
        let (artifact_major, _) = both_routes(&sets, 6, &lineage, &served);
        let rows: Vec<u32> = (0..6).collect();
        let ids = Resolved::new(rows.clone(), &[artifact_major]).columns_for(&rows)[0]
            .ids
            .clone();
        assert_eq!(
            ids,
            vec![
                Some(300),
                Some(300),
                Some(100),
                Some(100),
                Some(100),
                Some(200)
            ],
            "row 2 is in ordinals 0 and 1 and names the lower identifier; row 3 is in all three"
        );
    }

    /// **On a DAG two served artifacts may hold a point at one depth**, and the row-major search
    /// reaches a served ancestor on a second path: `0 → {1, 2} → 3`, memberships closed upward,
    /// so the row-major list column labels a row of `3` with every artifact holding it.
    #[test]
    fn a_dag_ties_by_depth_then_identifier_and_searches_every_path() {
        let lineage = Arc::new(Lineage::dag([
            (0, vec![]),
            (1, vec![0]),
            (2, vec![0]),
            (3, vec![1, 2]),
            (4, vec![1]),
        ]));
        // 3's members are in both 1 and 2; 4's are in 1 only; 0 holds everything and row 9.
        let sets: [Option<&[u32]>; 5] = [
            Some(&[0, 1, 2, 3, 4, 5, 9]),
            Some(&[0, 1, 2, 4, 5]),
            Some(&[0, 1, 2, 3]),
            Some(&[0, 1, 2]),
            Some(&[4, 5]),
        ];
        // The cut served 1 and 2 (the same depth) and the root; 3 and 4 were cut away.
        let served = [(0, 10), (1, 22), (2, 21)];
        assert_both_routes_agree(&sets, 10, &lineage, &served);
        let (_, row_major) = both_routes(&sets, 10, &lineage, &served);
        let rows: Vec<u32> = (0..10).collect();
        let ids = Resolved::new(rows.clone(), &[row_major]).columns_for(&rows)[0]
            .ids
            .clone();
        assert_eq!(
            ids[0..3],
            [Some(21), Some(21), Some(21)],
            "a member of 3 is held by 1 and 2 at one depth, and 2 has the lower identifier"
        );
        assert_eq!(ids[3], Some(21), "a member of 2 alone");
        assert_eq!(ids[4..6], [Some(22), Some(22)], "a member of 4 climbs to 1");
        assert_eq!(ids[9], Some(10), "a member of the root alone");
        assert_eq!(ids[6..9], [None, None, None], "held by nothing served");

        // Serving 3 as well: it is deeper than both, so it wins where it holds the point.
        let served = [(0, 10), (1, 22), (2, 21), (3, 99)];
        assert_both_routes_agree(&sets, 10, &lineage, &served);
    }

    /// Random DAGs with memberships closed upward, random served subsets, both routes against the
    /// rule — the agreement swept rather than argued.
    #[test]
    fn the_two_routes_agree_over_random_closed_dags() {
        let mut state = 0x5EEDu64;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..100 {
            let n = 2 + (next() % 12) as u32;
            let parents: Vec<Vec<u32>> = (0..n)
                .map(|o| {
                    let mut ups = Vec::new();
                    if o > 0 && !next().is_multiple_of(4) {
                        ups.push((next() % o as u64) as u32);
                        if next().is_multiple_of(2) {
                            ups.push((next() % o as u64) as u32);
                        }
                    }
                    ups.sort_unstable();
                    ups.dedup();
                    ups
                })
                .collect();
            let lineage = Arc::new(Lineage::dag(
                parents
                    .iter()
                    .enumerate()
                    .map(|(o, ups)| (o as u32, ups.clone())),
            ));
            // Each node's own rows, then the closure: a node holds its rows and its descendants'.
            let own: Vec<Vec<u32>> = (0..n).map(|o| vec![o * 2, o * 2 + 1]).collect();
            let mut closed: Vec<Vec<u32>> = own.clone();
            for o in (0..n as usize).rev() {
                let mine = closed[o].clone();
                let mut stack = parents[o].clone();
                let mut seen = Vec::new();
                while let Some(up) = stack.pop() {
                    if seen.contains(&up) {
                        continue;
                    }
                    seen.push(up);
                    closed[up as usize].extend_from_slice(&mine);
                    stack.extend_from_slice(&parents[up as usize]);
                }
            }
            for members in &mut closed {
                members.sort_unstable();
                members.dedup();
            }
            let sets: Vec<Option<&[u32]>> = closed.iter().map(|m| Some(m.as_slice())).collect();
            let served: Vec<(u32, u64)> = (0..n)
                .filter(|_| next().is_multiple_of(2))
                .map(|o| (o, 1000 - o as u64))
                .collect();
            if served.is_empty() {
                continue;
            }
            assert_both_routes_agree(&sets, n * 2, &lineage, &served);
        }
    }
}
