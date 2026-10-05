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
use tessera_types::layer::ServingLayout;

use crate::artifacts::MembershipRows;
use crate::derived::{place, Placement};
use crate::row_column::RowColumn;

use super::cache::{FiguresCache, Tail};
use super::counts::{CountsAt, Deltas, Dense};
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
    counts: Arc<CountsAt>,
    column: &Arc<RowColumn>,
    mask: &Mask,
    labels: Option<&DeniedLabels>,
    positions: &Arc<Positions>,
    cache: &Arc<FiguresCache>,
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
            box_of: Arc::new(move |rows: &Bitmap| super::box_of_rows(rows, &held.places())),
            cache: Arc::clone(cache),
        }),
    }
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
            let cache = Arc::new(FiguresCache::default());
            let what = format!("{layout:?} seed {seed} deny {deny}");

            let walked = column
                .accumulate_below(&mask.projection, BASE, Some(&places), true, &|| true)
                .expect("an unstopped walk answers");
            let counts = Arc::new(CountsAt::of(1, Dense::of(walked)));
            let labels = DeniedLabels::read([3; 32], &column, 1, &base_denied(&mask), &places);
            let shared = Arc::new(column.clone());
            let figures = assembled(
                Arc::clone(&counts),
                &shared,
                &mask,
                Some(&labels),
                &positions,
                &cache,
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
                    Brought::Ready(brought) => brought,
                    _ => panic!("{what}: the labels follow the column's own step"),
                };
            assert_eq!(
                brought.labels,
                DeniedLabels::read([3; 32], &column, 2, &base_denied(&mask), &places).labels,
                "{what}: labels brought forward by a step equal labels read afresh"
            );
            let shared = Arc::new(column.clone());
            let figures = assembled(followed, &shared, &mask, Some(&brought), &positions, &cache);
            assert_figures_match(
                &figures,
                &walk(&column, &mask.visible(), &places),
                &format!("{what}, after a growth"),
            );
            spent += cache.stats().reserve_spent;
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
    let walked = column
        .accumulate_below(&projection, BASE, Some(&places), true, &|| true)
        .unwrap();
    let counts = Arc::new(CountsAt::of(1, Dense::of(walked)));
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
        let cache = Arc::new(FiguresCache::default());
        let labels = DeniedLabels::read([3; 32], &column, 1, &base_denied(&mask), &places);
        let figures = assembled(
            Arc::clone(&counts),
            &shared,
            &mask,
            Some(&labels),
            &positions,
            &cache,
        );
        let oracle = walk(&column, &mask.visible(), &places);
        assert_eq!(
            figures.bbox(0),
            oracle[0].3,
            "{denied_edges} rows denied on the low x edge"
        );
        assert_eq!(
            cache.stats().reserve_spent > 0,
            denied_edges >= crate::row_column::RESERVE,
            "{denied_edges} rows denied: the reserve of {} answers fewer, and only those",
            crate::row_column::RESERVE
        );
    }
}

/// **A step that does not start where the labels stand is no way forward**, and neither is a
/// column the labels were not read from: the labels are read again.
#[test]
fn labels_follow_only_their_own_columns_steps() {
    let mut rng = StdRng::seed_from_u64(5);
    let positions = Positions::random(&mut rng);
    let places = positions.places();
    let mut column = column(&mut rng, ServingLayout::RowMajorList);
    let denied = sample(&mut rng, 0..BASE, 0.1);
    let labels = DeniedLabels::read([3; 32], &column, 4, &denied, &places);
    column.record_step(5, 6, &[(1, 2)]);
    assert!(matches!(
        labels.brought([3; 32], &column, 6, &denied, &places, u64::MAX),
        Brought::Broken
    ));
    let other = column.clone().renewed();
    assert!(matches!(
        labels.brought([3; 32], &other, 4, &denied, &places, u64::MAX),
        Brought::Broken
    ));
    let more = sample(&mut rng, 0..BASE, 0.3);
    assert!(matches!(
        labels.brought([3; 32], &column, 4, &more, &places, 10),
        Brought::TooMany
    ));
}

/// **What is written is what is read back**, and a file altered by one byte is not read.
#[test]
fn counts_and_labels_written_to_disk_read_back_whole_or_not_at_all() {
    let mut rng = StdRng::seed_from_u64(11);
    let positions = Positions::random(&mut rng);
    let places = positions.places();
    let column = column(&mut rng, ServingLayout::RowMajorList);
    let projection = sample(&mut rng, 0..ROWS, 0.5);
    let walked = column
        .accumulate_below(&projection, BASE, Some(&places), true, &|| true)
        .unwrap();
    let counts = CountsAt::of(7, Dense::of(walked));
    let dir = tempfile::tempdir().unwrap();
    let stem = super::persist::counts_stem(&[1; 32], "s0", "layer", 0, Geometry::Box);
    super::persist::write_counts(dir.path(), &stem, 7, &counts.folded());
    let read =
        super::persist::read_counts(dir.path(), &stem, 7, Geometry::Box).expect("it reads back");
    assert_eq!(read.counts, counts.dense.counts);
    assert_eq!(read.sums, counts.dense.sums);
    assert_eq!(read.boxes, counts.dense.boxes);
    assert_eq!(read.reserves, counts.dense.reserves);
    assert!(super::persist::read_counts(dir.path(), &stem, 8, Geometry::Box).is_none());
    assert!(super::persist::read_counts(dir.path(), &stem, 7, Geometry::Centroid).is_none());

    let denied = sample(&mut rng, 0..BASE, 0.1);
    let labels = DeniedLabels::read([3; 32], &column, 7, &denied, &places);
    let stem = super::persist::denied_stem("s0", "layer", 0);
    super::persist::write_denied(dir.path(), &stem, &labels);
    let read = super::persist::read_denied(dir.path(), &stem, 7, [3; 32], column.identity())
        .expect("it reads back");
    assert_eq!(read.labels, labels.labels);
    assert_eq!(read.rows, labels.rows);

    let path = dir.path().join(format!("{stem}-7.denied"));
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(
        super::persist::read_denied(dir.path(), &stem, 7, [3; 32], column.identity()).is_none()
    );
}
