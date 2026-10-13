//! The figures against a walk of every visible row: the oracle the rule is checked by.
//!
//! Each case composes a mask the way [`crate::compose::compose`] does — a projection `P`, a
//! `minus ⊆ P` holding denied and failing rows below and above the base, a `plus` outside `P` —
//! over a column with rows above its base, takes the fragment's counts, follows a growth, and
//! assembles the figures from the three parts. The oracle walks `(P − minus) ∪ plus` row by row,
//! reads each row's labels and position, and adds them up. The two must agree on every count,
//! centroid and box.

use std::sync::Arc;

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use mosaica_types::layer::ServingLayout;
use mosaica_types::LayerSpace;

use crate::artifacts::MembershipRows;
use crate::derived::{place, Placement};
use crate::row_column::RowColumn;

use super::cache::{Counters, Tail};
use super::counts::{CountsAt, Deltas, Dense, Reserves};
use super::denied::{Brought, DeniedLabels, DenyCorrection};
use super::{BoxSource, Figures, Geometry};

const BASE: u32 = 3_000;
const ROWS: u32 = 3_600;
const ORDINALS: u32 = 40;

fn scratch() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("a scratch directory"))
        .path()
}

/// Two segments' positions, the base and one extent, each leaving its last rows unplaced.
struct Positions(Vec<(u32, Vec<u32>, Vec<u32>)>);

impl Positions {
    fn random(rng: &mut StdRng) -> Self {
        let mut column = |n: u32| (0..n).map(|_| rng.gen::<u32>()).collect::<Vec<u32>>();
        let base = (0, column(BASE - 40), column(BASE - 40));
        let extent = (BASE, column(ROWS - BASE - 30), column(ROWS - BASE - 30));
        Positions(vec![base, extent])
    }

    fn places(&self) -> Vec<Placement<'_>> {
        let raw: Vec<(u32, &[u32], &[u32])> = self
            .0
            .iter()
            .map(|(base, m, r)| (*base, m.as_slice(), r.as_slice()))
            .collect();
        Placement::of_columns(&raw)
    }
}

/// Every artifact's members over the base and the extent: the label form gives each row at most
/// one, the list form lets a row carry several.
fn column(rng: &mut StdRng, layout: ServingLayout) -> RowColumn {
    let mut sets = vec![Bitmap::new(); ORDINALS as usize];
    for row in 0..ROWS {
        // A few artifacts are huge and spread, so a deny can spend a side's reserve.
        let ordinal = match rng.gen_range(0..10) {
            0..=2 => rng.gen_range(0..4),
            3 => continue,
            _ => rng.gen_range(0..ORDINALS),
        };
        sets[ordinal as usize].add(row);
        if layout == ServingLayout::RowMajorList && rng.gen_bool(0.2) {
            sets[rng.gen_range(0..ORDINALS) as usize].add(row);
        }
    }
    let membership = MembershipRows::of_rows(sets.into_iter().map(Some).collect());
    RowColumn::compose_over_base(&membership, BASE, ROWS, layout, scratch())
        .expect("the memberships fit the layout")
}

fn sample(rng: &mut StdRng, rows: impl Iterator<Item = u32>, p: f64) -> Bitmap {
    rows.filter(|_| rng.gen_bool(p)).collect()
}

/// Per ordinal: count, placed, sums and box, over `rows`.
type Oracle = Vec<(u64, u64, [u64; 2], Option<[u32; 4]>)>;

fn walk(column: &RowColumn, rows: &Bitmap, places: &[Placement<'_>]) -> Oracle {
    let mut out: Oracle = vec![(0, 0, [0; 2], None); column.len()];
    for row in rows.iter() {
        let position = place(places, row);
        column.for_each_label(row, |ordinal| {
            let entry = &mut out[ordinal as usize];
            entry.0 += 1;
            if let Some((x, y)) = position {
                entry.1 += 1;
                entry.2[0] += u64::from(x);
                entry.2[1] += u64::from(y);
                let b = entry.3.get_or_insert([u32::MAX, u32::MAX, 0, 0]);
                super::counts::widen(b, x, y);
            }
        });
    }
    out
}

fn assert_figures_match(figures: &Figures, oracle: &Oracle, what: &str) {
    for (ordinal, (count, placed, sums, bbox)) in oracle.iter().enumerate() {
        let o = ordinal as u32;
        assert_eq!(figures.get(o), *count, "{what}: ordinal {o}'s count");
        let centroid = (*placed > 0).then(|| {
            [
                sums[0] as f64 / *placed as f64,
                sums[1] as f64 / *placed as f64,
            ]
        });
        assert_eq!(
            figures.centroid(o),
            centroid,
            "{what}: ordinal {o}'s centroid"
        );
        assert_eq!(figures.bbox(o), *bbox, "{what}: ordinal {o}'s box");
    }
    let populated: Vec<u32> = figures.populated().iter().collect();
    let expected: Vec<u32> = oracle
        .iter()
        .enumerate()
        .filter(|(_, (count, ..))| *count > 0)
        .map(|(o, _)| o as u32)
        .collect();
    assert_eq!(populated, expected, "{what}: the populated ordinals");
}

/// One composed mask: the projection, what composition subtracts and adds, and the denied rows.
struct Mask {
    projection: Bitmap,
    minus: Bitmap,
    plus: Bitmap,
    denied: Bitmap,
}

impl Mask {
    fn visible(&self) -> Bitmap {
        let mut visible = self.projection.andnot(&self.minus);
        visible.or_inplace(&self.plus);
        visible
    }
}

fn mask(rng: &mut StdRng, deny: f64) -> Mask {
    let projection = sample(rng, 0..ROWS, 0.6);
    // Denied rows anywhere, in the projection or not, as the view's deny mask holds them.
    let denied = sample(rng, 0..ROWS, deny);
    // Rows the viewer fails, which only a buffered item has, and the projection holds none below
    // the base; they are here to check the arithmetic holds whatever the mask subtracts.
    let failing = sample(rng, projection.iter(), 0.01);
    let mut minus = denied.and(&projection);
    minus.or_inplace(&failing);
    let plus = sample(rng, (0..ROWS).filter(|r| !projection.contains(*r)), 0.05).andnot(&denied);
    Mask {
        projection,
        minus,
        plus,
        denied,
    }
}

/// The figures as the engine assembles them, from `counts` and the mask.
fn assembled(
    (counts, reserves): (Arc<CountsAt>, Option<Arc<Reserves>>),
    column: &Arc<RowColumn>,
    mask: &Mask,
    labels: Option<&DeniedLabels>,
    positions: &Arc<Positions>,
    counters: &Arc<Counters>,
) -> Figures {
    let places = positions.places();
    let mut subtracted = mask.minus.clone();
    subtracted.remove_range(BASE..);
    let deny = (!subtracted.is_empty())
        .then(|| Arc::new(DenyCorrection::of(subtracted, labels, column, &places)));
    let mut above = mask.projection.and(&Bitmap::from_range(BASE..ROWS));
    above.andnot_inplace(&mask.minus);
    above.or_inplace(&mask.plus);
    let mut tail = Deltas::default();
    for row in above.iter() {
        let position = place(&places, row);
        column.for_each_label(row, |o| tail.entry(o).or_default().add(position));
    }
    let held = Arc::clone(positions);
    Figures {
        counts,
        reserves,
        deny,
        tail: Some(Arc::new(Tail(tail))),
        len: column.len(),
        geometry: Geometry::Box,
        exact: Some(BoxSource {
            column: Arc::clone(column),
            projection: Arc::new(crate::projection::RowProjection::from_rows(
                mask.projection.clone(),
            )),
            base_rows: BASE,
            places: Arc::new(move |read: &mut dyn FnMut(&[Placement<'_>])| read(&held.places())),
            counters: Arc::clone(counters),
        }),
    }
}

/// The fragment's counts over `projection`'s base rows at `at`, and their reserves, as a walk
/// takes them.
fn filled(
    column: &RowColumn,
    projection: &Bitmap,
    places: &[Placement<'_>],
    at: u64,
) -> (Arc<CountsAt>, Option<Arc<Reserves>>) {
    let walked = column
        .accumulate_below(projection, BASE, Some(places), true, &|| true)
        .expect("an unstopped walk answers");
    let (dense, reserves) = Dense::of(at, walked);
    (Arc::new(CountsAt::of(dense)), reserves.map(Arc::new))
}

fn base_denied(mask: &Mask) -> Bitmap {
    let mut denied = mask.denied.clone();
    denied.remove_range(BASE..);
    denied
}

/// **F − D + T is the walk of the visible rows**, for every artifact, under either layout, before
/// and after a growth between folds, over masks that deny little and much.
#[test]
fn the_figures_are_a_walk_of_the_visible_rows() {
    let mut spent = 0;
    for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
        for seed in 0..12u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let positions = Arc::new(Positions::random(&mut rng));
            let places = positions.places();
            let mut column = column(&mut rng, layout);
            let deny = [0.0, 0.01, 0.2, 0.6][seed as usize % 4];
            let mask = mask(&mut rng, deny);
            let counters = Arc::new(Counters::default());
            let what = format!("{layout:?} seed {seed} deny {deny}");

            let (counts, reserves) = filled(&column, &mask.projection, &places, 1);
            let labels = DeniedLabels::read([3; 32], &column, 1, &base_denied(&mask), &places);
            let shared = Arc::new(column.clone());
            let figures = assembled(
                (Arc::clone(&counts), reserves.clone()),
                &shared,
                &mask,
                Some(&labels),
                &positions,
                &counters,
            );
            assert_figures_match(&figures, &walk(&column, &mask.visible(), &places), &what);

            // A growth: rows below the base join artifacts, some of them denied rows and some at
            // the map's edges, and the counts follow the column's step rather than walking again.
            let mut grown = Vec::new();
            for row in sample(&mut rng, 0..BASE, 0.05).iter() {
                let ordinal = rng.gen_range(0..ORDINALS);
                let mut held = false;
                column.for_each_label(row, |_| held = true);
                if layout == ServingLayout::RowMajorList || !held {
                    grown.push((row, ordinal));
                }
            }
            let kept = column
                .amend_kept(&grown, ROWS)
                .expect("the growth fits the layout");
            column.record_step(1, 2, &kept);
            let steps = column
                .steps_between(1, 2)
                .expect("one step leads from 1 to 2");
            let followed = Arc::new(counts.followed(
                2,
                &steps,
                |row| row < BASE && mask.projection.contains(row),
                |row| place(&places, row),
                Geometry::Box,
            ));
            let brought =
                match labels.brought([3; 32], &column, 2, &base_denied(&mask), &places, u64::MAX) {
                    (Brought::Ready(brought), _) => brought,
                    _ => DeniedLabels::read([3; 32], &column, 2, &base_denied(&mask), &places),
                };
            let shared = Arc::new(column.clone());
            let figures = assembled(
                (followed, reserves),
                &shared,
                &mask,
                Some(&brought),
                &positions,
                &counters,
            );
            assert_figures_match(
                &figures,
                &walk(&column, &mask.visible(), &places),
                &format!("{what}, after a growth"),
            );
            spent += counters.spent.load(std::sync::atomic::Ordering::Relaxed);
        }
    }
    assert!(
        spent > 0,
        "no deny spent a reserve, so the walk of an artifact's rows went untested"
    );
}

/// **A deny on a box's edge is answered from the reserve**, without walking the artifact's rows,
/// until the reserve is spent; then the box is worked out from the rows, and either way it is the
/// walk's.
#[test]
fn a_denied_edge_takes_the_next_row_from_the_reserve_until_it_is_spent() {
    let mut rng = StdRng::seed_from_u64(99);
    let positions = Arc::new(Positions::random(&mut rng));
    let places = positions.places();
    let column = column(&mut rng, ServingLayout::RowMajorLabel);
    let projection = Bitmap::from_range(0..ROWS);
    let filled = filled(&column, &projection, &places, 1);
    let shared = Arc::new(column.clone());
    // Artifact 0's base rows by `x`, lowest first.
    let mut by_x: Vec<(u32, u32)> = Vec::new();
    for row in 0..BASE {
        let mut ours = false;
        column.for_each_label(row, |o| ours |= o == 0);
        if let (true, Some((x, _))) = (ours, place(&places, row)) {
            by_x.push((x, row));
        }
    }
    by_x.sort_unstable();
    assert!(
        by_x.len() > 20,
        "artifact 0 is large enough to have a reserve to spend"
    );

    for denied_edges in [1usize, 5, 8, 9, 20] {
        let denied: Bitmap = by_x[..denied_edges].iter().map(|(_, row)| *row).collect();
        let mask = Mask {
            minus: denied.clone(),
            projection: projection.clone(),
            plus: Bitmap::new(),
            denied,
        };
        let counters = Arc::new(Counters::default());
        let labels = DeniedLabels::read([3; 32], &column, 1, &base_denied(&mask), &places);
        let figures = assembled(
            filled.clone(),
            &shared,
            &mask,
            Some(&labels),
            &positions,
            &counters,
        );
        let oracle = walk(&column, &mask.visible(), &places);
        assert_eq!(
            figures.bbox(0),
            oracle[0].3,
            "{denied_edges} rows denied on the low x edge"
        );
        assert_eq!(
            counters.spent.load(std::sync::atomic::Ordering::Relaxed) > 0,
            denied_edges >= crate::row_column::RESERVE,
            "{denied_edges} rows denied: the reserve of {} answers fewer, and only those",
            crate::row_column::RESERVE
        );
    }
}

/// **What is written is what is read back**, and a file altered by one byte is not read.
#[test]
fn counts_and_labels_written_to_disk_read_back_whole_or_not_at_all() {
    let mut rng = StdRng::seed_from_u64(11);
    let positions = Positions::random(&mut rng);
    let places = positions.places();
    let column = column(&mut rng, ServingLayout::RowMajorList);
    let projection = sample(&mut rng, 0..ROWS, 0.5);
    let (counts, reserves) = filled(&column, &projection, &places, 6);
    let mut column = column;
    let kept = column
        .amend_kept(&[(3, 1), (BASE - 1, 2)], ROWS)
        .expect("a list column takes any pair");
    column.record_step(6, 7, &kept);
    let steps = column.steps_between(6, 7).unwrap();
    let counts = counts.followed(
        7,
        &steps,
        |_| true,
        |row| place(&places, row),
        Geometry::Box,
    );
    let dir = tempfile::tempdir().unwrap();
    let stem = super::persist::counts_stem(&[1; 32], "s0", LayerSpace::ZERO, 0, Geometry::Box);
    super::persist::write_counts(dir.path(), &stem, &counts, reserves.as_deref());
    let (read, read_reserves) =
        super::persist::read_counts(dir.path(), &stem, 7, Geometry::Box).expect("it reads back");
    assert_eq!(read.dense.counts, counts.dense.counts);
    assert_eq!(read.dense.sums, counts.dense.sums);
    assert_eq!(read.dense.boxes, counts.dense.boxes);
    assert_eq!(read.dense.filled_at, 6);
    assert_eq!(
        read.grown, counts.grown,
        "what the step added is read back beside the walk"
    );
    assert_eq!(read_reserves.as_ref(), reserves.as_deref());
    assert!(super::persist::read_counts(dir.path(), &stem, 8, Geometry::Box).is_none());
    assert!(super::persist::read_counts(dir.path(), &stem, 7, Geometry::None).is_none());

    let denied = sample(&mut rng, 0..BASE, 0.1);
    let labels = DeniedLabels::read([3; 32], &column, 7, &denied, &places);
    let stem = super::persist::denied_stem("s0", LayerSpace::ZERO, 0);
    super::persist::write_denied(dir.path(), &stem, &labels);
    let read = super::persist::read_denied(dir.path(), &stem, 7, [3; 32], column.identity())
        .expect("it reads back");
    assert_eq!(read, labels);

    let path = dir.path().join(format!("{stem}-7.denied"));
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(
        super::persist::read_denied(dir.path(), &stem, 7, [3; 32], column.identity()).is_none()
    );
}

/// What one shard's mask denies.
#[derive(Debug, Clone, Copy)]
enum Denying {
    /// Rows anywhere, at this rate, as [`mask`] denies them.
    Random(f64),
    /// Artifact 0's this many lowest base rows by `x`, so the shard's box edge moves.
    Extremes(usize),
}

/// Artifact 0's `edges` lowest base rows by `x`, denied, over a projection of every row.
fn extremes_denied(column: &RowColumn, places: &[Placement<'_>], edges: usize) -> Mask {
    let mut by_x: Vec<(u32, u32)> = (0..BASE)
        .filter(|&row| {
            let mut ours = false;
            column.for_each_label(row, |o| ours |= o == 0);
            ours
        })
        .filter_map(|row| place(places, row).map(|(x, _)| (x, row)))
        .collect();
    by_x.sort_unstable();
    let denied: Bitmap = by_x[..edges].iter().map(|(_, row)| *row).collect();
    Mask {
        minus: denied.clone(),
        projection: Bitmap::from_range(0..ROWS),
        plus: Bitmap::new(),
        denied,
    }
}

/// One shard's figures and the walk of its visible rows: its own column, positions and mask.
fn shard(rng: &mut StdRng, denying: Denying) -> (Figures, Oracle) {
    let positions = Arc::new(Positions::random(rng));
    let places = positions.places();
    let column = column(rng, ServingLayout::RowMajorLabel);
    let mask = match denying {
        Denying::Random(rate) => mask(rng, rate),
        Denying::Extremes(edges) => extremes_denied(&column, &places, edges),
    };
    let counters = Arc::new(Counters::default());
    let filled = filled(&column, &mask.projection, &places, 1);
    let labels = DeniedLabels::read([3; 32], &column, 1, &base_denied(&mask), &places);
    let shared = Arc::new(column.clone());
    let figures = assembled(filled, &shared, &mask, Some(&labels), &positions, &counters);
    (figures, walk(&column, &mask.visible(), &places))
}

/// Per ordinal: count, placed, sums and box, over every shard.
type Summed = Vec<(u64, u64, [u128; 2], Option<[u32; 4]>)>;

/// The walks of two shards' visible rows as one walk of every visible row of both: what one shard
/// holding both shards' rows would count.
fn combined(a: &Oracle, b: &Oracle) -> Summed {
    a.iter()
        .zip(b)
        .map(|(a, b)| {
            let bbox = match (a.3, b.3) {
                (Some(mut x), Some(y)) => {
                    super::counts::widen(&mut x, y[0], y[1]);
                    super::counts::widen(&mut x, y[2], y[3]);
                    Some(x)
                }
                (x, y) => x.or(y),
            };
            let sums = [
                u128::from(a.2[0]) + u128::from(b.2[0]),
                u128::from(a.2[1]) + u128::from(b.2[1]),
            ];
            (a.0 + b.0, a.1 + b.1, sums, bbox)
        })
        .collect()
}

/// **Two shards' figures combined answer as one shard holding every row of both**: counts and
/// position sums add, and each side of the box is the extreme across shards of each shard's own,
/// over masks denying little and much and with each shard's extreme rows of one artifact denied.
#[test]
fn figures_over_two_shards_are_the_walk_of_every_visible_row_of_both() {
    let cases = [
        Denying::Random(0.0),
        Denying::Random(0.2),
        Denying::Random(0.6),
        Denying::Extremes(1),
        Denying::Extremes(9),
    ];
    for denying in cases {
        for seed in 0..4u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let (a, walk_a) = shard(&mut rng, denying);
            let (b, walk_b) = shard(&mut rng, denying);
            let figures = super::LevelFigures::of(vec![Arc::new(a), Arc::new(b)]);
            let what = format!("{denying:?}, seed {seed}");
            let summed = combined(&walk_a, &walk_b);
            for (ordinal, (count, placed, sums, bbox)) in summed.iter().enumerate() {
                let o = ordinal as u32;
                assert_eq!(figures.get(o), *count, "{what}: ordinal {o}'s count");
                let centroid = (*placed > 0)
                    .then(|| [sums[0] as f64 / *placed as f64, sums[1] as f64 / *placed as f64]);
                assert_eq!(figures.centroid(o), centroid, "{what}: ordinal {o}'s centroid");
                assert_eq!(figures.bbox(o), *bbox, "{what}: ordinal {o}'s box");
            }
            let expected: Vec<u32> = summed
                .iter()
                .enumerate()
                .filter(|(_, (count, ..))| *count > 0)
                .map(|(o, _)| o as u32)
                .collect();
            assert_eq!(figures.populated().iter().collect::<Vec<_>>(), expected, "{what}");
        }
    }
}

/// Position sums over several shards pass what a `u64` holds and are added in `u128`, and so are
/// the shards' filtered counts past a `u32`.
#[test]
fn sums_over_shards_are_widened_before_they_are_added() {
    let near_the_top = |placed: u32| {
        let dense = Dense {
            filled_at: 1,
            counts: vec![placed],
            placed: vec![placed],
            sums: vec![[u64::MAX - 1, u64::MAX - 3]],
            boxes: vec![[0, 0, u32::MAX, u32::MAX]],
        };
        Arc::new(Figures {
            counts: Arc::new(CountsAt::of(dense)),
            reserves: None,
            deny: None,
            tail: None,
            len: 1,
            geometry: Geometry::Centroid,
            exact: None,
        })
    };
    let figures = super::LevelFigures::of(vec![near_the_top(u32::MAX), near_the_top(u32::MAX)]);
    let placed = 2.0 * f64::from(u32::MAX);
    let sums = [2 * u128::from(u64::MAX - 1), 2 * u128::from(u64::MAX - 3)];
    assert_eq!(figures.get(0), 2 * u64::from(u32::MAX));
    assert_eq!(
        figures.centroid(0),
        Some([sums[0] as f64 / placed, sums[1] as f64 / placed])
    );
    assert_eq!(
        super::LevelFigures::summed_counts(&[&[u32::MAX, 1], &[u32::MAX]]),
        vec![2 * u64::from(u32::MAX), 1]
    );
}
