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
use tessera_engine::select::{SelectParams, Selection, Threshold};
use tessera_lifecycle::{ChangeOp, IngestBuffer, Overlay};
use tessera_spatial::{morton_of, tiler::sort_batch, Extent, Tile, TilerItem};
use tessera_store::read::{ColumnsRef, MortonSlice, SegmentData};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::{tile_ranges, Permutation};
use tessera_types::{EntityId, TermId, TesseraId};

const EXTENT: Extent = Extent {
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
            x,
            y,
            scalars: Vec::new(),
        })
        .collect();
    // `sort_batch` permutes a companion entity-id vector alongside the items. This fixture does not
    // use it: visibility is expressed directly in row space (see the module doc), so the companion
    // is a placeholder and its post-sort contents are deliberately ignored.
    let mut entity_ids: Vec<EntityId> = (0..items.len() as u64).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids, &EXTENT);
    write_segment(temp.path(), &items, &codes, &[]).unwrap();

    let data = SegmentData {
        seg_id: "seg0".to_string(),
        row_count: items.len() as u32,
        morton: MortonSlice::load(&temp.path().join("morton.u32")).unwrap(),
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
    mask_over_with(visible_rows, row_count, &Overlay::default())
}

/// [`mask_over`], but composed against `overlay` — how the run-decode tests obtain masks with
/// non-empty diffs (suppressions land in `minus`, predicate-widens onto rows outside
/// `visible_rows` land in `plus`; entity id == row index, so the overlay names rows directly).
fn mask_over_with(
    visible_rows: &[u32],
    row_count: u32,
    overlay: &Overlay,
) -> (TempDir, EffectiveMask) {
    let temp = TempDir::new().unwrap();
    let bound = row_count as u64;

    let postings_path = temp.path().join("postings.arrow");
    write_postings(&postings_path, &[visible_rows.to_vec()], 32).unwrap();
    let postings = PostingsReader::open(&postings_path, false).unwrap();

    let cache = FragmentCache::new(&temp.path().join("cache"), [1u8; 32], [2u8; 32]);
    let fragment = cache
        .get_or_build(&[TermId::new(0)], [3u8; 32], &postings, bound)
        .unwrap();

    let perm_path = temp.path().join("permutation.bin");
    let identity: Vec<EntityId> = (0..bound).map(EntityId::new).collect();
    write_permutation(&perm_path, &identity, bound).unwrap();
    let perm = Permutation::load(&perm_path).unwrap();

    let base = Arc::new(RowProjection::new(&fragment, &perm));
    let satisfied: FxHashSet<TermId> = [TermId::new(0)].into_iter().collect();
    let mask = compose(
        &fragment,
        &satisfied,
        overlay,
        &IngestBuffer::default(),
        base,
        &perm,
    );
    (temp, mask)
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
    Selection::of(mask, &seg.data, range, p, visible)
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

    let anchor = Threshold::anchor(visible.len() as u64, 8);
    let cap = 12;
    // Counted and asserted below: without it this test can pass vacuously if the fixture, the mask
    // or the parameters ever drift such that no mark is drawn in a tile that has a populated child.
    let mut checked = 0usize;

    for depth in 0..6u8 {
        let parent_params = params(2, cap, anchor.at_depth(depth));
        let child_params = params(2, cap, anchor.at_depth(depth + 1));

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
                let (x, y, _) = points_of(&seg, row);
                child_of.insert(seg.id_at(row), tile_of(x, y, depth + 1).prefix);
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

/// Row `row`'s `(x, y, tessera_id)` as stored — the inverse of `segment_of`'s input, read back from
/// the segment so a test never assumes the pre-sort order.
fn points_of(seg: &Segment, row: u32) -> (f32, f32, u64) {
    let cols = &seg.data.columns;
    let i = row as usize;
    (cols.x()[i], cols.y()[i], cols.tessera_id()[i])
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
// The fast path (Task 2)
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
        Threshold::anchor(visible.len() as u64, 4).at_depth(3),
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
                        let got = Selection::of(&mask, &seg.data, range.clone(), &p, vis);
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

    let got = Selection::of(&mask, &seg.data, range, &p, vis);
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
        let got = Selection::of(&mask, &seg.data, range.clone(), &p, vis);
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

    let anchor = Threshold::anchor(visible.len() as u64, 8);
    for depth in 0..5u8 {
        let p = params(2, 16, anchor.at_depth(depth));
        for (x, y, _) in points.iter().copied() {
            let tile = tile_of(x, y, depth);
            let range = tile_ranges(&seg.data, &tile);
            let vis = mask.count_range(range.clone());
            if vis == 0 {
                continue;
            }
            for row in Selection::of(&mask, &seg.data, range, &p, vis).rows {
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

/// **The B9 equivalence property.** `Selection::of`'s run decode is bit-identical — same `rows`,
/// same `rows_visited` — to the retired per-value path, over randomised masks, ranges and
/// parameters, on **both** decode routes and **both** internal branches.
///
/// The diffs-empty arm is the one that exercises the new cursor route; a corpus that always has
/// non-empty diffs tests only the fallback, which shares its bitmap with the oracle and proves
/// nothing. The route predicate is `diffs_are_empty` (that is the branch `for_each_visible_run`
/// takes), so asserting it per mask, plus the fired-counter floors below, pins that all four
/// route × branch combinations were genuinely reached — mirroring the per-branch coverage
/// asserts in [`selection_matches_the_definition_over_both_internal_branches`].
#[test]
fn run_decode_matches_the_per_value_path_on_both_routes_and_both_branches() {
    use std::collections::HashSet;

    let mut rng = StdRng::seed_from_u64(0xB9_0001);
    let points: Vec<(f32, f32, u64)> = (0..3000)
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

    // Three visibility shapes: sparse scatter, dense scatter, contiguous blocks — the last is the
    // long-run shape the run decode exists for.
    let shapes: Vec<Vec<u32>> = vec![
        (0..n).filter(|_| rng.gen_bool(0.07)).collect(),
        (0..n).filter(|_| rng.gen_bool(0.7)).collect(),
        (0..n).filter(|r| (r / 256) % 2 == 0).collect(),
    ];

    // fired[route][branch]: route 0 = diffs empty (cursor walk), 1 = diffs present (fallback);
    // branch 0 = serve-all, 1 = counting/heap.
    let mut fired = [[0usize; 2]; 2];

    for visible_rows in &shapes {
        for diffs_present in [false, true] {
            let mut overlay = Overlay::new();
            if diffs_present {
                // `minus ⊆ base`: suppress a sample of visible rows. `plus ∩ base = ∅`: widen a
                // sample of invisible rows via a predicate onto the granted term (entity id ==
                // row index in this fixture).
                for &row in visible_rows.iter().step_by(7) {
                    overlay.apply(EntityId::new(row as u64), ChangeOp::Suppress, None);
                }
                let vis_set: HashSet<u32> = visible_rows.iter().copied().collect();
                for row in (0..n).filter(|r| !vis_set.contains(r)).step_by(11) {
                    overlay.apply(
                        EntityId::new(row as u64),
                        ChangeOp::Predicate,
                        Some(vec![TermId::new(0)]),
                    );
                }
            }
            let (_t, mask) = mask_over_with(visible_rows, n, &overlay);
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
                        for i in 0..10 {
                            // Alternate wide and narrow ranges: wide ones exercise the
                            // counting/heap branch and multi-run decodes, narrow ones give the
                            // low visible counts the serve-all branch fires on.
                            let range = if i % 2 == 0 {
                                let a = rng.gen_range(0..n);
                                let b = rng.gen_range(0..n);
                                a.min(b)..a.max(b) + 1
                            } else {
                                let a = rng.gen_range(0..n);
                                a..(a + rng.gen_range(1..64)).min(n)
                            };
                            let vis = mask.count_range(range.clone());
                            if vis == 0 {
                                continue;
                            }
                            let got = Selection::of(&mask, &seg.data, range.clone(), &p, vis);
                            let (want_rows, want_visited) =
                                per_value_selection(&seg, &mask, range.clone(), &p, vis);
                            assert_eq!(
                                got.rows, want_rows,
                                "run decode diverged from the per-value path: diffs_present=\
                                 {diffs_present} k_min={k_min} cap={cap} threshold={threshold:?} \
                                 range={range:?} visible={vis}"
                            );
                            // R3: the counter, un-gated. Both sides count clamped rows actually
                            // read, so both must equal the visible cardinality of the range.
                            assert_eq!(
                                got.rows_visited, want_visited,
                                "rows_visited diverged at diffs_present={diffs_present} \
                                 cap={cap} threshold={threshold:?} range={range:?}"
                            );
                            assert_eq!(
                                want_visited, vis,
                                "the per-value oracle itself must read exactly the visible \
                                 cardinality — if this fails the fixture is broken, not the \
                                 run decode"
                            );

                            // The exact internal branch predicate, transcribed.
                            let floor = p.k_min.min(p.cap);
                            let serves_all = vis <= floor as u64
                                || (p.threshold.is_saturated() && vis <= p.cap as u64);
                            fired[usize::from(diffs_present)][usize::from(!serves_all)] += 1;
                        }
                    }
                }
            }
        }
    }

    for (route, route_name) in [(0, "diffs-empty cursor"), (1, "diffs-present fallback")] {
        for (branch, branch_name) in [(0, "serve-all"), (1, "counting/heap")] {
            assert!(
                fired[route][branch] > 100,
                "only {} comparisons hit the {route_name} route's {branch_name} branch — that \
                 combination is barely covered and this test's silence proves nothing for it",
                fired[route][branch]
            );
        }
    }
}
