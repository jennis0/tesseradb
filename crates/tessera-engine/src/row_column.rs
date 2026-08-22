//! The row-major columns: **a level's membership addressed by row instead of by artifact.**
//!
//! An artifact-major level answers a viewport by walking the tile index and probing each candidate's
//! bitmap. A row-major level answers it by scanning `viewport ∩ M_auth` once and reading off which
//! artifact each visible row belongs to — so candidacy costs **points rather than artifacts**, and
//! nothing in the request is a function of how many artifacts the layer holds
//! (`design/artifact-serving-at-scale.md` §5.1).
//!
//! Two forms, and which one applies is not a choice:
//!
//! - **[`ServingLayout::RowMajorLabel`]** — one label per row, for a level whose memberships
//!   partition the corpus. A single-valued attribute predicate is the motivating case: every point
//!   carries exactly one value, so the memberships are disjoint.
//! - **[`ServingLayout::RowMajorList`]** — a list per row, the same inversion at a larger constant,
//!   for a level whose memberships overlap.
//!
//! **A level that claims to partition and does not is composed artifact-major**, loudly
//! ([`RowColumn::compose`] and [`RowColumn::project`] both return `None` on a double claim). Keeping
//! the last writer would give each contested row to whichever artifact happened to be walked last,
//! which is a masked count short for one artifact and long for another with nothing reporting it.
//!
//! # What this replaces, and what it does not
//!
//! It replaces the **candidacy walk** and the **counting route** for the levels it covers, and the
//! per-artifact declared size the proportional criterion divides by. It does **not** replace the
//! generating sets containment is tested against, or the visible-row set derived content is computed
//! from: both are per-artifact row-space questions with no row-addressed form, and both go on being
//! answered from [`crate::artifacts::MembershipRows`] exactly as they were.
//!
//! ⊘ **So the residency half of §5.1 is not taken here.** The artifact-major row form is still built
//! for a row-major level, which is what the layout exists to avoid at 10⁹ rows (4 GB against 78.5).
//! Not building it needs containment's projection-loss test and the computed properties to reach the
//! membership another way, and neither is designed; what this stage delivers is the mechanism, the
//! record, the files and the route — with every answer asserted identical to the artifact-major
//! one's, which is what a later change removing the row form would be checked against.
//!
//! # The declared sizes are derived, never stored
//!
//! §10's answer to the proportional criterion's denominator on a row-major level is the per-artifact
//! **unmasked** membership size, which is mask-independent and corpus-wide. It is folded up in the
//! same pass that validates the column — an artifact's size is how many rows carry its label — so it
//! is a function of the bytes beside it rather than a second thing that could disagree with them.
//! That is [`crate::tile_index`]'s rule for the node hierarchy, one structure along.

use croaring::Bitmap;

use tessera_lifecycle::membership::ArtifactRecord;
use tessera_store::membership::{
    pack_label_column, pack_list_column, LabelColumnPack, ListColumnPack, ROW_COLUMN_HOLE,
};
use tessera_store::permutation::RowSpace;
use tessera_types::layer::ServingLayout;

use crate::artifacts::MembershipRows;

/// One walk of a level's live artifacts, handing each ordinal its **projected** rows.
///
/// **A callback rather than an iterator**, because the caller has to be able to run it more than
/// once: a list column is an offset table sized by one pass and filled by a second, and the fold's
/// walk holds one membership at a time rather than the level's. An iterator would have to be
/// re-created, which is what this type is.
type LevelWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, &Bitmap));
use crate::compose::WholeMask;

/// One `(view, layer, level)`'s row-addressed membership — mapped where a fold wrote it, a buffer
/// where a publication built it.
pub struct RowColumn {
    pack: Pack,
    /// Per ordinal, how many **rows** carry this artifact's label — the unmasked membership size in
    /// this view's row space. Derived at open; see the module doc.
    declared: Vec<u32>,
}

enum Pack {
    Label(LabelColumnPack),
    List(ListColumnPack),
}

impl std::fmt::Debug for RowColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowColumn")
            .field("layout", &self.layout())
            .field("ordinals", &self.len())
            .field("rows", &self.row_count())
            .finish()
    }
}

impl RowColumn {
    /// Compose one from a level's resident row form — what a publication or a growth builds, before
    /// any fold has consolidated it.
    ///
    /// `None` where `layout` is [`ServingLayout::RowMajorLabel`] and the memberships do not
    /// partition: the caller composes the level artifact-major instead and says so.
    ///
    /// **Framed and read back through the same checks a mapped file takes**, for the reason
    /// [`crate::containment::ContainmentPartition`] gives: the two routes are one reader, so a
    /// framing rule can never hold for a file and not for the form a publication built.
    pub fn compose(
        membership: &MembershipRows,
        row_count: u32,
        layout: ServingLayout,
    ) -> Option<Self> {
        let ordinals = membership.len() as u32;
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for ordinal in 0..ordinals {
                if let Some(rows) = membership.get(ordinal) {
                    visit(ordinal, rows);
                }
            }
        };
        Self::assemble(ordinals, row_count, layout, &each)
    }

    /// The same column, projected straight from a level's records without building the row form
    /// first — what the fold writes.
    ///
    /// **Equal to [`Self::compose`] over the row form of the same level**, by construction rather
    /// than by an argument: both read `RowSpace::project_base`'s output, which is precisely what
    /// [`MembershipRows`] stores. What differs is what is held while it runs — one membership at a
    /// time rather than the whole level's, which is the same asymmetry
    /// [`crate::tile_index::TileIndex::project`] takes and for the same reason.
    ///
    /// **`level` is called more than once**, and it has to be: a list column is an offset table and
    /// a value array, and sizing the first needs a pass the second then fills. A label column takes
    /// one pass and is handed the same closure.
    pub fn project<'a, I>(
        ordinals: u32,
        space: &RowSpace,
        layout: ServingLayout,
        level: impl Fn() -> I,
    ) -> Option<Self>
    where
        I: Iterator<Item = (u32, &'a ArtifactRecord)>,
    {
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for (ordinal, record) in level() {
                visit(ordinal, &space.project_base(&record.members));
            }
        };
        Self::assemble(ordinals, space.base_rows(), layout, &each)
    }

    /// Open a fold-written column, mapped in place, and check it is the form the manifest claims.
    ///
    /// **The tag is checked against the file, not trusted over it** (selection memo §5). Each format
    /// carries its own magic, so a manifest that names a list where a label column sits refuses at
    /// the first bytes rather than reading an offset table as labels — and the caller's answer to a
    /// refusal is to recompose the level, which is what every request did before this structure
    /// existed.
    pub fn open(path: &std::path::Path, expected: ServingLayout) -> tessera_store::Result<Self> {
        let pack = match expected {
            ServingLayout::RowMajorLabel => Pack::Label(LabelColumnPack::open(path)?),
            ServingLayout::RowMajorList => Pack::List(ListColumnPack::open(path)?),
            ServingLayout::ArtifactMajor | ServingLayout::SpatialRanges => {
                return Err(tessera_store::StoreError::MalformedBundle {
                    detail: format!(
                        "row-major column {}: the manifest tags it {}, which has no column — the \
                         entry names a file no writer produces",
                        path.display(),
                        expected.pin_word()
                    ),
                })
            }
        };
        Ok(Self::over(pack))
    }

    /// Which form this is.
    pub fn layout(&self) -> ServingLayout {
        match self.pack {
            Pack::Label(_) => ServingLayout::RowMajorLabel,
            Pack::List(_) => ServingLayout::RowMajorList,
        }
    }

    /// How many ordinals this column covers, holes included.
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// The row space this column was addressed in.
    pub fn row_count(&self) -> u32 {
        match &self.pack {
            Pack::Label(pack) => pack.rows(),
            Pack::List(pack) => pack.rows(),
        }
    }

    /// The artifact's **unmasked** membership size in this view's row space — the proportional
    /// criterion's denominator on a row-major level.
    ///
    /// Zero for a hole and for a live artifact whose membership projects to nothing, exactly as the
    /// row form's cardinality is: the two are told apart by the records, never here.
    pub fn declared_size(&self, ordinal: u32) -> u64 {
        self.declared
            .get(ordinal as usize)
            .copied()
            .map(u64::from)
            .unwrap_or(0)
    }

    /// **Candidacy: one scan of `viewport ∩ M_auth`, marking labels.**
    ///
    /// Every ordinal returned has a member the viewer can see inside the viewport — which is
    /// *exactly* the question the artifact-major route reaches through the tile index's walk and a
    /// masked probe per candidate. There is no separate probe here because the scan already asked
    /// it: a row in `here` is visible by construction, so the artifact it labels has a visible
    /// member in view.
    ///
    /// `here` must come from the composed mask and from nothing else — see
    /// [`MaskedSet::visible_rows`], which is the only way to obtain one.
    ///
    /// **Ascending**, which is what the cut downstream is entitled to.
    ///
    /// A `Vec<bool>` over the level's ordinals rather than adding into the bitmap as the scan goes:
    /// a scattered layer's rows hit the same handful of ordinals over and over, and a set insert per
    /// row is the cost the marking array exists to remove. It is one byte per artifact for the
    /// length of the call.
    pub fn candidates(&self, here: &Bitmap) -> Bitmap {
        let mut seen = vec![false; self.len()];
        match &self.pack {
            Pack::Label(pack) => {
                for row in here.iter() {
                    let label = pack.label(row as usize);
                    if label != ROW_COLUMN_HOLE {
                        seen[label as usize] = true;
                    }
                }
            }
            Pack::List(pack) => {
                for row in here.iter() {
                    for ordinal in pack.list(row as usize) {
                        seen[ordinal as usize] = true;
                    }
                }
            }
        }
        let mut out = Bitmap::new();
        for (ordinal, hit) in seen.iter().enumerate() {
            if *hit {
                out.add(ordinal as u32);
            }
        }
        out.run_optimize();
        out
    }

    /// **The masked count for every artifact of this level, in one walk of the mask** — decision
    /// 0093's one named exception, and the only route a row-major level has to the quantity the
    /// disclosure rule requires.
    ///
    /// **Over the whole mask, not over the viewport.** A viewer is told how many of an artifact's
    /// documents they can see, which does not change as they pan; a per-viewport count would move
    /// with the box and let a viewer difference two boxes for the members in between.
    ///
    /// **Filter-blind**, exactly as [`MaskedSet::count_intersection`] is: a filtered count here
    /// would make an artifact's existence criterion a function of the filter, so an artifact would
    /// appear and disappear as a viewer typed — a filter moving the frontier down, which **I12**
    /// forbids. [`MaskedSet::visible_all`] is the composed mask and carries no filter, which is what
    /// makes that structural rather than remembered.
    ///
    /// `u32` per artifact, which is ~4 B each — 4 MB at 10⁶ artifacts and 40 MB at 10⁷ — and is why
    /// the cache holding these is byte-budgeted (`crate::histogram`). A count cannot exceed the row
    /// space, which is `u32`-addressed.
    pub fn histogram(&self, mask: &impl WholeMask) -> Vec<u32> {
        let mut counts = vec![0u32; self.len()];
        let visible = mask.visible_all();
        match &self.pack {
            Pack::Label(pack) => {
                for row in visible.iter() {
                    let label = pack.label(row as usize);
                    if label != ROW_COLUMN_HOLE {
                        counts[label as usize] += 1;
                    }
                }
            }
            Pack::List(pack) => {
                for row in visible.iter() {
                    for ordinal in pack.list(row as usize) {
                        counts[ordinal as usize] += 1;
                    }
                }
            }
        }
        counts
    }

    /// The durable bytes — what the fold writes into the prefix.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.pack {
            Pack::Label(pack) => pack.as_bytes(),
            Pack::List(pack) => pack.as_bytes(),
        }
    }

    /// One pass over the packed column, folding up the per-artifact declared sizes.
    fn over(pack: Pack) -> Self {
        let ordinals = match &pack {
            Pack::Label(pack) => pack.ordinals(),
            Pack::List(pack) => pack.ordinals(),
        };
        let mut declared = vec![0u32; ordinals as usize];
        match &pack {
            Pack::Label(pack) => {
                for row in 0..pack.rows() as usize {
                    let label = pack.label(row);
                    if label != ROW_COLUMN_HOLE {
                        declared[label as usize] += 1;
                    }
                }
            }
            Pack::List(pack) => {
                for row in 0..pack.rows() as usize {
                    for ordinal in pack.list(row) {
                        declared[ordinal as usize] += 1;
                    }
                }
            }
        }
        RowColumn { pack, declared }
    }

    /// The two builders, sharing one walk protocol: `each` calls `visit` once per live artifact with
    /// its projected rows, and may be called more than once.
    fn assemble(
        ordinals: u32,
        row_count: u32,
        layout: ServingLayout,
        each: LevelWalk<'_>,
    ) -> Option<Self> {
        let bytes = match layout {
            ServingLayout::ArtifactMajor | ServingLayout::SpatialRanges => return None,
            ServingLayout::RowMajorLabel => {
                let mut labels = vec![ROW_COLUMN_HOLE; row_count as usize];
                let mut overlapped = false;
                each(&mut |ordinal, rows| {
                    for row in rows.iter() {
                        let at = row as usize;
                        // A row past the column is a member the projection placed above this view's
                        // base row space, which `project_base` does not produce. Guarded rather than
                        // trusted: the alternative is a panic on a shape nothing here controls.
                        if at >= labels.len() {
                            continue;
                        }
                        // **A double claim is not a partition**, and this is where the pin's second
                        // refusal fires — the one that could not be checked at parse.
                        if labels[at] != ROW_COLUMN_HOLE {
                            overlapped = true;
                            return;
                        }
                        labels[at] = ordinal;
                    }
                });
                if overlapped {
                    return None;
                }
                pack_label_column(ordinals, &labels)
            }
            ServingLayout::RowMajorList => {
                // Pass one sizes each row's list; pass two fills it. Two passes rather than a
                // vector per row, which at 10⁹ rows is the allocator's whole address space in
                // headers alone.
                let mut at = vec![0u32; row_count as usize + 1];
                each(&mut |_, rows| {
                    for row in rows.iter() {
                        if (row as usize) < row_count as usize {
                            at[row as usize + 1] += 1;
                        }
                    }
                });
                for i in 1..at.len() {
                    at[i] += at[i - 1];
                }
                let mut values = vec![0u32; *at.last().unwrap_or(&0) as usize];
                let mut cursor = at.clone();
                each(&mut |ordinal, rows| {
                    for row in rows.iter() {
                        let at = row as usize;
                        if at >= row_count as usize {
                            continue;
                        }
                        values[cursor[at] as usize] = ordinal;
                        cursor[at] += 1;
                    }
                });
                pack_list_column(ordinals, &at, &values)
            }
        };
        let pack = match layout {
            ServingLayout::RowMajorLabel => Pack::Label(
                LabelColumnPack::from_bytes(bytes)
                    .expect("a column this crate just packed frames by construction"),
            ),
            _ => Pack::List(
                ListColumnPack::from_bytes(bytes)
                    .expect("a column this crate just packed frames by construction"),
            ),
        };
        Some(Self::over(pack))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::MaskedSet;

    fn rows_of(sets: &[Option<&[u32]>]) -> MembershipRows {
        MembershipRows::of_rows(
            sets.iter()
                .map(|set| set.map(|s| s.iter().copied().collect::<Bitmap>()))
                .collect(),
        )
    }

    fn bitmap(values: &[u32]) -> Bitmap {
        values.iter().copied().collect()
    }

    /// **The label form answers the three questions the route asks of it**, and the hole is a real
    /// state: a row no artifact claims contributes to nobody's count.
    #[test]
    fn a_label_column_answers_candidacy_counts_and_sizes() {
        // Ordinal 0 holds rows 0..3, ordinal 1 holds 5 and 6, ordinal 2 is a hole, ordinal 3 is
        // live with an empty projection. Rows 4, 7, 8, 9 belong to nobody.
        let membership = rows_of(&[Some(&[0, 1, 2]), Some(&[5, 6]), None, Some(&[])]);
        let column =
            RowColumn::compose(&membership, 10, ServingLayout::RowMajorLabel).expect("partitions");

        assert_eq!(column.layout(), ServingLayout::RowMajorLabel);
        assert_eq!(column.len(), 4);
        assert_eq!(column.row_count(), 10);
        assert_eq!(column.declared_size(0), 3);
        assert_eq!(column.declared_size(1), 2);
        assert_eq!(column.declared_size(2), 0, "a hole has no rows");
        assert_eq!(column.declared_size(3), 0, "and neither has an empty one");
        assert_eq!(column.declared_size(99), 0, "past the level is not a panic");

        // Candidacy over a viewport∩mask holding row 1 and row 6.
        assert_eq!(
            column
                .candidates(&bitmap(&[1, 6]))
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        // A viewport holding only unclaimed rows returns nothing.
        assert!(column.candidates(&bitmap(&[4, 8])).is_empty());

        // The histogram is over the whole mask, not the viewport.
        let mask = bitmap(&[0, 2, 5, 9]);
        assert_eq!(column.histogram(&mask), vec![2, 1, 0, 0]);
    }

    /// The list form is the same three answers where a row belongs to several artifacts — which is
    /// exactly the state the label form refuses to represent.
    #[test]
    fn a_list_column_carries_a_row_that_several_artifacts_claim() {
        let membership = rows_of(&[Some(&[0, 1]), Some(&[1, 2]), Some(&[])]);
        let column =
            RowColumn::compose(&membership, 4, ServingLayout::RowMajorList).expect("always builds");
        assert_eq!(column.layout(), ServingLayout::RowMajorList);
        assert_eq!(column.declared_size(0), 2);
        assert_eq!(column.declared_size(1), 2);
        assert_eq!(column.declared_size(2), 0);

        // Row 1 is claimed by both, so a viewport holding only it makes both candidates.
        assert_eq!(
            column.candidates(&bitmap(&[1])).iter().collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(column.histogram(&bitmap(&[0, 1, 2, 3])), vec![2, 2, 0]);
    }

    /// **A level that does not partition is refused a label column**, rather than served one whose
    /// contested rows went to whichever artifact was walked last. The list form takes the same
    /// memberships.
    #[test]
    fn a_double_claim_declines_the_label_form_and_not_the_list_form() {
        let overlapping = rows_of(&[Some(&[0, 1]), Some(&[1, 2])]);
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::RowMajorLabel).is_none());
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::RowMajorList).is_some());
        // And artifact-major has no column at all, in either builder.
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::ArtifactMajor).is_none());
    }

    /// **A composed column and a mapped one are the same structure**, so the fold's consolidation is
    /// a change of backing rather than a second encoder — and a file offered under the wrong tag is
    /// a refusal rather than a misread.
    #[test]
    fn a_column_answers_the_same_mapped_as_composed() {
        let tmp = tempfile::tempdir().unwrap();
        for (layout, name) in [
            (ServingLayout::RowMajorLabel, "c.tslb"),
            (ServingLayout::RowMajorList, "c.tsll"),
        ] {
            let membership = rows_of(&[Some(&[0, 1, 2]), None, Some(&[5, 6]), Some(&[])]);
            let composed = RowColumn::compose(&membership, 8, layout).expect("builds");
            let path = tmp.path().join(name);
            std::fs::write(&path, composed.as_bytes()).unwrap();
            let mapped = RowColumn::open(&path, layout).unwrap();

            assert_eq!(mapped.len(), composed.len());
            assert_eq!(mapped.row_count(), composed.row_count());
            for ordinal in 0..4u32 {
                assert_eq!(
                    mapped.declared_size(ordinal),
                    composed.declared_size(ordinal)
                );
            }
            let here = bitmap(&[1, 6]);
            assert_eq!(
                mapped.candidates(&here).iter().collect::<Vec<_>>(),
                composed.candidates(&here).iter().collect::<Vec<_>>()
            );
            let mask = bitmap(&[0, 1, 5]);
            assert_eq!(mapped.histogram(&mask), composed.histogram(&mask));

            // The other tag over the same bytes: the distinct magics are what make this a refusal.
            let other = match layout {
                ServingLayout::RowMajorLabel => ServingLayout::RowMajorList,
                _ => ServingLayout::RowMajorLabel,
            };
            assert!(RowColumn::open(&path, other).is_err());
            assert!(RowColumn::open(&path, ServingLayout::ArtifactMajor).is_err());
        }
    }

    /// The column and the row form agree about every artifact's size and every artifact's masked
    /// count — which is the property the whole route rests on, asserted here at the structure and
    /// again end to end in `tests/artifact_row_major.rs`.
    #[test]
    fn the_column_and_the_row_form_agree_artifact_for_artifact() {
        let sets: Vec<Vec<u32>> = (0..64u32)
            .map(|i| ((i * 7)..(i * 7 + 7)).collect())
            .collect();
        let refs: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let membership = rows_of(&refs);
        let row_count = 64 * 7;
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let column = RowColumn::compose(&membership, row_count, layout).expect("partitions");
            let mask: Bitmap = (0..row_count).filter(|r| r % 3 == 0).collect();
            let histogram = column.histogram(&mask);
            for ordinal in 0..64u32 {
                let rows = membership.get(ordinal).unwrap();
                assert_eq!(
                    u64::from(histogram[ordinal as usize]),
                    mask.count_intersection(rows),
                    "masked count disagreed at ordinal {ordinal} under {layout:?}"
                );
                assert_eq!(
                    column.declared_size(ordinal),
                    rows.cardinality(),
                    "declared size disagreed at ordinal {ordinal} under {layout:?}"
                );
            }
        }
    }
}
