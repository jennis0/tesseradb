//! §7.2's selection definition, tested against a synthetic segment with hand-chosen identities.
//!
//! Why a synthetic segment rather than a built bundle: the definition's behaviour is a function of
//! how `tessera_id` is *distributed* within a tile, and in a real bundle those identities come out
//! of the keyed bijection and cannot be chosen. `TilerItem` carries `tessera_id` directly, so
//! `write_segment` lets a test place a known identity at a known geometry — which is what makes the
//! density assertions below exact rather than statistical.
//!
//! The mask is built the same way `tests/compose.rs` builds one: postings over an identity
//! permutation, so **entity id == row index** and a test can name the rows it wants visible
//! directly. That is a property of this fixture only; nothing in the engine assumes it.

use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rustc_hash::FxHashSet;
use tempfile::TempDir;

use tessera_authz::{write_postings, FragmentCache, PostingsReader};
use tessera_engine::compose::{compose, EffectiveMask, RowProjection};
use tessera_engine::occupancy::{
    occupied_tiles, occupied_tiles_ladder_with_precision, TileSketch, SKETCH_PRECISION,
};
use tessera_engine::select::{
    decode_tier, DecodeTier, SelectParams, Selection, SelectionPart, SelectionParts, Threshold,
};
use tessera_lifecycle::{ChangeOp, IngestBuffer, Overlay};
use tessera_spatial::{fixed32, morton_of, tiler::sort_batch, Bounds, Tile, TilerItem};
use tessera_store::read::{ColumnsRef, CutIndex, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{tile_ranges, Permutation, RowSpace};
use tessera_types::{EntityId, TermId, TesseraId};

const EXTENT: Bounds = Bounds {
    x_min: 0.0,
    x_max: 1024.0,
    y_min: 0.0,
    y_max: 1024.0,
};

/// A segment plus the temp dir backing its mmaps, which must outlive it.
struct Segment {
    _temp: TempDir,
    data: SegmentData,
    /// `items` after `sort_batch`, i.e. in row order — so `row_order_ids()[r]` is row `r`'s id.
    sorted: Vec<TilerItem>,
}

impl Segment {
    /// Row `r`'s `tessera_id`, as stored.
    fn id_at(&self, row: u32) -> u64 {
        self.sorted[row as usize].tessera_id.raw()
    }

    fn row_count(&self) -> u32 {
        self.sorted.len() as u32
    }
}

/// Write a segment from `(x, y, tessera_id)` triples, sorting them into row order first.
fn segment_of(points: &[(f32, f32, u64)]) -> Segment {
    let temp = TempDir::new().unwrap();
    let mut items: Vec<TilerItem> = points
        .iter()
        .map(|&(x, y, id)| TilerItem {
            tessera_id: TesseraId::new(id),
            qx: fixed32(x as f64, EXTENT.x_min, EXTENT.x_max),
            qy: fixed32(y as f64, EXTENT.y_min, EXTENT.y_max),
            scalars: Vec::new(),
        })
        .collect();
    // `sort_batch` permutes a companion entity-id vector alongside the items. This fixture does not
    // use it: visibility is expressed directly in row space (see the module doc), so the companion
    // is a placeholder and its post-sort contents are deliberately ignored.
    let mut entity_ids: Vec<EntityId> = (0..items.len() as u64).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);
    write_segment(temp.path(), &items, &codes, &[]).unwrap();

    let data = SegmentData {
        seg_id: "seg0".to_string(),
        row_count: items.len() as u32,
        morton: MortonSlice::load(&temp.path().join("morton.u32")).unwrap(),
        cuts: CutIndex::load(
            &temp.path().join(CutIndex::FILE),
            items.len() as u32,
        )
        .unwrap(),
        columns: ColumnsRef::load(&temp.path().join("columns.arrow")).unwrap(),
    };
    Segment {
        _temp: temp,
        data,
        sorted: items,
    }
}

/// An [`EffectiveMask`] over exactly `visible_rows`, with no overlay or buffer effects.
///
/// Uses an identity permutation so entity ids and row indices coincide — see the module doc.
fn mask_over(visible_rows: &[u32], row_count: u32) -> (TempDir, EffectiveMask) {
    mask_over_with(
        visible_rows,
        row_count,
        &Overlay::default(),
        &IngestBuffer::default(),
    )
}

/// [`mask_over`], but composed against `overlay` — how the run-decode tests obtain masks with
/// non-empty diffs (suppressions land in `minus`, predicate-widens onto rows outside
/// `visible_rows` land in `plus`; entity id == row index, so the overlay names rows directly).
fn mask_over_with(
    visible_rows: &[u32],
    row_count: u32,
    overlay: &Overlay,
    buffer: &IngestBuffer,
) -> (TempDir, EffectiveMask) {
    let temp = TempDir::new().unwrap();
    let bound = row_count as u64;

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[visible_rows.to_vec()], 32).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], 0, &postings, &[], bound)
        .unwrap();

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).unwrap();
    let perm = RowSpace::new(
        Arc::new(Permutation::load(&perm_path).unwrap()),
        bound as u32,
    );

    let base = Arc::new(RowProjection::new(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let denied = tessera_engine::denied_rows_of(overlay, &perm);
    let mask = compose(&satisfied, overlay, buffer, base, &perm, &denied);
    (temp, mask)
}

/// §7.2's θ at one depth, over this fixture's single segment: `N_occ(depth)` counted inside the
/// mask, exactly as the engine's own call site counts it.
fn theta_at(mask: &EffectiveMask, seg: &SegmentData, m_target: u64, depth: u8) -> Threshold {
    Threshold::at_depth(
        mask.visible_total(),
        m_target,
        occupied_tiles(mask, &[(seg, 0)], depth),
    )
}

fn params(k_min: usize, cap: usize, threshold: Threshold) -> SelectParams {
    SelectParams {
        k_min,
        cap,
        threshold,
    }
}

/// The depth-`d` tile containing `(x, y)`.
fn tile_of(x: f32, y: f32, depth: u8) -> Tile {
    let code = morton_of(x as f64, y as f64, &EXTENT).raw();
    Tile {
        prefix: if depth == 0 {
            0
        } else {
            (code >> (32 - 2 * depth as u32)) as u64
        },
        depth,
    }
}

/// Run the definition over one tile, returning the served identities in served order.
fn served_ids(seg: &Segment, mask: &EffectiveMask, tile: &Tile, p: &SelectParams) -> Vec<u64> {
    let range = tile_ranges(&seg.data, tile);
    let visible = mask.count_range(range.clone());
    Selection::of(
        mask,
        &SelectionParts::new(&[SelectionPart::base(&seg.data, range, visible)]),
        p,
        visible,
    )
    .rows
    .into_iter()
    .map(|row| seg.id_at(row))
    .collect()
}

// ---------------------------------------------------------------------------------------------
// The discriminating test
// ---------------------------------------------------------------------------------------------

/// **The regression test for the priority-as-identity-prefix defect.**
///
/// Storage order is `(morton, tessera_id)`, so *within a single leaf Morton cell* row order already
/// is identity order and the retired first-*k* placeholder agreed with the definition there. The two
/// diverge only across cells — so a fixture must span several, and the divergence must be asserted
/// rather than assumed, or the test silently proves nothing.
///
/// The stakes: a sample taken in row order is a sample ordered by **permission signature**, because
/// entity IDs are signature-sorted permanently under I9. A principal whose visible set spans two
/// groups would see mostly whichever group was allocated lower entity IDs, however much larger the
/// other was. That keeps the letter of I7 and breaks its purpose.
#[test]
fn selection_is_the_bottom_m_by_identity_not_the_first_rows_in_morton_order() {
    // Eight points marching across distinct Morton cells inside the same depth-1 quadrant, with
    // identities assigned in *descending* order — so the lowest identities sit in the last cells.
    let points: Vec<(f32, f32, u64)> = (0..8u64)
        .map(|i| {
            let step = 4.0 * i as f32;
            (step, step, (8 - i) * (1u64 << 56))
        })
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    let tile = tile_of(0.0, 0.0, 1);
    let p = params(1, 3, Threshold::Saturated);
    let got = served_ids(&seg, &mask, &tile, &p);

    let mut expected: Vec<u64> = points.iter().map(|&(_, _, id)| id).collect();
    expected.sort_unstable();
    expected.truncate(3);
    assert_eq!(
        got, expected,
        "served set must be the three lowest identities"
    );

    let range = tile_ranges(&seg.data, &tile);
    assert!(
        range.end - range.start >= 4,
        "fixture must span several rows in one tile for this test to discriminate"
    );
    let first_rows: Vec<u64> = (range.start..range.start + 3)
        .map(|r| seg.id_at(r))
        .collect();
    assert_ne!(
        got, first_rows,
        "selection is returning the first k rows in Morton order — i.e. ordered by permission \
         signature, the defect this test exists to catch"
    );
}

// ---------------------------------------------------------------------------------------------
// The three clauses
// ---------------------------------------------------------------------------------------------

/// **I7.** A tile whose visible items all sit *above* the threshold still draws `k_min` marks. This
/// is the clause that stops the sparsest principals' maps going blank, and it may not be removed as
/// an optimisation.
#[test]
fn the_floor_clause_keeps_a_tile_non_empty_when_the_threshold_admits_nothing() {
    let points: Vec<(f32, f32, u64)> = (0..20u64)
        .map(|i| (4.0 * i as f32, 4.0 * i as f32, u64::MAX - i))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    // A cut of 1 admits nothing: every identity here is near u64::MAX.
    let p = params(2, 128, Threshold::Cut(1));
    let got = served_ids(&seg, &mask, &tile_of(0.0, 0.0, 0), &p);
    assert_eq!(got.len(), 2, "the floor must still serve k_min marks");

    let mut expected: Vec<u64> = points.iter().map(|&(_, _, id)| id).collect();
    expected.sort_unstable();
    assert_eq!(
        got,
        expected[0..2],
        "the floor serves the LOWEST identities"
    );
}

/// No non-empty tile ever serves zero marks, for any threshold and any positive cap — the property
/// owner decision 4 turns on. Fewer marks than the retired flat *k* is intended; **none** is not.
#[test]
fn no_non_empty_tile_ever_serves_zero_marks() {
    let points: Vec<(f32, f32, u64)> = (0..12u64)
        .map(|i| (4.0 * i as f32, 4.0 * i as f32, u64::MAX - i))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    for cut in [
        Threshold::Cut(1),
        Threshold::Cut(1 << 40),
        Threshold::Saturated,
    ] {
        for cap in [1usize, 2, 7, 128] {
            // k_min starts at 1: a floor of 0 is not a valid configuration, and
            // `a_zero_floor_would_blank_a_tile_which_is_why_config_refuses_it` below pins why.
            for k_min in [1usize, 2, 5] {
                let p = params(k_min, cap, cut);
                let got = served_ids(&seg, &mask, &tile_of(0.0, 0.0, 0), &p);
                assert!(
                    !got.is_empty(),
                    "empty map at k_min={k_min} cap={cap} cut={cut:?}"
                );
            }
        }
    }
}

/// Why `k_min = 0` is refused at config load rather than clamped.
///
/// With no floor, `m = min(cap, max(0, C_θ))` is 0 whenever the threshold admits nothing, and the
/// tile goes blank despite having visible items — I7 gone, with no error anywhere. This test pins
/// the consequence so the `ConfigError::FloorClauseDisabled` refusal has a demonstrated reason
/// rather than an asserted one, and so nobody "simplifies" the refusal away later.
#[test]
fn a_zero_floor_would_blank_a_tile_which_is_why_config_refuses_it() {
    let points: Vec<(f32, f32, u64)> = (0..12u64)
        .map(|i| (4.0 * i as f32, 4.0 * i as f32, u64::MAX - i))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    let no_floor = params(0, 8, Threshold::Cut(1));
    let got = served_ids(&seg, &mask, &tile_of(0.0, 0.0, 0), &no_floor);
    assert!(
        got.is_empty(),
        "a zero floor is expected to blank a tile whose items are all above the threshold — if \
         this no longer holds, revisit ConfigError::FloorClauseDisabled"
    );

    // The same tile with the smallest legal floor is not blank.
    let with_floor = params(1, 8, Threshold::Cut(1));
    assert_eq!(
        served_ids(&seg, &mask, &tile_of(0.0, 0.0, 0), &with_floor).len(),
        1
    );
}

/// **The density signal.** Two tiles of different visible count, with θ positioned so `θ·n` lands
/// strictly inside `(floor, cap)` for both: mark count must track visible count.
///
/// Identities are spread evenly over the identity space within each tile, so `C_θ` is exact rather
/// than statistical — a cut at `2⁶⁴/4` admits exactly a quarter of each tile.
#[test]
fn mark_count_tracks_visible_count_inside_the_window() {
    // Tile A: 40 points in the bottom-left depth-1 quadrant. Tile B: 80 in the top-right.
    let mut points: Vec<(f32, f32, u64)> = Vec::new();
    for i in 0..40u64 {
        points.push((4.0 * i as f32, 4.0, spread(i, 40)));
    }
    for i in 0..80u64 {
        points.push((512.0 + 4.0 * i as f32, 600.0, spread(i, 80)));
    }
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    // A quarter of the identity space.
    let p = params(1, 100, Threshold::Cut(1u64 << 62));
    let a = served_ids(&seg, &mask, &tile_of(0.0, 4.0, 1), &p);
    let b = served_ids(&seg, &mask, &tile_of(600.0, 600.0, 1), &p);

    assert_eq!(a.len(), 10, "40 visible, a quarter admitted");
    assert_eq!(b.len(), 20, "80 visible, a quarter admitted");
    assert!(
        b.len() > a.len(),
        "the denser tile must draw more marks — this is the whole point of the threshold clause"
    );
}

/// `spread(i, n)` — the `i`th of `n` identities spread evenly over the identity space, so that a cut
/// at a fraction `f` of `2⁶⁴` admits exactly `ceil(f·n)` of them.
fn spread(i: u64, n: u64) -> u64 {
    ((i as u128) * (1u128 << 64) / (n as u128)) as u64
}

/// **The accepted residual, pinned by a test rather than by prose.**
///
/// Where `C_θ >= cap`, tiles of very different visible count serve the same number of marks and
/// mark count stops reading as density. This is the cap-flat region: accepted by the owner
/// (decision 2, 2026-07-30) over both a measured per-session θ anchor and a client-supplied θ, and
/// backstopped by the §3.3 density underlay.
///
/// **This test asserts the flatness on purpose.** If a later reader "fixes" it by making θ adapt to
/// real occupancy, this test fails and sends them to the decision rather than letting the change
/// land silently.
#[test]
fn the_accepted_cap_flat_region_serves_equally_from_unequal_tiles() {
    let mut points: Vec<(f32, f32, u64)> = Vec::new();
    for i in 0..40u64 {
        points.push((4.0 * i as f32, 4.0, spread(i, 40)));
    }
    for i in 0..80u64 {
        points.push((512.0 + 4.0 * i as f32, 600.0, spread(i, 80)));
    }
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    // Half the identity space admitted: C_theta is 20 and 40, both at or above a cap of 15.
    let p = params(1, 15, Threshold::Cut(1u64 << 63));
    let a = served_ids(&seg, &mask, &tile_of(0.0, 4.0, 1), &p);
    let b = served_ids(&seg, &mask, &tile_of(600.0, 600.0, 1), &p);

    assert_eq!(a.len(), 15);
    assert_eq!(
        b.len(),
        15,
        "a tile with twice the visible count serves the same number of marks once both are above \
         the cap — the accepted cap-flat region (owner decision 2)"
    );
}

// ---------------------------------------------------------------------------------------------
// Nesting
// ---------------------------------------------------------------------------------------------

/// **The stability property.** An item drawn in a parent tile is still drawn in whichever child
/// contains it, so marks never pop out on zoom-in.
///
/// The proof needs three things and this test exercises all of them: ranks fall under a subset,
/// θ is monotone in depth (`θ_{d+1} = 4·θ_d`), and **`cap` is the same at both depths**. That last
/// premise is a client obligation — a client that *reduces* `k` while zooming in forfeits nesting —
/// so the test holds `cap` fixed, which is the documented contract, and
/// [`cap_decreasing_on_descent_can_drop_a_mark`] pins the documented consequence of not doing so.
#[test]
fn a_drawn_mark_is_still_drawn_in_the_child_that_contains_it() {
    use std::collections::{HashMap, HashSet};

    let mut rng = StdRng::seed_from_u64(0xD0_1CE);
    let points: Vec<(f32, f32, u64)> = (0..600)
        .map(|_| {
            (
                rng.gen_range(0.0f32..1024.0),
                rng.gen_range(0.0f32..1024.0),
                rng.gen(),
            )
        })
        .collect();
    let seg = segment_of(&points);
    // A realistic mask: about two thirds visible, scattered.
    let visible: Vec<u32> = (0..seg.row_count()).filter(|r| r % 3 != 0).collect();
    let (_t, mask) = mask_over(&visible, seg.row_count());

    let cap = 12;
    // Counted and asserted below: without it this test can pass vacuously if the fixture, the mask
    // or the parameters ever drift such that no mark is drawn in a tile that has a populated child.
    let mut checked = 0usize;

    for depth in 0..6u8 {
        let parent_params = params(2, cap, theta_at(&mask, &seg.data, 8, depth));
        let child_params = params(2, cap, theta_at(&mask, &seg.data, 8, depth + 1));

        // Deduplicate: the loop is over points, but many points share a tile — at depth 0 all 600
        // do. Without this the same tile is re-selected hundreds of times.
        let parents: HashSet<u64> = points
            .iter()
            .map(|&(x, y, _)| tile_of(x, y, depth).prefix)
            .collect();

        for parent_prefix in parents {
            let parent = Tile {
                prefix: parent_prefix,
                depth,
            };
            let drawn = served_ids(&seg, &mask, &parent, &parent_params);
            if drawn.is_empty() {
                continue;
            }

            // Which child holds each drawn identity — built once per parent by walking the parent's
            // rows, rather than rescanning each child range per identity.
            let parent_range = tile_ranges(&seg.data, &parent);
            let mut child_of: HashMap<u64, u64> = HashMap::new();
            for row in parent_range.start..parent_range.end {
                if !mask.contains_row(row) {
                    continue;
                }
                child_of.insert(seg.id_at(row), tile_of_row(&seg, row, depth + 1).prefix);
            }

            let mut served_by_child: HashMap<u64, Vec<u64>> = HashMap::new();
            for id in &drawn {
                let Some(&child_prefix) = child_of.get(id) else {
                    continue;
                };
                let entry = served_by_child.entry(child_prefix).or_insert_with(|| {
                    served_ids(
                        &seg,
                        &mask,
                        &Tile {
                            prefix: child_prefix,
                            depth: depth + 1,
                        },
                        &child_params,
                    )
                });
                assert!(
                    entry.contains(id),
                    "mark {id:#x} drawn at depth {depth} vanished in its own child at depth {}",
                    depth + 1
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked > 200,
        "only {checked} drawn marks were followed into a child — this test is not exercising \
         nesting, so its silence proves nothing"
    );
}

/// The depth-`d` tile row `row` falls in, read back from the segment so a test never assumes the
/// pre-sort order.
///
/// Taken from the stored cell code rather than from a coordinate: no coordinate is stored, and a
/// tile prefix is a prefix of that code anyway — the same shift `tile_of` performs after
/// quantising.
fn tile_of_row(seg: &Segment, row: u32, depth: u8) -> Tile {
    let code = seg.data.morton.u32()[row as usize];
    Tile {
        prefix: if depth == 0 {
            0
        } else {
            (code >> (32 - 2 * depth as u32)) as u64
        },
        depth,
    }
}

/// The documented consequence of breaking the nesting premise: a client that reduces `cap` on
/// descent can lose a mark it was already shown.
///
/// This is asserted rather than merely written down, so the obligation stated in `select.rs`'s
/// module doc and in contracts §3.2 is pinned by a test. It is **not** a defect — it is why `k`
/// must be non-decreasing on zoom-in.
#[test]
fn cap_decreasing_on_descent_can_drop_a_mark() {
    let points: Vec<(f32, f32, u64)> = (0..16u64)
        .map(|i| (2.0 * i as f32, 2.0, spread(i, 16)))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    let parent = tile_of(0.0, 2.0, 0);
    let child = tile_of(0.0, 2.0, 1);

    let wide = params(1, 8, Threshold::Saturated);
    let narrow = params(1, 2, Threshold::Saturated);

    let at_parent = served_ids(&seg, &mask, &parent, &wide);
    let at_child_same_cap = served_ids(&seg, &mask, &child, &wide);
    let at_child_narrowed = served_ids(&seg, &mask, &child, &narrow);

    // Held fixed, nesting holds.
    for id in &at_parent {
        let child_range = tile_ranges(&seg.data, &child);
        if (child_range.start..child_range.end).any(|r| seg.id_at(r) == *id) {
            assert!(
                at_child_same_cap.contains(id),
                "nesting must hold at a fixed cap"
            );
        }
    }
    // Narrowed, it does not — which is the client obligation, documented not enforced.
    assert!(
        at_child_narrowed.len() < at_child_same_cap.len(),
        "narrowing the cap on descent must be able to drop marks, or this fixture does not \
         exercise the premise the nesting proof depends on"
    );
}

// ---------------------------------------------------------------------------------------------
// The fast path
// ---------------------------------------------------------------------------------------------

/// **Selection matches the definition, computed independently, over both internal branches.**
///
/// The serve-all branch is no longer reachable as a public route, so this asserts against a
/// brute-force reference rather than against the other branch — which is the stronger check anyway:
/// comparing two branches proves only that they agree, while this proves both compute §7.2.
///
/// The sweep is counted **per branch** and both counts are asserted. An earlier version asserted a
/// single `compared > 200`, which a review pointed out was two orders of magnitude below what the
/// sweep produces and would still have passed if one branch had stopped being exercised entirely.
#[test]
fn selection_matches_the_definition_over_both_internal_branches() {
    let mut rng = StdRng::seed_from_u64(0xFA_57);
    let points: Vec<(f32, f32, u64)> = (0..400)
        .map(|_| {
            (
                rng.gen_range(0.0f32..1024.0),
                rng.gen_range(0.0f32..1024.0),
                rng.gen(),
            )
        })
        .collect();
    let seg = segment_of(&points);
    let visible: Vec<u32> = (0..seg.row_count()).filter(|r| r % 2 == 0).collect();
    let (_t, mask) = mask_over(&visible, seg.row_count());

    let mut served_all = 0usize;
    let mut selected = 0usize;

    // Parameters and depths chosen so both branches fire: saturated-and-under-cap at shallow depths
    // with a generous cap, floor-covered at deep depths where tiles hold one or two rows, and a live
    // cut with a small cap to force real selection.
    for threshold in [
        Threshold::Saturated,
        theta_at(&mask, &seg.data, 4, 3),
    ] {
        for cap in [1usize, 4, 64, 4096] {
            for k_min in [1usize, 2, 6] {
                let p = params(k_min, cap, threshold);
                for depth in [0u8, 2, 5, 8] {
                    for (x, y, _) in points.iter().copied() {
                        let tile = tile_of(x, y, depth);
                        let range = tile_ranges(&seg.data, &tile);
                        let vis = mask.count_range(range.clone());
                        if vis == 0 {
                            continue;
                        }
                        let got = Selection::of(
                            &mask,
                            &SelectionParts::new(&[SelectionPart::base(
                                &seg.data,
                                range.clone(),
                                vis,
                            )]),
                            &p,
                            vis,
                        );
                        let want = reference_served(&seg, &mask, range, &p);
                        assert_eq!(
                            got.rows, want,
                            "selection disagrees with the definition at depth {depth}, \
                             k_min={k_min}, cap={cap}, visible={vis}, threshold={threshold:?}"
                        );
                        if want.len() as u64 == vis {
                            served_all += 1;
                        } else {
                            selected += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(
        served_all > 100,
        "only {served_all} tiles served everything visible — the serve-all branch is barely covered"
    );
    assert!(
        selected > 100,
        "only {selected} tiles needed real selection — the counting/heap branch is barely covered"
    );
}

/// §7.2's definition, brute force: materialise the tile's visible rows, sort by `tessera_id`, count
/// how many fall below the cut, and slice. Deliberately shaped unlike the engine's single-pass
/// bounded heap — a reference that mirrored the implementation would prove nothing.
fn reference_served(
    seg: &Segment,
    mask: &EffectiveMask,
    range: std::ops::Range<u32>,
    p: &SelectParams,
) -> Vec<u32> {
    let mut vis: Vec<(u64, u32)> = mask
        .rows_in_range(range)
        .iter()
        .map(|row| (seg.id_at(row), row))
        .collect();
    vis.sort_unstable();
    let c_theta = vis.iter().filter(|(id, _)| p.threshold.admits(*id)).count();
    let floor = p.k_min.min(p.cap);
    let m = p.cap.min(floor.max(c_theta)).min(vis.len());
    vis[..m].iter().map(|&(_, row)| row).collect()
}

/// A saturated tile **over** the cap still needs real selection: taking the serve-all shortcut there
/// would emit more marks than the definition allows.
#[test]
fn a_saturated_tile_over_the_cap_is_still_capped() {
    let points: Vec<(f32, f32, u64)> = (0..40u64)
        .map(|i| (2.0 * i as f32, 2.0, spread(i, 40)))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    let p = params(2, 10, Threshold::Saturated);
    let tile = tile_of(0.0, 2.0, 0);
    let range = tile_ranges(&seg.data, &tile);
    let vis = mask.count_range(range.clone());
    assert_eq!(vis, 40);

    let got = Selection::of(
        &mask,
        &SelectionParts::new(&[SelectionPart::base(&seg.data, range, vis)]),
        &p,
        vis,
    );
    assert_eq!(got.rows.len(), 10, "the cap must still bind");
}

/// A request for no points yields no points — the count-only arm the benches measure — and does so
/// whichever branch the parameters would otherwise select.
#[test]
fn a_zero_cap_serves_no_points() {
    let points: Vec<(f32, f32, u64)> = (0..8u64)
        .map(|i| (2.0 * i as f32, 2.0, spread(i, 8)))
        .collect();
    let seg = segment_of(&points);
    let all_rows: Vec<u32> = (0..seg.row_count()).collect();
    let (_t, mask) = mask_over(&all_rows, seg.row_count());

    let tile = tile_of(0.0, 2.0, 0);
    let range = tile_ranges(&seg.data, &tile);
    let vis = mask.count_range(range.clone());
    for threshold in [Threshold::Saturated, Threshold::Cut(1)] {
        let p = params(2, 0, threshold);
        let got = Selection::of(
            &mask,
            &SelectionParts::new(&[SelectionPart::base(&seg.data, range.clone(), vis)]),
            &p,
            vis,
        );
        assert!(
            got.rows.is_empty(),
            "cap 0 must serve nothing ({threshold:?})"
        );
        assert_eq!(
            got.rows_visited, 0,
            "the cap-0 early return reads no rows, so it must report none read ({threshold:?})"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Masking (I7): selection never reaches outside the mask
// ---------------------------------------------------------------------------------------------

/// Every served row must be visible. The definition is evaluated *inside* the mask — the ranks and
/// the count `C_θ` are both taken over `vis(T)` — so a served row outside the mask would be a
/// straightforward I7 violation, not merely an imprecision.
#[test]
fn every_served_row_is_visible() {
    let mut rng = StdRng::seed_from_u64(0x15_EE);
    let points: Vec<(f32, f32, u64)> = (0..300)
        .map(|_| {
            (
                rng.gen_range(0.0f32..1024.0),
                rng.gen_range(0.0f32..1024.0),
                rng.gen(),
            )
        })
        .collect();
    let seg = segment_of(&points);
    let visible: Vec<u32> = (0..seg.row_count()).filter(|r| r % 5 == 1).collect();
    let (_t, mask) = mask_over(&visible, seg.row_count());

    for depth in 0..5u8 {
        let p = params(2, 16, theta_at(&mask, &seg.data, 8, depth));
        for (x, y, _) in points.iter().copied() {
            let tile = tile_of(x, y, depth);
            let range = tile_ranges(&seg.data, &tile);
            let vis = mask.count_range(range.clone());
            if vis == 0 {
                continue;
            }
            for row in Selection::of(
                &mask,
                &SelectionParts::new(&[SelectionPart::base(&seg.data, range, vis)]),
                &p,
                vis,
            )
            .rows
            {
                assert!(
                    mask.contains_row(row),
                    "row {row} was served but is not visible (I7)"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The run decode (B9): mechanism equivalence against the retired per-value path
// ---------------------------------------------------------------------------------------------

/// The pre-B9 `Selection::of`, transcribed: materialise `rows_in_range`, iterate it one value at
/// a time, serve-all and counting/heap branches exactly as they stood. Kept as the oracle because
/// the run decode claims to be a *mechanism* change only — so the reference is the retired
/// mechanism itself, not the definition (the definition is already pinned by
/// [`selection_matches_the_definition_over_both_internal_branches`]).
///
/// Returns `(rows, rows_visited)`: the counter is first-class output here, asserted un-gated —
/// a clamp off-by-one in the run decode would corrupt the bench's C4 numerator without failing
/// any served-set assertion.
fn per_value_selection(
    seg: &Segment,
    mask: &EffectiveMask,
    range: std::ops::Range<u32>,
    p: &SelectParams,
    visible: u64,
) -> (Vec<u32>, u64) {
    use std::collections::BinaryHeap;

    if p.cap == 0 {
        return (Vec::new(), 0);
    }
    let ids = seg.data.columns.tessera_id();
    let visible_rows = mask.rows_in_range(range);
    let floor = p.k_min.min(p.cap);
    let serves_all =
        visible <= floor as u64 || (p.threshold.is_saturated() && visible <= p.cap as u64);

    let mut rows_visited: u64 = 0;
    let rows: Vec<u32> = if serves_all {
        let mut rows: Vec<u32> = Vec::new();
        for row in visible_rows.iter() {
            rows_visited += 1;
            rows.push(row);
        }
        rows.sort_unstable_by_key(|&row| ids[row as usize]);
        rows
    } else {
        let mut c_theta: u64 = 0;
        let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::new();
        for row in visible_rows.iter() {
            rows_visited += 1;
            let id = ids[row as usize];
            if p.threshold.admits(id) {
                c_theta += 1;
            }
            if heap.len() == p.cap {
                if id >= heap.peek().expect("non-empty at len == cap").0 {
                    continue;
                }
                heap.pop();
            }
            heap.push((id, row));
        }
        let m = p
            .cap
            .min(floor.max(usize::try_from(c_theta).unwrap_or(usize::MAX)))
            .min(usize::try_from(visible).unwrap_or(usize::MAX));
        let mut kept: Vec<(u64, u32)> = heap.into_vec();
        kept.sort_unstable();
        kept.truncate(m);
        kept.into_iter().map(|(_, row)| row).collect()
    };
    (rows, rows_visited)
}

/// **The adaptive-decode equivalence property.** `Selection::of`'s tiered decode is
/// bit-identical — same `rows`, same `rows_visited` — to the retired per-value path, over
/// randomised masks, ranges and parameters, on **both** decode routes, **both** internal
/// branches, and **all three** tiers.
///
/// The diffs-empty arm is the one that exercises the direct-over-`base` decodes; a corpus that
/// always has non-empty diffs tests only the fallback, which shares its bitmap with the oracle
/// and proves nothing. The route predicate is `diffs_are_empty` and the tier predicate is the
/// engine's own `decode_tier` (imported, not transcribed, so the stratification cannot drift
/// from the gate), so asserting the route per mask plus the fired-counter floors below pins that
/// every route × tier and route × branch combination was genuinely reached — mirroring the
/// per-branch coverage asserts in
/// [`selection_matches_the_definition_over_both_internal_branches`].
///
/// Tier-0 and tier-1 corpora cannot be left to chance (a random range almost never lands with
/// ≥95% density), so two of the four range flavours are crafted against the block shapes: a
/// range wholly inside a fully visible stretch (tier 0) and a visible block plus a short
/// invisible tail (density ≥ the gate constant but not full — tier 1). With diffs present the
/// overlay is confined to the lower third of the row space and the crafted ranges to the upper
/// two thirds, so the diffs flip the *route* without dirtying the crafted densities.
#[test]
fn tiered_decode_matches_the_per_value_path_on_all_tiers_routes_and_branches() {
    use std::collections::HashSet;

    let mut rng = StdRng::seed_from_u64(0xB9_0002);
    let points: Vec<(f32, f32, u64)> = (0..4096)
        .map(|_| {
            (
                rng.gen_range(0.0f32..1024.0),
                rng.gen_range(0.0f32..1024.0),
                rng.gen(),
            )
        })
        .collect();
    let seg = segment_of(&points);
    let n = seg.row_count();
    let diffs_bound = n / 3; // overlay effects stay below this row

    // Four visibility shapes, density-stratified: sparse scatter and dense scatter feed tier 2,
    // 256-row blocks feed tiers 0 (inside a block) and 1 (block + short tail) via the crafted
    // ranges, and the 95%-scatter shape gives the run tier organic wide-range hits at the gate
    // boundary.
    #[derive(Clone, Copy, PartialEq)]
    enum Shape {
        Sparse,
        Dense,
        Blocks,
        NearFull,
    }
    let shapes: Vec<(Shape, Vec<u32>)> = vec![
        (
            Shape::Sparse,
            (0..n).filter(|_| rng.gen_bool(0.07)).collect(),
        ),
        (Shape::Dense, (0..n).filter(|_| rng.gen_bool(0.7)).collect()),
        (
            Shape::Blocks,
            (0..n).filter(|r| (r / 256) % 2 == 0).collect(),
        ),
        (Shape::NearFull, (0..n).filter(|r| r % 20 != 0).collect()),
    ];

    // fired_branch[route][branch]: branch 0 = serve-all, 1 = counting/heap.
    // fired_tier[route][tier]: tier by the engine's own gate. Route 0 = diffs empty.
    let mut fired_branch = [[0usize; 2]; 2];
    let mut fired_tier = [[0usize; 3]; 2];

    for (shape, visible_rows) in &shapes {
        for diffs_present in [false, true] {
            let mut overlay = Overlay::new();
            let mut buffer = IngestBuffer::default();
            if diffs_present {
                // `minus ⊆ base`: suppress a sample of visible rows below `diffs_bound`.
                // `plus ∩ base = ∅`: buffer a sample of invisible rows below `diffs_bound` under
                // the granted term (entity id == row index in this fixture, and the permutation is
                // the identity, so a buffered entity has a row here). The evaluate store used to
                // supply this half; with it gone (decision 0048) the buffer is the only source of
                // a `plus` row.
                for &row in visible_rows
                    .iter()
                    .filter(|&&r| r < diffs_bound)
                    .step_by(43)
                {
                    overlay.apply(EntityId::new(row as u64), ChangeOp::Suppress);
                }
                let vis_set: HashSet<u32> = visible_rows.iter().copied().collect();
                for row in (0..diffs_bound)
                    .filter(|r| !vis_set.contains(r))
                    .step_by(11)
                {
                    buffer.insert_row_with_terms(
                        &tessera_lifecycle::WalRow {
                            external_id: None,
                            entity_id: EntityId::new(row as u64),
                            view: "s0".to_string(),
                            join: false,
                            descriptors: Vec::new(),
                            x: 0.0,
                            y: 0.0,
                            scalars: Vec::new(),
                            scoped: Vec::new(),
                        },
                        vec![TermId::new(0)],
                    );
                }
            }
            let (_t, mask) = mask_over_with(visible_rows, n, &overlay, &buffer);
            assert_eq!(
                mask.diffs_are_empty(),
                !diffs_present,
                "route-coverage precondition: the fixture must actually put each mask on the \
                 route this arm claims to test"
            );

            for threshold in [
                Threshold::Saturated,
                Threshold::Cut(1),
                Threshold::Cut(1u64 << 62),
                Threshold::Cut(u64::MAX),
            ] {
                for cap in [1usize, 4, 30, 4096] {
                    for k_min in [1usize, 2] {
                        let p = params(k_min, cap, threshold);
                        for i in 0..16 {
                            let range = match i % 4 {
                                // Wide random: multi-run decodes, the counting/heap branch.
                                0 => {
                                    let a = rng.gen_range(0..n);
                                    let b = rng.gen_range(0..n);
                                    a.min(b)..a.max(b) + 1
                                }
                                // Narrow random: the low counts the serve-all branch fires on.
                                1 => {
                                    let a = rng.gen_range(0..n);
                                    a..(a + rng.gen_range(1..64)).min(n)
                                }
                                // Tier 0 crafted: wholly inside a fully visible stretch.
                                2 => match shape {
                                    Shape::Blocks => {
                                        // Visible blocks start at 512·j; stay above diffs_bound.
                                        let j = rng.gen_range(3..8u32);
                                        let start = 512 * j + rng.gen_range(0..200);
                                        start..start + rng.gen_range(1..56)
                                    }
                                    Shape::NearFull => {
                                        // Rows 20k+1 ..= 20k+19 are all visible.
                                        let k = rng.gen_range(69..203u32);
                                        let start = 20 * k + 1 + rng.gen_range(0..10);
                                        start..start + rng.gen_range(1..9)
                                    }
                                    _ => {
                                        let a = rng.gen_range(0..n);
                                        a..(a + rng.gen_range(1..64)).min(n)
                                    }
                                },
                                // Tier 1 crafted: a visible block plus a short invisible tail —
                                // density ≥ the gate constant, strictly below full.
                                _ => match shape {
                                    Shape::Blocks => {
                                        let j = rng.gen_range(3..7u32);
                                        let start = 512 * j;
                                        start..start + 256 + rng.gen_range(1..13)
                                    }
                                    _ => {
                                        let a = rng.gen_range(0..n);
                                        let b = rng.gen_range(0..n);
                                        a.min(b)..a.max(b) + 1
                                    }
                                },
                            };
                            let vis = mask.count_range(range.clone());
                            if vis == 0 {
                                continue;
                            }
                            let got = Selection::of(
                                &mask,
                                &SelectionParts::new(&[SelectionPart::base(
                                    &seg.data,
                                    range.clone(),
                                    vis,
                                )]),
                                &p,
                                vis,
                            );
                            let (want_rows, want_visited) =
                                per_value_selection(&seg, &mask, range.clone(), &p, vis);
                            assert_eq!(
                                got.rows, want_rows,
                                "tiered decode diverged from the per-value path: diffs_present=\
                                 {diffs_present} k_min={k_min} cap={cap} threshold={threshold:?} \
                                 range={range:?} visible={vis}"
                            );
                            // R3: the counter, un-gated. Every tier counts the clamped rows it
                            // actually reads, so both sides must equal the visible cardinality.
                            assert_eq!(
                                got.rows_visited, want_visited,
                                "rows_visited diverged at diffs_present={diffs_present} \
                                 cap={cap} threshold={threshold:?} range={range:?}"
                            );
                            assert_eq!(
                                want_visited, vis,
                                "the per-value oracle itself must read exactly the visible \
                                 cardinality — if this fails the fixture is broken, not the \
                                 tiered decode"
                            );

                            // Stratify by the engine's own predicates.
                            let range_len = u64::from(range.end - range.start);
                            let tier = match decode_tier(vis, range_len) {
                                DecodeTier::FullRange => 0,
                                DecodeTier::Runs => 1,
                                DecodeTier::Values => 2,
                            };
                            fired_tier[usize::from(diffs_present)][tier] += 1;
                            let floor = p.k_min.min(p.cap);
                            let serves_all = vis <= floor as u64
                                || (p.threshold.is_saturated() && vis <= p.cap as u64);
                            fired_branch[usize::from(diffs_present)][usize::from(!serves_all)] += 1;
                        }
                    }
                }
            }
        }
    }

    for (route, route_name) in [(0, "diffs-empty"), (1, "diffs-present fallback")] {
        for (branch, branch_name) in [(0, "serve-all"), (1, "counting/heap")] {
            assert!(
                fired_branch[route][branch] > 100,
                "only {} comparisons hit the {route_name} route's {branch_name} branch — that \
                 combination is barely covered and this test's silence proves nothing for it",
                fired_branch[route][branch]
            );
        }
        for (tier, tier_name) in [(0, "full-range"), (1, "runs"), (2, "values")] {
            assert!(
                fired_tier[route][tier] > 100,
                "only {} comparisons hit the {route_name} route's {tier_name} tier — that \
                 combination is barely covered and this test's silence proves nothing for it",
                fired_tier[route][tier]
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// §7.2's second θ anchor: `N_occ(d)`, the occupied-tile count inside the mask
// ---------------------------------------------------------------------------------------------

/// The linear-scan oracle for `N_occ(d)`: one pass over every visible row, counting distinct
/// depth-`d` Morton prefixes.
///
/// Correct without any argument about runs or gallops — the codes ascend with the row id, so a
/// change of prefix is a new tile — and slow by the same token. It is the baseline
/// [`occupied_tiles`] is asserted against, and it shares no code with it.
fn occupied_tiles_oracle(mask: &EffectiveMask, seg: &SegmentData, depth: u8) -> u64 {
    let codes = seg.morton.u32();
    let shift = 32 - 2 * u32::from(depth);
    let mut seen: FxHashSet<u64> = FxHashSet::default();
    mask.for_each_visible_run(0..seg.row_count, |run| {
        for row in run.start..run.end {
            seen.insert(u64::from(codes[row as usize]) >> shift);
        }
    });
    seen.len() as u64
}

/// Points spread over several clusters plus a scattering, so the occupied-tile count grows well
/// short of ×4 per level — the shape the whole change exists for.
fn clustered_points(n: usize, seed: u64) -> Vec<(f32, f32, u64)> {
    let mut rng = StdRng::seed_from_u64(seed);
    let centres = [(120.0f32, 200.0f32), (700.0, 640.0), (900.0, 100.0)];
    (0..n)
        .map(|i| {
            let id = rng.gen();
            if i % 8 == 0 {
                (
                    rng.gen_range(0.0f32..1024.0),
                    rng.gen_range(0.0f32..1024.0),
                    id,
                )
            } else {
                let (cx, cy) = centres[i % centres.len()];
                (
                    (cx + rng.gen_range(-12.0f32..12.0)).clamp(0.0, 1023.9),
                    (cy + rng.gen_range(-12.0f32..12.0)).clamp(0.0, 1023.9),
                    id,
                )
            }
        })
        .collect()
}

/// Every depth-`depth` tile a linear scan finds holding a visible row, across every segment.
///
/// **The independent statement of `N_occ`'s input.** It shares no code with the gallop walk: it
/// reads every row of every segment, asks the mask, and recomputes the tile by a shift. Both
/// routes below are pinned against it — the counted one by taking `len`, the estimated one by
/// putting the same set through the same sketch.
fn tiles_over_segments(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
) -> FxHashSet<u64> {
    let shift = 32 - 2 * u32::from(depth);
    let mut seen: FxHashSet<u64> = FxHashSet::default();
    for (seg, row_base) in segments {
        let base = *row_base;
        let codes = seg.morton.u32();
        mask.for_each_visible_run(base..base + seg.row_count, |run| {
            for row in run.start..run.end {
                seen.insert(u64::from(codes[(row - base) as usize]) >> shift);
            }
        });
    }
    seen
}

/// The ladder the **estimated** route must produce: the linear scan's tile set at each depth, fed
/// to a fresh sketch, clamped to `4^d`, running maximum applied.
///
/// **This is what the multi-segment `occupied_tiles` must equal exactly.** The sketch is a
/// deterministic function of the tile set, so factoring it out of both sides leaves the gallop
/// walk pinned against a linear scan as tightly as an exact comparison would — a walk that emitted
/// one tile too many, too few, or the wrong one gives a different register array and a different
/// number. What the sketch removes from this comparison is nothing: it is on both sides.
fn sketched_ladder_oracle(
    mask: &EffectiveMask,
    segments: &[(&SegmentData, u32)],
    depth: u8,
) -> u64 {
    let mut running = 0u64;
    for d in 0..=depth {
        let mut sketch = TileSketch::new();
        for tile in tiles_over_segments(mask, segments, d) {
            sketch.add(tile);
        }
        running = running.max(sketch.estimate().min(1u64 << (2 * u32::from(d))));
    }
    running
}

/// **`N_occ(d)` is exact at one segment, non-decreasing in depth, and inside `min(4^d, |mask|)`.**
///
/// One segment is the **counted** route (`occupancy::occupied_tiles_ladder`'s predicate): the walk
/// emits each tile once and ascending, so a counter is the accumulator and the answer is a count
/// rather than an estimate. `assert_eq` against the linear scan is therefore the right assertion
/// here and a tolerance would be a weaker one.
///
/// Exactness against the oracle is what the gallop buys: crediting the tile indices a run *spans*
/// instead — the endpoint arithmetic — over-counts wherever a run's codes are not dense in
/// tile-index space, which on a clustered corpus is everywhere.
///
/// Monotonicity is the property §7.2's nesting proof needs. On this route the grid gives it —
/// every occupied tile has an occupied child, and children of distinct parents are distinct — and
/// the ladder's running maximum, which the estimated route needs, never binds. It is asserted
/// rather than argued because nothing else in the tree would notice it failing.
#[test]
fn the_occupied_tile_count_is_exact_monotone_and_bounded() {
    let points = clustered_points(4_000, 0xA11CE);
    let seg = segment_of(&points);
    let n = seg.row_count();

    for (label, visible) in [
        ("full", (0..n).collect::<Vec<u32>>()),
        ("two thirds", (0..n).filter(|r| r % 3 != 0).collect()),
        ("sparse", (0..n).filter(|r| r % 37 == 5).collect()),
        ("one row", vec![n / 2]),
        ("empty", Vec::new()),
    ] {
        let (_t, mask) = mask_over(&visible, n);
        let cardinality = visible.len() as u64;
        let mut previous = 0u64;
        for depth in 0..=16u8 {
            let got = occupied_tiles(&mask, &[(&seg.data, 0)], depth);
            assert_eq!(
                got,
                occupied_tiles_oracle(&mask, &seg.data, depth),
                "{label}, depth {depth}: the gallop walk disagrees with the linear scan"
            );
            assert!(
                got >= previous,
                "{label}, depth {depth}: N_occ fell from {previous} to {got}"
            );
            let tiles_at_depth = 1u64 << (2 * u32::from(depth));
            assert!(
                got <= tiles_at_depth.min(cardinality),
                "{label}, depth {depth}: N_occ {got} exceeds min(4^d, |mask|)"
            );
            previous = got;
        }
    }
}

/// The same, over masks whose overlay diffs are **not** empty — the route
/// [`EffectiveMask::for_each_visible_run`] takes when it cannot walk `base` in place, and the one
/// the chunked walk exists to keep bounded.
#[test]
fn the_occupied_tile_count_is_exact_with_a_composed_mask() {
    let points = clustered_points(3_000, 0xB0B);
    let seg = segment_of(&points);
    let n = seg.row_count();
    let visible: Vec<u32> = (0..n).filter(|r| r % 3 != 0).collect();

    let mut overlay = Overlay::new();
    for &row in visible.iter().step_by(7) {
        overlay.apply(EntityId::new(u64::from(row)), ChangeOp::Suppress);
    }
    let mut buffer = IngestBuffer::default();
    let vis: FxHashSet<u32> = visible.iter().copied().collect();
    for row in (0..n).filter(|r| !vis.contains(r)).step_by(5) {
        buffer.insert_row_with_terms(
            &tessera_lifecycle::WalRow {
                external_id: None,
                entity_id: EntityId::new(u64::from(row)),
                view: "s0".to_string(),
                join: false,
                descriptors: Vec::new(),
                x: 0.0,
                y: 0.0,
                scalars: Vec::new(),
                scoped: Vec::new(),
            },
            vec![TermId::new(0)],
        );
    }
    let (_t, mask) = mask_over_with(&visible, n, &overlay, &buffer);
    assert!(
        !mask.diffs_are_empty(),
        "this test is about the diffs-present route and the mask has no diffs"
    );

    for depth in 0..=16u8 {
        assert_eq!(
            occupied_tiles(&mask, &[(&seg.data, 0)], depth),
            occupied_tiles_oracle(&mask, &seg.data, depth),
            "depth {depth}: the gallop walk disagrees with the linear scan under a composed mask"
        );
    }
}

/// **Above one segment `N_occ` is the sketch of the tile set a linear scan finds** — and three
/// separate things are asserted, because they fail for different reasons.
///
/// **The tile set is exact**, against [`sketched_ladder_oracle`] — the linear scan with the same
/// sketch on top — and it is asserted with `assert_eq`, not a tolerance. This is what the gallop
/// buys, and it is the assertion the endpoint arithmetic would break.
///
/// **The estimate is close to the count**, against [`tiles_over_segments`]'s own cardinality,
/// within five standard errors of a 2¹⁴-register sketch and a floor of two for the cases small
/// enough to round. It is the only tolerance here, and it is what says the estimator is estimating
/// rather than answering nonsense.
///
/// **Monotone and bounded.** The running maximum is what carries §7.2's nesting property through
/// an estimate, and `N_occ(d) <= 4^d` is exact from the ladder's clamp. `N_occ(d) <= |mask|` is
/// **not** clamped and holds only to within the sketch's error, so the bound below carries the same
/// slack: a clamp on the visible total was available and was not taken, so the answer stays a
/// function of the tile set alone.
#[test]
fn the_sketch_route_estimates_the_tile_set_a_linear_scan_finds() {
    let points = clustered_points(4_000, 0x5EED);
    let left: Vec<(f32, f32, u64)> = points.iter().copied().filter(|p| p.2 % 3 != 0).collect();
    let right: Vec<(f32, f32, u64)> = points.iter().copied().filter(|p| p.2 % 3 == 0).collect();
    let seg_a = segment_of(&left);
    let seg_b = segment_of(&right);
    let n = seg_a.row_count() + seg_b.row_count();
    let segments: Vec<(&SegmentData, u32)> =
        vec![(&seg_a.data, 0), (&seg_b.data, seg_a.row_count())];

    for (label, visible) in [
        ("full", (0..n).collect::<Vec<u32>>()),
        ("two thirds", (0..n).filter(|r| r % 3 != 0).collect()),
        ("sparse", (0..n).filter(|r| r % 37 == 5).collect()),
        ("empty", Vec::new()),
    ] {
        let (_t, mask) = mask_over(&visible, n);
        let cardinality = visible.len() as u64;
        let mut previous = 0u64;
        for depth in 0..=16u8 {
            let got = occupied_tiles(&mask, &segments, depth);
            assert_eq!(
                got,
                sketched_ladder_oracle(&mask, &segments, depth),
                "{label}, depth {depth}: the gallop walk emitted a different tile set from the \
                 linear scan"
            );

            let exact = tiles_over_segments(&mask, &segments, depth).len() as u64;
            let slack = (exact as f64 * 0.0406).max(2.0);
            assert!(
                (got as f64 - exact as f64).abs() <= slack,
                "{label}, depth {depth}: estimated {got} against {exact} occupied tiles, \
                 tolerance ±{slack:.1}"
            );

            assert!(
                got >= previous,
                "{label}, depth {depth}: N_occ fell from {previous} to {got}"
            );
            assert!(
                got <= 1u64 << (2 * u32::from(depth)),
                "{label}, depth {depth}: N_occ {got} exceeds the 4^d tiles the grid has"
            );
            assert!(
                (got as f64) <= cardinality as f64 + slack,
                "{label}, depth {depth}: N_occ {got} exceeds |mask| = {cardinality} by more than \
                 the sketch's error"
            );
            previous = got;
        }
    }
}

/// **A tile is counted once however many segments hold rows in it.**
///
/// The same points are written as one segment and as two, with the second segment's rows based
/// after the first's. `N_occ` counts tiles rather than per-segment shares of them, so the two
/// layouts must agree at every depth — summing the segments' own counts would double every tile
/// the split runs through.
///
/// **Both sides take the estimated route**, at the engine's own precision, because the two layouts
/// otherwise differ by the route as well as by the split and the comparison would say nothing. It
/// is the sketch's idempotence that makes the property hold here: adding a tile twice is adding it
/// once, which is precisely what an exact accumulator needs a union, a sort or a bitset for.
#[test]
fn a_tile_spanning_two_segments_is_counted_once() {
    let points = clustered_points(1_200, 0xC0FFEE);
    let whole = segment_of(&points);
    let n = whole.row_count();

    // The split is by geometry rather than by row index, so both segments hold rows in the same
    // tiles at every depth above the one that separates the clusters.
    let left: Vec<(f32, f32, u64)> = points.iter().copied().filter(|p| p.2 % 2 == 0).collect();
    let right: Vec<(f32, f32, u64)> = points.iter().copied().filter(|p| p.2 % 2 == 1).collect();
    let seg_a = segment_of(&left);
    let seg_b = segment_of(&right);
    let split_rows = seg_a.row_count() + seg_b.row_count();
    assert_eq!(split_rows, n, "the split must not lose a row");

    let (_t1, mask_whole) = mask_over(&(0..n).collect::<Vec<u32>>(), n);
    let (_t2, mask_split) = mask_over(&(0..split_rows).collect::<Vec<u32>>(), split_rows);

    for depth in 0..=16u8 {
        let one = occupied_tiles_ladder_with_precision(
            &mask_whole,
            &[(&whole.data, 0)],
            depth,
            SKETCH_PRECISION,
        )
        .at(depth);
        let two = occupied_tiles_ladder_with_precision(
            &mask_split,
            &[(&seg_a.data, 0), (&seg_b.data, seg_a.row_count())],
            depth,
            SKETCH_PRECISION,
        )
        .at(depth);
        assert_eq!(
            one, two,
            "depth {depth}: one segment estimated {one} tiles, two segments estimated {two}"
        );
    }
}

/// **Occupied tiles grow far short of the ×4 the old anchor assumed.**
///
/// The change replaces `4^d` with this count, so a fixture where the two coincide would let the
/// difference go untested. Pinned as a range rather than a figure: what matters is that the
/// fixture is clustered enough for the two anchors to disagree.
#[test]
fn a_clustered_fixture_grows_well_short_of_four_per_level() {
    let points = clustered_points(4_000, 0xD00D);
    let seg = segment_of(&points);
    let n = seg.row_count();
    let (_t, mask) = mask_over(&(0..n).collect::<Vec<u32>>(), n);

    let counts: Vec<u64> = (0..=12u8)
        .map(|d| occupied_tiles(&mask, &[(&seg.data, 0)], d))
        .collect();
    let deep = counts[12];
    assert!(
        deep < (1u64 << 24) / 100,
        "N_occ(12) is {deep}, which is within a hundredth of the 4^12 the old anchor assumed — \
         this fixture is too evenly spread to tell the two apart"
    );
    // Depth 0 is one tile whatever the data does, which is where the two anchors coincide.
    assert_eq!(counts[0], 1);
}
