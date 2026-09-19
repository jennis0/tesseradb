//! **The walk serves what the sweep served, or the geometry is answering.**
//!
//! `crate::tile_index` replaces the serving path's loop over every ordinal of a level with a
//! top-down walk of a row-range hierarchy, and the whole licence for that is `design/artifact-serving-at-scale.md`
//! §4.1: an artifact in no node the viewport touches has no member there, so skipping it withholds
//! nothing — and an artifact inside a node the viewport covers entirely has
//! `membership ⊆ viewport`, so one probe against `viewport ∩ M_auth` answers both the request's
//! question and the layer-wide one. Neither is a verdict. If either is wrong the failure is silent
//! in both directions: an omitted artifact is absent, which a viewer cannot tell from one that
//! failed its criterion, and a wrongly settled one is answered on a claim that is no longer true.
//!
//! So the sweep this replaced is kept here as the oracle — **not** a reimplementation of it: it is
//! the same `ArtifactRows::intersects` over `0..len` that `serve_artifacts` ran, against the same
//! `ArtifactRows::candidate_in` that `serve_artifacts` now calls, so the classification under test
//! is the shipped one and not a transcription of it. The two must agree ordinal for ordinal, count
//! for count and rank for rank, over masks × viewports × membership shapes.
//!
//! This is the engine-side twin of the probe's `assert_same_answer`, and like
//! `artifact_containment.rs` it is built to fail where the probe's fixture could not: the row order
//! is a shuffle, memberships are defined in **row** space so the three shapes the cost model names
//! are real, the overlay is live, a growth lands mid-sequence, and holes and empty projections sit
//! among the artifacts rather than at the end.

use std::sync::Arc;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::artifacts::{
    ArtifactProjections, ArtifactRows, ArtifactVerdict, ArtifactView, MembershipRows,
};
use tessera_engine::compose::{compose, EffectiveMask};
use tessera_engine::projection::RowProjection;
use tessera_engine::denied_rows_of;
use tessera_engine::row_column::RowColumn;
use tessera_engine::tile_index::{Extent, TileIndex, Viewport};
use tessera_lifecycle::membership::{ArtifactRecord, ArtifactStore, Attachment, ContentSet};
use tessera_lifecycle::wal::ParentRef;
use tessera_lifecycle::{ChangeOp, IngestBuffer, Overlay};
use tessera_store::write::write_permutation;
use tessera_store::{Permutation, RowSpace};
use tessera_types::layer::{
    ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ServingLayout,
};
use tessera_types::{EntityId, TermId};

/// Rows in the view. Large enough that the hierarchy has four levels — a corpus that builds one
/// level would settle everything at the root and prove nothing about the descent.
const UNIVERSE: u32 = 300_000;
const TERMS: u32 = 32;
const LAYER: &str = "clusters/a";
const SMALL_TERM_THRESHOLD: u32 = 24;

/// Entities the postings name and the permutation does not cover — **members awaiting a fold**. An
/// artifact all of whose members are these projects to nothing, which is the `Empty` extent.
const UNROWED: u32 = UNIVERSE + 1;
const LABELLED: u32 = UNIVERSE + 64;

/// The three row-space localities `design/artifact-serving-at-scale.md` §5 names, which is the axis
/// that decides what the index can do — not the membership *source*, which does not line up with
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Clustered: a contiguous row range per artifact, jittered so many of them straddle a node
    /// boundary and are promoted a level. The extent beside the tree is what rescues those.
    Runs,
    /// A real hierarchy: membership built **from** the tree, the root owning the row space and each
    /// node splitting its range among its children. A parent is in view whenever any child is, so
    /// the coarse artifacts are settled by nothing narrow.
    Nested,
    /// Scattered: members everywhere, so every artifact is too wide for any node and lands in
    /// `everywhere` — the set that makes such a layer cost the population at every zoom.
    Scattered,
    /// [`Shape::Runs`] with the jitter taken out, so the memberships **partition** the row space:
    /// the one shape a label column composes over, and therefore the only way the label form's
    /// half of a row-major differential gets exercised at all.
    Partitioned,
}

struct Fixture {
    _temp: TempDir,
    postings: PostingsReader,
    row_space: RowSpace,
    /// `order[row] = entity`, kept so a membership can be declared in **row** space and converted,
    /// which is what makes the three shapes above real rather than nominal.
    order: Vec<EntityId>,
    store: ArtifactStore,
    /// Ordinals the level holds no record at — a fold executed a deletion and the slot is open.
    holes: Vec<u32>,
    /// The ordinal whose whole membership is unrowed: a live artifact with an empty projection.
    unprojectable: u32,
    ordinals: u32,
}

fn build_fixture(shape: Shape) -> Fixture {
    let temp = TempDir::new().unwrap();
    let mut rng = StdRng::seed_from_u64(0x_71_1e_1d_ec);

    // Signatures drawn independently of everything else, as `artifact_containment.rs` argues.
    let terms_of: Vec<Vec<u32>> = (0..LABELLED)
        .map(|_| {
            let mut terms: Vec<u32> = (0..rng.gen_range(1..=2))
                .map(|_| rng.gen_range(0..TERMS))
                .collect();
            terms.sort_unstable();
            terms.dedup();
            terms
        })
        .collect();
    let mut per_term: Vec<Vec<u32>> = vec![Vec::new(); TERMS as usize];
    for (entity, terms) in terms_of.iter().enumerate() {
        for term in terms {
            per_term[*term as usize].push(entity as u32);
        }
    }
    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &per_term, SMALL_TERM_THRESHOLD).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    // **A shuffle, not the identity**: row order and entity order agreeing would let a bug that
    // conflated the two pass every case here — and would make a row-local membership accidentally
    // entity-local as well.
    let mut order: Vec<EntityId> = (0..UNIVERSE as u64).map(EntityId::new).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.gen_range(0..=i));
    }
    let perm_path = temp.path().join("permutation.bin");
    write_permutation(&perm_path, &order, UNIVERSE as u64).unwrap();
    let row_space = RowSpace::new(Arc::new(Permutation::load(&perm_path).unwrap()), UNIVERSE);

    let spans = spans_for(shape, &mut rng);
    let ordinals = spans.len() as u32;
    // Holes among the artifacts rather than at the end, where a length check would find them.
    let holes = vec![5u32, 17, ordinals / 2];
    let unprojectable = 9u32;

    let mut store = ArtifactStore::new();
    for (ordinal, (rows, parent)) in spans.iter().enumerate() {
        let ordinal = ordinal as u32;
        if holes.contains(&ordinal) {
            continue;
        }
        let members: Bitmap = if ordinal == unprojectable {
            // Every member awaiting a fold: the artifact is live and its projection is empty.
            (0..4).map(|i| UNROWED + i).collect()
        } else {
            rows.iter()
                .map(|row| order[*row as usize].raw() as u32)
                .collect()
        };
        // Ranked contents, so a rank-for-rank comparison means something: rank 0 is the widest, so
        // a viewer usually falls through to a narrower one.
        let contents: Vec<ContentSet> = (0..rng.gen_range(1..=3u32))
            .map(|rank| {
                let width = if rank == 0 { 6 } else { 2 };
                let set: Bitmap = (0..width).map(|_| rng.gen_range(0..UNIVERSE)).collect();
                ContentSet {
                    values: Some(vec!["text".to_string()]),
                    digest: tessera_lifecycle::membership::content_digest(&["text".to_string()]),
                    cardinality: set.cardinality(),
                    generated_from: set,
                }
            })
            .collect();
        store.put(
            LAYER,
            0,
            ordinal,
            ArtifactRecord {
                entity: EntityId::new(u64::from(u32::MAX - ordinal)),
                key: None,
                view: None,
                members: members.into(),
                contents,
                attached_to: None,
                parents: parent.iter().copied().collect(),
            },
            None,
        );
    }

    Fixture {
        _temp: temp,
        postings,
        row_space,
        order,
        store,
        holes,
        unprojectable,
        ordinals,
    }
}

/// One membership per ordinal, in **row** space, with the parent edge where the shape has one.
fn spans_for(shape: Shape, rng: &mut StdRng) -> Vec<(Vec<u32>, Option<ParentRef>)> {
    match shape {
        Shape::Runs => (0..600u32)
            .map(|i| {
                let span = UNIVERSE / 600;
                // Jitter, so that many artifacts straddle a 1 024-row node boundary and are
                // promoted — the case the extent test exists for.
                let lo = i * span + rng.gen_range(0..span);
                let hi = (lo + rng.gen_range(1..span * 3)).min(UNIVERSE - 1);
                ((lo..=hi).collect(), None)
            })
            .collect(),
        Shape::Nested => {
            // A balanced three-way tree over the row space: the root owns everything, each node
            // splits its range among its children. Five levels is 121 nodes.
            let mut out: Vec<(Vec<u32>, Option<ParentRef>)> = Vec::new();
            let mut frontier: Vec<(u32, u32, Option<u32>)> = vec![(0, UNIVERSE - 1, None)];
            for _ in 0..5 {
                let mut next = Vec::new();
                for (lo, hi, parent) in frontier {
                    let ordinal = out.len() as u32;
                    out.push((
                        (lo..=hi).collect(),
                        parent.map(|ordinal| ParentRef { level: 0, ordinal }),
                    ));
                    let width = (hi - lo + 1) / 3;
                    if width > 0 {
                        for child in 0..3u32 {
                            let clo = lo + child * width;
                            let chi = if child == 2 { hi } else { clo + width - 1 };
                            next.push((clo, chi, Some(ordinal)));
                        }
                    }
                }
                frontier = next;
            }
            out
        }
        Shape::Scattered => (0..200u32)
            .map(|_| {
                let mut rows: Vec<u32> = (0..40).map(|_| rng.gen_range(0..UNIVERSE)).collect();
                rows.sort_unstable();
                rows.dedup();
                (rows, None)
            })
            .collect(),
        // Contiguous and disjoint, tiling the row space: no row is claimed twice, which is what a
        // label column requires.
        Shape::Partitioned => (0..600u32)
            .map(|i| {
                let span = UNIVERSE / 600;
                let lo = i * span;
                let hi = if i == 599 {
                    UNIVERSE - 1
                } else {
                    lo + span - 1
                };
                ((lo..=hi).collect(), None)
            })
            .collect(),
    }
}

impl Fixture {
    fn rows(&self) -> ArtifactRows {
        ArtifactRows::build(self.store.level(LAYER, 0), &self.row_space)
    }

    fn membership(&self) -> MembershipRows {
        MembershipRows::build(self.store.level(LAYER, 0), &self.row_space)
    }

    fn entity_of(&self, ordinal: u32) -> Option<EntityId> {
        self.store
            .get(LAYER, 0, ordinal)
            .map(|record| record.entity)
    }

    /// The composed mask, the principal's term set and this view's deny mask — the three things a
    /// request holds, derived exactly as `compose` and the deny lane derive them.
    fn mask(
        &self,
        granted: &[u32],
        overlay: &Overlay,
    ) -> (EffectiveMask, FxHashSet<TermId>, Bitmap) {
        let satisfied: FxHashSet<TermId> = granted.iter().map(|t| TermId::new(*t)).collect();
        let mut sorted: Vec<TermId> = satisfied.iter().copied().collect();
        sorted.sort_unstable_by_key(|t| t.raw());
        let cache = FragmentCache::new(&self._temp.path().join("frag"), [1u8; 32], [2u8; 32]);
        let fragment = cache
            .get_or_build(&sorted, [3u8; 32], 0, &self.postings, &[], UNIVERSE as u64)
            .unwrap();
        let base = Arc::new(RowProjection::walk(&fragment, &self.row_space));
        let buffer = IngestBuffer::new();
        let denied = denied_rows_of(overlay, &self.row_space);
        let mask = compose(&satisfied, overlay, &buffer, base, &self.row_space, &denied);
        (mask, satisfied, denied)
    }
}

fn declaration() -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: LAYER.into(),
        title: None,
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration::default(),
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

fn always_served(_attachment: &Attachment) -> bool {
    true
}

/// Six principals of very different breadths, including one holding nothing and one holding
/// everything.
fn principals() -> Vec<Vec<u32>> {
    vec![
        (0..TERMS).collect(),
        (0..TERMS).filter(|t| t % 2 == 0).collect(),
        (0..TERMS).filter(|t| t % 5 == 0).collect(),
        vec![3, 11],
        vec![7],
        Vec::new(),
    ]
}

/// Viewports as row-space sets, including two that are **several runs** — which is what a real tile
/// set is, and what a walk that assumed one contiguous range would get wrong.
fn viewports() -> Vec<(&'static str, Vec<std::ops::RangeInclusive<u32>>)> {
    vec![
        ("whole map", vec![0..=UNIVERSE - 1]),
        ("half", vec![0..=UNIVERSE / 2]),
        ("aligned block", vec![4_096..=8_191]),
        ("unaligned window", vec![1_000..=1_100]),
        ("one row", vec![33_333..=33_333]),
        ("two panes", vec![2_000..=2_400, 40_000..=41_000]),
        (
            "a comb",
            vec![0..=99, 10_000..=10_099, 20_000..=20_099, 50_000..=50_099],
        ),
    ]
}

fn rows_of(ranges: &[std::ops::RangeInclusive<u32>]) -> Bitmap {
    let mut out = Bitmap::new();
    for range in ranges {
        out.add_range(*range.start()..=*range.end());
    }
    out
}

/// What one request serves for one layer at one level: the ordinal, the masked count beside it and
/// the rank it was served at. **All three**, because a walk that returned the right *set* on the
/// wrong route would still be answering a different question.
type Served = Vec<(u32, u64, Option<u32>)>;

/// **The oracle: the loop `serve_artifacts` ran before the index existed.** Every ordinal of the
/// level, `ArtifactRows::intersects` for candidacy, then the one predicate.
fn sweep(
    fx: &Fixture,
    rows: &ArtifactRows,
    view: &ArtifactView<'_, EffectiveMask>,
    tiles: &Bitmap,
    mask: &EffectiveMask,
) -> Served {
    let mut out = Served::new();
    for ordinal in 0..rows.len() as u32 {
        if !rows.intersects(ordinal, tiles, mask) {
            continue;
        }
        let Some(entity) = fx.entity_of(ordinal) else {
            continue;
        };
        if let ArtifactVerdict::Serve { masked_count, rank } = view.verdict(entity, ordinal, None) {
            out.push((ordinal, masked_count, rank));
        }
    }
    out
}

/// **The shipped route**: the walk, then `ArtifactRows::candidate_in` — the same call
/// `serve_artifacts` makes, so this drives the classification rather than restating it.
fn walk(
    fx: &Fixture,
    rows: &ArtifactRows,
    view: &ArtifactView<'_, EffectiveMask>,
    tiles: &Bitmap,
    mask: &EffectiveMask,
) -> (Served, u64) {
    let viewport = Viewport::compose(tiles, mask);
    // `ArtifactRows::candidacy`, which is what `serve_artifacts` calls — so this drives the route
    // choice as well as the classification. These fixtures are all artifact-major, so the walk is
    // the route taken and the node count is real.
    let candidates = rows.candidacy(&viewport, None);
    let tessera_engine::artifacts::Candidacy::Indexed(walked) = &candidates else {
        panic!("an artifact-major level is answered by the walk");
    };
    let nodes_visited = walked.nodes_visited();
    let mut out = Served::new();
    for ordinal in candidates.iter() {
        if !rows.candidate_in(ordinal, &candidates, &viewport, mask) {
            continue;
        }
        let Some(entity) = fx.entity_of(ordinal) else {
            continue;
        };
        if let ArtifactVerdict::Serve { masked_count, rank } = view.verdict(entity, ordinal, None) {
            out.push((ordinal, masked_count, rank));
        }
    }
    (out, nodes_visited)
}

/// The overlay in the three shapes that behave differently: a plain suppression, a deletion, and
/// `delete → suppress → unsuppress`, which must leave the entity deleted.
fn denied_overlay(fx: &Fixture) -> Overlay {
    let mut overlay = Overlay::new();
    for row in (0..UNIVERSE).filter(|r| r % 37 == 0) {
        overlay.apply(fx.order[row as usize], ChangeOp::Suppress);
    }
    for row in (0..UNIVERSE).filter(|r| r % 53 == 0) {
        overlay.apply(fx.order[row as usize], ChangeOp::Delete);
    }
    for row in (0..UNIVERSE).filter(|r| r % 101 == 0) {
        let entity = fx.order[row as usize];
        overlay.apply(entity, ChangeOp::Delete);
        overlay.apply(entity, ChangeOp::Suppress);
        overlay.apply(entity, ChangeOp::Unsuppress);
    }
    // And two artifacts' own entities, which the predicate's first conjunct takes out whatever the
    // geometry said.
    overlay.apply(EntityId::new(u64::from(u32::MAX - 2)), ChangeOp::Suppress);
    overlay.apply(EntityId::new(u64::from(u32::MAX - 3)), ChangeOp::Delete);
    overlay
}

/// **The stage's headline.** Over three membership shapes, six principals, two overlays and seven
/// viewports, the walk and the sweep serve the same ordinals with the same counts at the same
/// ranks.
#[test]
fn the_walk_serves_exactly_what_the_sweep_served() {
    for shape in [Shape::Runs, Shape::Nested, Shape::Scattered] {
        let fx = build_fixture(shape);
        let rows = fx.rows();
        assert_shapes_are_what_they_claim(shape, &rows);
        let served = differential(&fx, &rows);
        assert!(
            served > 0,
            "{shape:?} served nothing across the whole grid, so the agreement is trivial"
        );
    }
}

/// The whole grid for one fixture, returning how many artifacts were served across it.
fn differential(fx: &Fixture, rows: &ArtifactRows) -> usize {
    let declaration = declaration();
    let denied_overlay = denied_overlay(fx);
    let mut total = 0usize;
    for overlay in [&Overlay::new(), &denied_overlay] {
        for granted in principals() {
            let (mask, satisfied, denied) = fx.mask(&granted, overlay);
            let view = ArtifactView {
                declaration: &declaration,
                overlay,
                satisfied: &satisfied,
                layer_reachable: true,
                rows,
                dependency_served: &always_served,
                mask: &mask,
                denied: &denied,
                containment: None,
                counts: None,
            };
            for (name, ranges) in viewports() {
                let tiles = rows_of(&ranges);
                let expected = sweep(fx, rows, &view, &tiles, &mask);
                let (actual, _) = walk(fx, rows, &view, &tiles, &mask);
                assert_eq!(
                    actual, expected,
                    "the walk disagrees with the sweep at the {name} viewport for terms {granted:?}"
                );
                total += expected.len();
            }
        }
    }
    total
}

/// Each arm is the shape it claims to be. A `Runs` fixture where everything landed in `everywhere`,
/// or a `Scattered` one where nothing did, would run the grid above without exercising the route it
/// is named for.
fn assert_shapes_are_what_they_claim(shape: Shape, rows: &ArtifactRows) {
    let index = rows.index();
    let wide = index.everywhere();
    let live = (0..index.len() as u32)
        .filter(|o| matches!(index.extent(*o), Extent::Span { .. }))
        .count() as u64;
    match shape {
        Shape::Runs => assert!(
            wide * 20 < live,
            "a run-shaped layer places nearly every artifact in a node: {wide} wide of {live}"
        ),
        Shape::Nested => assert!(
            wide > 0 && wide < live,
            "a hierarchy's coarse nodes are wide and its leaves are not: {wide} of {live}"
        ),
        Shape::Scattered => assert_eq!(
            wide, live,
            "every artifact of a scattered layer is too wide for any node"
        ),
        Shape::Partitioned => assert!(
            wide * 20 < live,
            "a partitioned layer is run-shaped: {wide} wide of {live}"
        ),
    }
}

/// **The stale-narrow extent, which is what the one-snapshot rule is for**
/// (`2026-08-21-artifact-layout-selection.md` §9's first constraint).
///
/// A growth adds members, so an index built before it is **narrow**: the artifact's node no longer
/// contains its membership, and a viewport over the new region finds it in no node it touches. The
/// artifact is then absent with a visible member in view, which a viewer cannot tell from one that
/// failed a criterion.
///
/// The two arms are the point. Handed the pre-growth index — the state a second read of the store
/// would produce — the walk drops it. Built inside one snapshot, it serves it. And the cache's own
/// key is what makes the second arm what a request gets: the growth moved the level's version, so
/// the form and its index are rebuilt together.
#[test]
fn a_growth_between_two_reads_would_leave_the_extent_narrow() {
    let mut fx = build_fixture(Shape::Runs);
    let ordinal = 3u32;
    let stale = TileIndex::build(&fx.membership(), UNIVERSE);
    let Extent::Span { hi: was, .. } = stale.extent(ordinal) else {
        panic!("the fixture's artifact has a membership");
    };

    // The growth: the artifact gains members in a region far above its own span.
    let grown_lo = UNIVERSE - 500;
    let mut record = fx.store.get(LAYER, 0, ordinal).unwrap().clone();
    for row in grown_lo..UNIVERSE {
        record
            .members
            .to_mut()
            .add(fx.order[row as usize].raw() as u32);
    }
    assert!(
        was < grown_lo,
        "the growth must land outside the old extent"
    );
    fx.store.put(LAYER, 0, ordinal, record, None);

    let membership = fx.membership();
    let tiles = rows_of(&[grown_lo..=UNIVERSE - 1]);
    assert!(
        membership
            .get(ordinal)
            .is_some_and(|m| m.and_cardinality(&tiles) > 0),
        "the grown artifact has members in the viewport"
    );

    // The stale index: accepted by ordinal count, and the artifact is not a candidate at all.
    let stale_rows = ArtifactRows::build_over(fx.store.level(LAYER, 0), &fx.row_space, Some(stale));
    assert!(
        !stale_rows
            .index()
            .candidates(&tiles)
            .iter()
            .any(|o| o == ordinal),
        "a stale-narrow extent drops an artifact that has members in view — which is the hazard, \
         and this arm exists so the next one is not vacuous"
    );

    // One snapshot: the index is derived from the membership this walk just built.
    let fresh = fx.rows();
    assert!(
        fresh
            .index()
            .candidates(&tiles)
            .iter()
            .any(|o| o == ordinal),
        "the artifact grew into the viewport and the walk must return it"
    );

    // And what a request gets is the second arm, because the growth moved the level's version and
    // the whole family is rebuilt under one key.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    let (before, _) = projections.get_or_build(
        "v0",
        "s0",
        LAYER,
        0,
        &fx.store,
        &fx.row_space,
        None,
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert!(before
        .index()
        .candidates(&tiles)
        .iter()
        .any(|o| o == ordinal));
    assert_eq!(projections.builds(), 1);
}

/// **A hole and an empty projection are two states, and neither is ever a candidate.**
///
/// A hole is an ordinal a fold emptied, held open because an ordinal is identity; an empty
/// projection is a live artifact every member of which is awaiting a fold. Collapsing them would
/// make one of the two readable as the other — and the direction that costs is a hole read as an
/// artifact, which is the withholding the fold made durable.
#[test]
fn a_hole_is_absent_and_an_empty_projection_is_a_live_artifact() {
    let fx = build_fixture(Shape::Runs);
    let rows = fx.rows();
    let whole = rows_of(&[0..=UNIVERSE - 1]);
    let candidates = rows.index().candidates(&whole);

    for hole in &fx.holes {
        assert_eq!(rows.index().extent(*hole), Extent::Hole);
        assert!(
            !candidates.iter().any(|o| o == *hole),
            "a hole was returned as a candidate at ordinal {hole}"
        );
        assert!(rows.get(*hole).is_none());
        // And the level holds no record there, so no route reaches it: the serving path resolves an
        // ordinal to an entity through the store, and a hole has none.
        assert!(fx.entity_of(*hole).is_none());
    }

    let empty = fx.unprojectable;
    assert_eq!(rows.index().extent(empty), Extent::Empty);
    assert!(
        !candidates.iter().any(|o| o == empty),
        "an artifact with no visible row anywhere is in no viewport"
    );
    // **Still an artifact.** Its record is there, its entity resolves, and the predicate serves it
    // — which is what the identifier route reaches, and what a criterion-free layer shows with a
    // zero count.
    let declaration = declaration();
    let overlay = Overlay::new();
    let (mask, satisfied, denied) = fx.mask(&(0..TERMS).collect::<Vec<_>>(), &overlay);
    let view = ArtifactView {
        declaration: &declaration,
        overlay: &overlay,
        satisfied: &satisfied,
        layer_reachable: true,
        rows: &rows,
        dependency_served: &always_served,
        mask: &mask,
        denied: &denied,
        containment: None,
        counts: None,
    };
    let entity = fx.entity_of(empty).expect("the artifact is live");
    assert!(
        matches!(
            view.verdict(entity, empty, None),
            ArtifactVerdict::Serve {
                masked_count: 0,
                ..
            }
        ),
        "an artifact awaiting its fold is absent from the map and present on the identifier route"
    );
}

/// **The adoption rule, both directions.** A fold-written column is claimed only where the prefix,
/// the view and the level version are all the ones it was projected under, and derived at anything
/// else.
///
/// The direction that matters is the second, and it is the mirror image of the containment
/// partition's: a stale index is **narrow**, so it settles an artifact whose membership is not
/// inside the viewport — which turns the collapse one probe rests on into a claim that is false.
#[test]
fn a_tile_index_is_claimed_at_its_own_coordinate_and_at_no_other() {
    let fx = build_fixture(Shape::Runs);
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("partitions/default/tile-index")).unwrap();
    let rel = "partitions/default/tile-index/tile-index-000001-000.tsti";
    let index = TileIndex::build(&fx.membership(), UNIVERSE);
    std::fs::write(tmp.path().join(rel), index.as_bytes()).unwrap();

    // A coordinate that is a number rather than a zero, so *moved down* is expressible as well as
    // *moved up*.
    let mut store = fx.store.clone();
    store.seed_level_version(LAYER, 0, 11);
    let projected_at = store.level_version(LAYER, 0);
    let entry = |view: &str, version: u64| tessera_store::manifest::DerivedExtent {
        incarnation: Some(0),
        path: rel.to_string(),
        view: Some(view.to_string()),
        layer: LAYER.to_string(),
        level: 0,
        level_version: version,
        form: tessera_store::manifest::DerivedForm::TileIndex,
    };

    // The coordinate holds: claimed, and the level's first request derives nothing.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_indexes(tmp.path(), "v00000", &[entry("s0", projected_at)], &store);
    let (rows, _) = projections.get_or_build(
        "v00000",
        "s0",
        LAYER,
        0,
        &store,
        &fx.row_space,
        None,
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert_eq!(projections.indexes_adopted(), 1);
    assert_eq!(rows.index().len(), fx.ordinals as usize);

    // The level has moved since, in either direction: dropped, and the index is derived.
    for moved in [projected_at + 1, projected_at - 1] {
        let projections = ArtifactProjections::new(std::env::temp_dir());
        projections.adopt_indexes(tmp.path(), "v00000", &[entry("s0", moved)], &store);
        let _ = projections.get_or_build(
            "v00000",
            "s0",
            LAYER,
            0,
            &store,
            &fx.row_space,
            None,
            tessera_types::layer::ServingLayout::ArtifactMajor,
            None,
            0,
            false,
        );
        assert_eq!(
            projections.indexes_adopted(),
            0,
            "an index projected at {moved} must not answer for a level at {projected_at}"
        );
    }

    // Another prefix: row space renumbers wholesale at a fold, so an index held for one prefix must
    // not be claimed under another.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_indexes(tmp.path(), "v00000", &[entry("s0", projected_at)], &store);
    let _ = projections.get_or_build(
        "v00001",
        "s0",
        LAYER,
        0,
        &store,
        &fx.row_space,
        None,
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert_eq!(projections.indexes_adopted(), 0);

    // **Another view, which is the term a containment partition does not carry.** An extent is a
    // pair of rows, so a column belongs to exactly the row space it was projected through.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_indexes(tmp.path(), "v00000", &[entry("s0", projected_at)], &store);
    let _ = projections.get_or_build(
        "v00000",
        "s1",
        LAYER,
        0,
        &store,
        &fx.row_space,
        None,
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert_eq!(projections.indexes_adopted(), 0);

    // A file the manifest names that is not there is an absence, not a refusal: the level derives
    // its own, which is what every request did before the fold wrote anything.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    let mut missing = entry("s0", projected_at);
    missing.path = "partitions/default/tile-index/gone.tsti".to_string();
    projections.adopt_indexes(tmp.path(), "v00000", &[missing], &store);
    let _ = projections.get_or_build(
        "v00000",
        "s0",
        LAYER,
        0,
        &store,
        &fx.row_space,
        None,
        tessera_types::layer::ServingLayout::ArtifactMajor,
        None,
        0,
        false,
    );
    assert_eq!(projections.indexes_adopted(), 0);
}

/// A claimed index that does not cover the level's ordinals is refused rather than used. A shorter
/// column would leave every ordinal past its end out of every walk, so those artifacts stop being
/// served with nothing anywhere reporting a fault.
#[test]
fn an_index_over_another_population_is_refused_and_the_level_derives_its_own() {
    let fx = build_fixture(Shape::Runs);
    // A column over four ordinals, offered for a level of six hundred.
    let mut small = ArtifactStore::new();
    for ordinal in 0..4u32 {
        small.put(
            LAYER,
            0,
            ordinal,
            ArtifactRecord {
                entity: EntityId::new(u64::from(ordinal)),
                key: None,
                view: None,
                members: [fx.order[ordinal as usize].raw() as u32]
                    .into_iter()
                    .collect::<Bitmap>()
                    .into(),
                contents: Vec::new(),
                attached_to: None,
                parents: Vec::new(),
            },
            None,
        );
    }
    let short = TileIndex::build(
        &MembershipRows::build(small.level(LAYER, 0), &fx.row_space),
        UNIVERSE,
    );
    assert_eq!(short.len(), 4);

    let rows = ArtifactRows::build_over(fx.store.level(LAYER, 0), &fx.row_space, Some(short));
    assert_eq!(
        rows.index().len(),
        rows.len(),
        "the offered column covered another population and must have been refused"
    );
    let whole = rows_of(&[0..=UNIVERSE - 1]);
    assert!(rows.index().candidates(&whole).len() > 4);
}

/// **The fold's column and the row form's index are the same bytes.**
///
/// The fold projects extents straight from a level's records, one membership at a time, so that it
/// is not holding a whole level's row form beside the one the flip is about to build. That is a
/// residency choice and must not become a second derivation: the two must agree ordinal for ordinal
/// — including at a hole and at an empty projection, which is where two encoders would first
/// differ.
#[test]
fn the_folds_projection_and_the_derived_index_are_the_same_column() {
    for shape in [Shape::Runs, Shape::Nested, Shape::Scattered] {
        let fx = build_fixture(shape);
        let derived = TileIndex::build(&fx.membership(), fx.row_space.base_rows());
        let ordinals = fx.store.level(LAYER, 0).count() as u32;
        let projected = TileIndex::project(
            ordinals,
            || fx.store.level(LAYER, 0),
            &fx.row_space,
            &fx.store,
        );
        assert_eq!(
            projected.as_bytes(),
            derived.as_bytes(),
            "{shape:?}: the fold writes a different column from the one a request derives"
        );
        assert_eq!(projected.everywhere(), derived.everywhere());
        for ordinal in 0..derived.len() as u32 {
            assert_eq!(projected.extent(ordinal), derived.extent(ordinal));
        }
    }
}

/// **An entry held for the published prefix survives a claim under the outgoing one.** A fold
/// adopts what it wrote before it warms the levels, and the warm is sequential, so a request that
/// loaded the outgoing generation reaches the projections under the old prefix and the store's
/// new version while entries are waiting. A claim that removed on any mismatch took the entry
/// with it, and the warm then projected the level whole. Only a same-prefix version mismatch drops
/// an entry, and adoption purges what another prefix held.
#[test]
fn an_entry_held_for_the_published_prefix_survives_a_claim_under_the_outgoing_one() {
    let fx = build_fixture(Shape::Partitioned);
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("partitions/default/tile-index")).unwrap();
    std::fs::create_dir_all(tmp.path().join("partitions/default/row-column")).unwrap();
    let index_rel = "partitions/default/tile-index/tile-index-000001-000.tsti";
    let index = TileIndex::build(&fx.membership(), UNIVERSE);
    std::fs::write(tmp.path().join(index_rel), index.as_bytes()).unwrap();
    let column_rel = "partitions/default/row-column/row-column-000001-000.tsrc";
    let column = RowColumn::compose(
        fx.rows().membership(),
        fx.row_space.base_rows(),
        ServingLayout::RowMajorLabel,
        &std::env::temp_dir(),
    )
    .expect("a partitioned level composes a label column");
    std::fs::write(tmp.path().join(column_rel), column.as_bytes()).unwrap();

    // The level before the fold's retirement and after it: the version moved by one.
    let mut before = fx.store.clone();
    before.seed_level_version(LAYER, 0, 10);
    let mut store = fx.store.clone();
    store.seed_level_version(LAYER, 0, 11);
    let mut later = fx.store.clone();
    later.seed_level_version(LAYER, 0, 12);
    let index_entry = |version: u64| tessera_store::manifest::DerivedExtent {
        incarnation: Some(0),
        path: index_rel.to_string(),
        view: Some("s0".to_string()),
        layer: LAYER.to_string(),
        level: 0,
        level_version: version,
        form: tessera_store::manifest::DerivedForm::TileIndex,
    };
    let column_entry = |version: u64| tessera_store::manifest::DerivedExtent {
        incarnation: Some(0),
        path: column_rel.to_string(),
        view: Some("s0".to_string()),
        layer: LAYER.to_string(),
        level: 0,
        level_version: version,
        form: tessera_store::manifest::DerivedForm::RowColumn {
            layout: ServingLayout::RowMajorLabel,
        },
    };
    let build = |projections: &ArtifactProjections,
                 prefix: &str,
                 store: &tessera_lifecycle::membership::ArtifactStore,
                 layout: ServingLayout| {
        projections
            .get_or_build(
                prefix,
                "s0",
                LAYER,
                0,
                store,
                &fx.row_space,
                None,
                layout,
                None,
                0,
                false,
            )
            .0
    };

    // The index, for an artifact-major level. The outgoing prefix held an entry of its own; the
    // fold's adoption replaces it.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_indexes(tmp.path(), "v00000", &[index_entry(10)], &before);
    projections.adopt_indexes(tmp.path(), "v00001", &[index_entry(11)], &store);
    // A request still on the outgoing generation: its prefix, the store's version. Nothing is
    // claimed and nothing is removed.
    let _ = build(&projections, "v00000", &store, ServingLayout::ArtifactMajor);
    assert_eq!(projections.indexes_adopted(), 0);
    // The warm, under the published prefix, claims it.
    let rows = build(&projections, "v00001", &store, ServingLayout::ArtifactMajor);
    assert_eq!(
        projections.indexes_adopted(),
        1,
        "the index survived the outgoing generation's claim"
    );
    assert_eq!(rows.index().as_bytes(), index.as_bytes());

    // The column, for a row-major level, the same way.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_columns(tmp.path(), "v00000", &[column_entry(10)], &before);
    projections.adopt_columns(tmp.path(), "v00001", &[column_entry(11)], &store);
    let _ = build(&projections, "v00000", &store, ServingLayout::RowMajorLabel);
    assert_eq!(projections.columns_adopted(), 0);
    // The outgoing generation's request composed a column of its own, having nothing to claim;
    // the warm composes none.
    let composed_by_outgoing = projections.columns_composed();
    let rows = build(&projections, "v00001", &store, ServingLayout::RowMajorLabel);
    assert_eq!(
        projections.columns_adopted(),
        1,
        "the column survived the outgoing generation's claim"
    );
    assert_eq!(
        projections.columns_composed(),
        composed_by_outgoing,
        "and the warm transposed it rather than composing one"
    );
    assert!(rows.column().is_some());

    // The retention rule stands: under the entry's own prefix, a version it does not carry drops
    // it, and a later claim at its version finds nothing.
    let projections = ArtifactProjections::new(std::env::temp_dir());
    projections.adopt_indexes(tmp.path(), "v00001", &[index_entry(11)], &store);
    let _ = build(&projections, "v00001", &later, ServingLayout::ArtifactMajor);
    assert_eq!(projections.indexes_adopted(), 0);
    let _ = build(&projections, "v00001", &store, ServingLayout::ArtifactMajor);
    assert_eq!(
        projections.indexes_adopted(),
        0,
        "dropped at the same-prefix mismatch, so nothing is held for the process's life"
    );
}

/// **The row form transposed out of a column is the row form projected from the memberships** —
/// membership, generating sets, declared sizes and the tile index over them, artifact for
/// artifact.
///
/// This is what lets a level recorded row-major skip the projection at open: the column the fold
/// wrote already holds the membership, addressed by row, and reading it back costs one sequential
/// pass instead of decoding and permuting 1.6×10⁹ entries. If the two ever disagreed the failure
/// would be silent in the usual two directions — an artifact short a member is one that stops
/// being a candidate where it should be, and a masked count that comes back low.
///
/// **Both forms and all three shapes.** `Nested` is the overlapping case (a parent owns every
/// row its children do), where only the list form composes; `Runs` and `Scattered` compose as
/// label columns too where their memberships happen to partition. The hole and the artifact whose
/// projection is empty are in every fixture, and they are the two states a transposition could
/// confuse — it cannot tell them apart, and must not have to: the records do.
#[test]
fn a_transposed_row_form_is_the_projected_one() {
    for shape in [
        Shape::Runs,
        Shape::Nested,
        Shape::Scattered,
        Shape::Partitioned,
    ] {
        let fx = build_fixture(shape);
        let projected = fx.rows();
        let mut composed = 0;
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let Some(column) = RowColumn::compose(
                projected.membership(),
                fx.row_space.base_rows(),
                layout,
                &std::env::temp_dir(),
            ) else {
                continue;
            };
            composed += 1;
            let transposed = ArtifactRows::build_from_column(
                fx.store.level(LAYER, 0),
                &fx.row_space,
                &column,
                &mut None,
                false,
            )
            .expect("a column composed from this very level covers it");

            assert_eq!(transposed.membership().len(), projected.membership().len());
            for ordinal in 0..projected.membership().len() as u32 {
                let here = transposed.membership().get(ordinal);
                let there = projected.membership().get(ordinal);
                assert_eq!(
                    here.is_some(),
                    there.is_some(),
                    "{shape:?}/{layout:?}: a hole and a live artifact were confused at {ordinal}"
                );
                assert_eq!(
                    here.map(Bitmap::to_vec),
                    there.map(Bitmap::to_vec),
                    "{shape:?}/{layout:?}: the membership disagreed at ordinal {ordinal}"
                );
                // **Bit for bit, not merely set for set.** Both forms are written through the same
                // container encoder (`tessera_roaring::Sink`), so the serialized bytes agree too —
                // which is what makes `blocks_per_artifact` and every other statistic over the row
                // form the same number whichever route built it.
                assert_eq!(
                    here.map(|rows| rows.serialize::<croaring::Portable>()),
                    there.map(|rows| rows.serialize::<croaring::Portable>()),
                    "{shape:?}/{layout:?}: the encoded membership differs at ordinal {ordinal}"
                );
                assert_eq!(
                    transposed
                        .membership()
                        .generating(ordinal)
                        .iter()
                        .map(Bitmap::to_vec)
                        .collect::<Vec<_>>(),
                    projected
                        .membership()
                        .generating(ordinal)
                        .iter()
                        .map(Bitmap::to_vec)
                        .collect::<Vec<_>>(),
                    "{shape:?}/{layout:?}: a generating set disagreed at ordinal {ordinal}"
                );
                assert_eq!(
                    transposed.index().extent(ordinal),
                    projected.index().extent(ordinal),
                    "{shape:?}/{layout:?}: the extent disagreed at ordinal {ordinal}"
                );
            }
            assert_eq!(
                transposed.index().as_bytes(),
                projected.index().as_bytes(),
                "{shape:?}/{layout:?}: the tile index over the two forms differs"
            );
            // The column composed from the transposed form is the column it was transposed from,
            // which is the round trip the serving path takes: the level is served from the very
            // bytes this membership was read out of.
            let again = RowColumn::compose(
                transposed.membership(),
                fx.row_space.base_rows(),
                layout,
                &std::env::temp_dir(),
            )
            .expect("the same memberships compose the same form");
            assert_eq!(again.as_bytes(), column.as_bytes());
            for ordinal in 0..column.len() as u32 {
                assert_eq!(again.declared_size(ordinal), column.declared_size(ordinal));
            }
        }
        assert!(composed > 0, "{shape:?}: neither row-major form composed");
    }
}

/// **The perimeter, not the population.** At a narrow viewport over a large level the walk visits
/// an order of magnitude fewer nodes than there are artifacts, and returns an order of magnitude
/// fewer candidates. An assertion about the shape of the cost, not a benchmark.
#[test]
fn a_narrow_viewport_walks_the_perimeter_rather_than_the_level() {
    let fx = build_fixture(Shape::Runs);
    let rows = fx.rows();
    let declaration = declaration();
    let overlay = Overlay::new();
    let (mask, satisfied, denied) = fx.mask(&(0..TERMS).collect::<Vec<_>>(), &overlay);
    let view = ArtifactView {
        declaration: &declaration,
        overlay: &overlay,
        satisfied: &satisfied,
        layer_reachable: true,
        rows: &rows,
        dependency_served: &always_served,
        mask: &mask,
        denied: &denied,
        containment: None,
        counts: None,
    };

    let population = rows.len() as u64;
    let narrow = rows_of(&[30_000..=30_099]);
    let (served, nodes) = walk(&fx, &rows, &view, &narrow, &mask);
    assert!(
        nodes * 10 < population,
        "a hundred-row viewport visited {nodes} nodes over {population} artifacts"
    );
    assert!(
        (served.len() as u64) * 10 < population,
        "and returned {} of {population} artifacts",
        served.len()
    );
    // The whole map is the other end of the same statement: every occupied node is on the
    // perimeter of nothing, so the walk settles at the top and visits fewer nodes still.
    let whole = rows_of(&[0..=UNIVERSE - 1]);
    let (all, whole_nodes) = walk(&fx, &rows, &view, &whole, &mask);
    assert!(
        whole_nodes < 32,
        "the whole map visited {whole_nodes} nodes"
    );
    assert!(all.len() > served.len());
}
