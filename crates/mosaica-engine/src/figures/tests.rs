//! The figures against a walk of every visible row: the oracle the rule is checked by.
//!
//! Each case composes a mask in each of several shards the way [`crate::compose::compose`] does —
//! a projection `P`, a `minus ⊆ P` holding denied and failing rows below and above the base, a
//! `plus` outside `P` — over a column with rows above its base, sums the shards' walks of their
//! base rows, follows a growth, and assembles the figures from the three parts. The oracle walks
//! `(P − minus) ∪ plus` in every shard row by row, reads each row's labels and position, and adds
//! them up. The two must agree on every count, centroid and box. A sum brought forward by one
//! shard's difference must be the sum made again from every shard's walk.

use std::sync::Arc;

use croaring::Bitmap;
use mosaica_types::layer::ServingLayout;
use mosaica_types::{LayerSpace, ShardId};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::artifacts::MembershipRows;
use crate::derived::{place, Placement};
use crate::row_column::RowColumn;

use super::cache::{Counters, Tail};
use super::counts::{CountsAt, Deltas, Dense, Follow, Part, Reserves, SideAt, SideRow};
use super::denied::{Brought, DeniedLabels, DenyCorrection, Subtracted};
use super::sum::{difference, sum_of};
use super::{BoxSource, Geometry, LevelFigures};

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
/// one, the list form lets a row carry several. A few artifacts are huge and spread, and the rest
/// are drawn from `ordinals`: every one of them, or a few a shard holds most of, as a shard of a
/// corpus laid out in space does.
fn column_over(
    rng: &mut StdRng,
    layout: ServingLayout,
    ordinals: std::ops::Range<u32>,
) -> RowColumn {
    let mut sets = vec![Bitmap::new(); ORDINALS as usize];
    for row in 0..ROWS {
        // A few artifacts are huge and spread, so a deny can spend a side's reserve.
        let ordinal = match rng.gen_range(0..10) {
            0..=2 => rng.gen_range(0..4),
            3 => continue,
            _ => rng.gen_range(ordinals.clone()),
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

/// Per ordinal over every shard: count, placed, sums and box.
type Oracle = Vec<(u64, u64, [u128; 2], Option<[u32; 4]>)>;

/// Add a walk of `rows` of `column` to `out`.
fn walk_into(out: &mut Oracle, column: &RowColumn, rows: &Bitmap, places: &[Placement<'_>]) {
    for row in rows.iter() {
        let position = place(places, row);
        column.for_each_label(row, |ordinal| {
            let entry = &mut out[ordinal as usize];
            entry.0 += 1;
            if let Some((x, y)) = position {
                entry.1 += 1;
                entry.2[0] += u128::from(x);
                entry.2[1] += u128::from(y);
                let b = entry.3.get_or_insert([u32::MAX, u32::MAX, 0, 0]);
                super::counts::widen(b, x, y);
            }
        });
    }
}

fn assert_figures_match(figures: &LevelFigures, oracle: &Oracle, what: &str) {
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
#[derive(Clone)]
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

    /// The mask's `minus` below the base.
    fn subtracted(&self) -> Bitmap {
        let mut subtracted = self.minus.clone();
        subtracted.remove_range(BASE..);
        subtracted
    }

    fn base_denied(&self) -> Bitmap {
        let mut denied = self.denied.clone();
        denied.remove_range(BASE..);
        denied
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

/// What one shard's mask denies.
#[derive(Debug, Clone, Copy)]
enum Denying {
    /// Rows anywhere, at this rate, as [`mask`] denies them.
    Random(f64),
    /// Artifact 0's this many lowest base rows by `x`, so the shard's box edge moves.
    Extremes(usize),
}

/// One shard as a case holds it: its number, column, positions and mask.
struct Shard {
    id: ShardId,
    column: RowColumn,
    positions: Arc<Positions>,
    mask: Mask,
}

impl Shard {
    fn random(rng: &mut StdRng, id: u32, layout: ServingLayout, denying: Denying) -> Self {
        Self::over(rng, id, layout, denying, 0..ORDINALS)
    }

    /// [`Self::random`], its smaller artifacts drawn from `ordinals` ([`column_over`]).
    fn over(
        rng: &mut StdRng,
        id: u32,
        layout: ServingLayout,
        denying: Denying,
        ordinals: std::ops::Range<u32>,
    ) -> Self {
        let positions = Arc::new(Positions::random(rng));
        let column = column_over(rng, layout, ordinals);
        let mask = match denying {
            Denying::Random(rate) => mask(rng, rate),
            Denying::Extremes(edges) => extremes_denied(&column, &positions.places(), edges),
        };
        Shard {
            id: ShardId::new(id).unwrap(),
            column,
            positions,
            mask,
        }
    }

    /// The shard's walk of the projection's base rows.
    fn part(&self) -> Part {
        let walked = self
            .column
            .accumulate_below(
                &self.mask.projection,
                BASE,
                Some(&self.positions.places()),
                true,
                &|| true,
            )
            .expect("an unstopped walk answers");
        Part::of(walked, true)
    }

    fn side(&self, at: u64) -> SideAt {
        SideAt {
            shard: self.id,
            identity: [self.id.raw() as u8; 32],
            filled_at: at,
            at,
            grown: Arc::default(),
        }
    }

    fn labels(&self, at: u64) -> DeniedLabels {
        DeniedLabels::read(
            [self.id.raw() as u8; 32],
            &self.column,
            at,
            &self.mask.base_denied(),
            &self.positions.places(),
        )
    }
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

/// Where a row of one of `shards` is.
fn position_of(shards: &[Shard]) -> impl Fn(SideRow) -> Option<(u32, u32)> + '_ {
    move |(id, row)| {
        let shard = shards.iter().find(|shard| shard.id.raw() == id)?;
        place(&shard.positions.places(), row)
    }
}

/// The sum of every shard's walk at version `at`, and its reserve.
fn summed(shards: &[Shard], at: u64) -> (CountsAt, Option<Reserves>) {
    let parts: Vec<Part> = shards.iter().map(Shard::part).collect();
    let every: Vec<(ShardId, &Part)> = shards.iter().map(|s| s.id).zip(parts.iter()).collect();
    let (dense, reserves) = sum_of(&every, Geometry::Box, &position_of(shards));
    let sides = shards.iter().map(|shard| shard.side(at)).collect();
    (CountsAt::of(sides, dense), reserves)
}

/// The figures as the engine assembles them over `shards`, from `counts` and each shard's mask,
/// its denied rows labelled from `labels` where given.
fn assembled(
    shards: &[Shard],
    (counts, reserves): (Arc<CountsAt>, Option<Arc<Reserves>>),
    labels: &[Option<DeniedLabels>],
    counters: &Arc<Counters>,
) -> LevelFigures {
    let places: Vec<Vec<Placement<'_>>> = shards.iter().map(|s| s.positions.places()).collect();
    let subtracted: Vec<Bitmap> = shards.iter().map(|s| s.mask.subtracted()).collect();
    let deny = subtracted.iter().any(|rows| !rows.is_empty()).then(|| {
        Arc::new(DenyCorrection::of(
            shards
                .iter()
                .zip(&subtracted)
                .zip(&places)
                .zip(labels)
                .filter(|(((_, rows), _), _)| !rows.is_empty())
                .map(|(((shard, rows), places), labels)| Subtracted {
                    shard: shard.id,
                    rows: rows.clone(),
                    labels: labels.as_ref(),
                    column: &shard.column,
                    places,
                }),
        ))
    });
    let mut tail = Deltas::default();
    for (shard, places) in shards.iter().zip(&places) {
        let mut above = shard.mask.projection.and(&Bitmap::from_range(BASE..ROWS));
        above.andnot_inplace(&shard.mask.minus);
        above.or_inplace(&shard.mask.plus);
        for row in above.iter() {
            let position = place(places, row);
            shard
                .column
                .for_each_label(row, |o| tail.entry(o).or_default().add(position));
        }
    }
    let sources = shards
        .iter()
        .map(|shard| {
            let held = Arc::clone(&shard.positions);
            BoxSource {
                shard: shard.id,
                column: Arc::new(shard.column.clone()),
                projection: Arc::new(crate::projection::RowProjection::from_rows(
                    shard.mask.projection.clone(),
                )),
                base_rows: BASE,
                places: Arc::new(move |read: &mut dyn FnMut(&[Placement<'_>])| {
                    read(&held.places())
                }),
            }
        })
        .collect();
    LevelFigures {
        counts,
        reserves,
        deny,
        tail: Some(Arc::new(Tail(tail))),
        len: ORDINALS as usize,
        geometry: Geometry::Box,
        sources,
        counters: Some(Arc::clone(counters)),
    }
}

/// The walk of every shard's visible rows.
fn oracle(shards: &[Shard]) -> Oracle {
    let mut out: Oracle = vec![(0, 0, [0; 2], None); ORDINALS as usize];
    for shard in shards {
        walk_into(
            &mut out,
            &shard.column,
            &shard.mask.visible(),
            &shard.positions.places(),
        );
    }
    out
}

/// **F − D + T is the walk of the visible rows**, for every artifact, over one shard and three,
/// under either layout, before and after a growth in one shard, over masks that deny little and
/// much and masks that deny an artifact's extreme rows.
#[test]
fn the_figures_are_a_walk_of_the_visible_rows() {
    let cases = [
        Denying::Random(0.0),
        Denying::Random(0.01),
        Denying::Random(0.2),
        Denying::Random(0.6),
        Denying::Extremes(1),
        Denying::Extremes(9),
    ];
    let mut spent = 0;
    for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
        for (case, denying) in cases.into_iter().enumerate() {
            for shards in [1u32, 3] {
                let mut rng = StdRng::seed_from_u64(case as u64 * 7 + u64::from(shards));
                let mut shards: Vec<Shard> = (0..shards)
                    .map(|id| Shard::random(&mut rng, id, layout, denying))
                    .collect();
                let counters = Arc::new(Counters::default());
                let what = format!("{layout:?} {denying:?} over {} shards", shards.len());

                let (counts, reserves) = summed(&shards, 1);
                let (counts, reserves) = (Arc::new(counts), reserves.map(Arc::new));
                let labels: Vec<Option<DeniedLabels>> =
                    shards.iter().map(|s| Some(s.labels(1))).collect();
                let figures = assembled(
                    &shards,
                    (Arc::clone(&counts), reserves.clone()),
                    &labels,
                    &counters,
                );
                assert_figures_match(&figures, &oracle(&shards), &what);

                // A growth in the first shard: rows below the base join artifacts, some of them
                // denied rows and some at the map's edges, and the sum follows the shard's step
                // rather than walking again.
                let mut grown = Vec::new();
                for row in sample(&mut rng, 0..BASE, 0.05).iter() {
                    let ordinal = rng.gen_range(0..ORDINALS);
                    let mut held = false;
                    shards[0].column.for_each_label(row, |_| held = true);
                    if layout == ServingLayout::RowMajorList || !held {
                        grown.push((row, ordinal));
                    }
                }
                let kept = shards[0]
                    .column
                    .amend_kept(&grown, ROWS)
                    .expect("the growth fits the layout");
                shards[0].column.record_step(1, 2, &kept);
                let first = &shards[0];
                let steps = first
                    .column
                    .steps_between(1, 2)
                    .expect("one step leads from 1 to 2");
                let places = first.positions.places();
                let followed = Arc::new(counts.followed(
                    &[Follow {
                        place: 0,
                        to: 2,
                        steps,
                        counted: &|row| row < BASE && first.mask.projection.contains(row),
                        place_of: &|row| place(&places, row),
                    }],
                    Geometry::Box,
                ));
                let mut labels = labels;
                labels[0] = Some(
                    match labels[0].as_ref().unwrap().brought(
                        [0; 32],
                        &first.column,
                        2,
                        &first.mask.base_denied(),
                        &places,
                        u64::MAX,
                    ) {
                        (Brought::Ready(brought), _) => brought,
                        _ => first.labels(2),
                    },
                );
                let figures = assembled(&shards, (followed, reserves), &labels, &counters);
                assert_figures_match(
                    &figures,
                    &oracle(&shards),
                    &format!("{what}, after a growth"),
                );
                spent += counters.spent.load(std::sync::atomic::Ordering::Relaxed);
            }
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
    let mut shard = Shard::random(&mut rng, 0, ServingLayout::RowMajorLabel, Denying::Random(0.0));
    shard.mask = Mask {
        projection: Bitmap::from_range(0..ROWS),
        minus: Bitmap::new(),
        plus: Bitmap::new(),
        denied: Bitmap::new(),
    };
    let (counts, reserves) = summed(std::slice::from_ref(&shard), 1);
    let filled = (Arc::new(counts), reserves.map(Arc::new));
    let places = shard.positions.places();
    // Artifact 0's base rows by `x`, lowest first.
    let mut by_x: Vec<(u32, u32)> = Vec::new();
    for row in 0..BASE {
        let mut ours = false;
        shard.column.for_each_label(row, |o| ours |= o == 0);
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
        shard.mask.minus = denied.clone();
        shard.mask.denied = denied;
        let counters = Arc::new(Counters::default());
        let shards = std::slice::from_ref(&shard);
        let figures = assembled(shards, filled.clone(), &[Some(shard.labels(1))], &counters);
        assert_eq!(
            figures.bbox(0),
            oracle(shards)[0].3,
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

/// Per ordinal and side, the rows a reserve names.
fn reserve_rows(reserves: Option<&Reserves>) -> Vec<Vec<Option<Vec<SideRow>>>> {
    (0..ORDINALS)
        .map(|o| {
            (0..4)
                .map(|side| reserves.and_then(|r| Some(r.side(o, side)?.collect())))
                .collect()
        })
        .collect()
}

/// **A sum brought forward by one shard's difference is the sum made again** from every shard's
/// walk: counts, placed counts, sums, boxes and reserves alike, whether the shard's walk is
/// replaced by another of the same shard, taken away, or added, whichever artifacts' edges it
/// held, and whether each artifact is spread over every shard or mostly in one.
#[test]
fn a_sum_brought_forward_by_a_shards_difference_is_the_sum_made_again() {
    for seed in 0..32u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let layout = match seed % 2 {
            0 => ServingLayout::RowMajorLabel,
            _ => ServingLayout::RowMajorList,
        };
        // Spread over every shard, or each shard holding most of its own few artifacts.
        let ordinals = |id: u32| match seed % 4 < 2 {
            true => 0..ORDINALS,
            false => 4 + id * 9..(4 + id * 9 + 11).min(ORDINALS),
        };
        let held: Vec<Shard> = (0..3)
            .map(|id| Shard::over(&mut rng, id, layout, Denying::Random(0.0), ordinals(id)))
            .collect();
        let (pred, pred_reserves) = summed(&held, 1);
        let parts: Vec<Part> = held.iter().map(Shard::part).collect();
        // Shard 1 compacted into another walk, taken away, or a shard 3 opened.
        let replacement = Shard::over(&mut rng, 1, layout, Denying::Random(0.0), ordinals(1));
        let opened = Shard::over(&mut rng, 3, layout, Denying::Random(0.0), ordinals(3));
        let cases: [(&str, Vec<usize>, Vec<&Shard>); 3] = [
            ("replaced", vec![1], vec![&replacement]),
            ("taken away", vec![1], vec![]),
            ("added", vec![], vec![&opened]),
        ];
        for (what, old, new) in cases {
            let kept: Vec<&Shard> = held
                .iter()
                .enumerate()
                .filter(|(at, _)| !old.contains(at))
                .map(|(_, shard)| shard)
                .collect();
            let mut after: Vec<&Shard> = kept.iter().copied().chain(new.iter().copied()).collect();
            after.sort_by_key(|shard| shard.id);
            let after_owned: Vec<Shard> = after
                .iter()
                .map(|shard| Shard {
                    id: shard.id,
                    column: shard.column.clone(),
                    positions: Arc::clone(&shard.positions),
                    mask: shard.mask.clone(),
                })
                .collect();
            let (again, again_reserves) = summed(&after_owned, 1);

            let new_parts: Vec<Part> = new.iter().map(|shard| shard.part()).collect();
            let added: Vec<(ShardId, &Part)> =
                new.iter().map(|shard| shard.id).zip(new_parts.iter()).collect();
            let old_parts: Vec<(usize, Part)> =
                old.iter().map(|&at| (at, parts[at].clone())).collect();
            let kept_parts = || {
                Some(
                    held.iter()
                        .zip(&parts)
                        .enumerate()
                        .filter(|(at, _)| !old.contains(at))
                        .map(|(_, (shard, part))| (shard.id, part.clone()))
                        .collect(),
                )
            };
            let (dense, grown, reserves) = difference(
                &pred,
                pred_reserves.as_ref(),
                &old_parts,
                &added,
                kept_parts,
                &position_of(&after_owned),
            )
            .expect("the difference holds");
            let at = format!("seed {seed}: {what}");
            assert_eq!(dense, *again.dense, "{at}: the walks' sum");
            assert!(grown.is_empty(), "{at}: nothing grown");
            assert_eq!(
                reserve_rows(reserves.as_ref()),
                reserve_rows(again_reserves.as_ref()),
                "{at}: the reserves"
            );
        }
    }
}

/// **A shard's growth leaves with its walk**: a sum followed through a growth in the shard whose
/// walk is replaced holds none of that growth afterwards, and one in a shard kept keeps it.
#[test]
fn a_replaced_shards_growth_leaves_the_sum_with_its_walk() {
    let mut rng = StdRng::seed_from_u64(5);
    let layout = ServingLayout::RowMajorList;
    let mut held: Vec<Shard> = (0..2)
        .map(|id| Shard::random(&mut rng, id, layout, Denying::Random(0.0)))
        .collect();
    let (pred, reserves) = summed(&held, 1);
    let parts: Vec<Part> = held.iter().map(Shard::part).collect();
    let mut joined = Vec::new();
    for (shard, rows) in held.iter_mut().zip([[3u32, 7], [11, 13]]) {
        let kept = shard
            .column
            .amend_kept(&[(rows[0], 1), (rows[1], 2)], ROWS)
            .unwrap();
        joined.push(kept.len() as u64);
        shard.column.record_step(1, 2, &kept);
    }
    assert!(joined.iter().all(|&n| n > 0), "each shard's growth joins a row");
    let follows: Vec<(Vec<_>, Vec<Placement<'_>>)> = held
        .iter()
        .map(|shard| {
            (
                shard.column.steps_between(1, 2).unwrap(),
                shard.positions.places(),
            )
        })
        .collect();
    type Placed<'a> = Box<dyn Fn(u32) -> Option<(u32, u32)> + 'a>;
    let every = |_: u32| true;
    let placed: Vec<Placed<'_>> = follows
        .iter()
        .map(|(_, places)| Box::new(move |row| place(places, row)) as Placed<'_>)
        .collect();
    let followed = pred.followed(
        &follows
            .iter()
            .enumerate()
            .map(|(at, (steps, _))| Follow {
                place: at,
                to: 2,
                steps: steps.clone(),
                counted: &every,
                place_of: &*placed[at],
            })
            .collect::<Vec<_>>(),
        Geometry::Box,
    );
    assert!(followed.count(1) > pred.count(1));
    let replacement = Shard::random(&mut rng, 0, layout, Denying::Random(0.0));
    let part = replacement.part();
    let after = [replacement, Shard {
        id: held[1].id,
        column: held[1].column.clone(),
        positions: Arc::clone(&held[1].positions),
        mask: held[1].mask.clone(),
    }];
    let (_, grown, _) = difference(
        &followed,
        reserves.as_ref(),
        &[(0, parts[0].clone())],
        &[(after[0].id, &part)],
        || Some(vec![(after[1].id, parts[1].clone())]),
        &position_of(&after),
    )
    .expect("the difference holds");
    let left: Vec<u32> = grown
        .values()
        .flat_map(|g| g.points.iter().map(|point| point.0))
        .collect();
    assert!(!left.is_empty() && left.iter().all(|&shard| shard == 1), "{left:?}");
    let total: u64 = grown.values().map(|g| g.figures.count).sum();
    assert_eq!(total, joined[1], "shard 1's rows grown, and none of shard 0's");
}

/// A difference that takes away more than the sum holds is refused, so the sum is made again.
#[test]
fn a_difference_taking_away_more_than_is_held_is_refused() {
    let mut rng = StdRng::seed_from_u64(8);
    let held: Vec<Shard> = (0..2)
        .map(|id| Shard::random(&mut rng, id, ServingLayout::RowMajorLabel, Denying::Random(0.0)))
        .collect();
    let (pred, reserves) = summed(&held, 1);
    let mut too_much = held[0].part();
    for count in &mut too_much.counts {
        *count += 1;
    }
    assert!(difference(
        &pred,
        reserves.as_ref(),
        &[(0, too_much)],
        &[],
        || None,
        &position_of(&held),
    )
    .is_none());
}

/// **What is written is what is read back**, and a file altered by one byte is not read.
#[test]
fn sums_walks_and_labels_written_to_disk_read_back_whole_or_not_at_all() {
    let mut rng = StdRng::seed_from_u64(11);
    let mut shards: Vec<Shard> = (0..2)
        .map(|id| Shard::random(&mut rng, id, ServingLayout::RowMajorList, Denying::Random(0.1)))
        .collect();
    let (counts, reserves) = summed(&shards, 6);
    let kept = shards[1]
        .column
        .amend_kept(&[(3, 1), (BASE - 1, 2)], ROWS)
        .expect("a list column takes any pair");
    shards[1].column.record_step(6, 7, &kept);
    let second = &shards[1];
    let places = second.positions.places();
    let counts = counts.followed(
        &[Follow {
            place: 1,
            to: 7,
            steps: second.column.steps_between(6, 7).unwrap(),
            counted: &|_| true,
            place_of: &|row| place(&places, row),
        }],
        Geometry::Box,
    );
    let dir = tempfile::tempdir().unwrap();
    let stem = super::persist::counts_stem(&[1; 32], "s0", LayerSpace::ZERO, 0, Geometry::Box);
    super::persist::write_sum(dir.path(), &stem, &counts, reserves.as_ref());
    let (read, read_reserves) =
        super::persist::read_sum(dir.path(), &stem, Geometry::Box).expect("it reads back");
    assert_eq!(read.sides, counts.sides, "each side, its versions and its growth");
    assert_eq!(read.dense, counts.dense);
    assert_eq!(read.grown, counts.grown, "what the step added is read back beside the walks");
    assert_eq!(read_reserves, reserves);
    assert!(super::persist::read_sum(dir.path(), &stem, Geometry::None).is_none());

    let part = shards[0].part();
    super::persist::write_part(dir.path(), &stem, 6, &part);
    assert_eq!(
        super::persist::read_part(dir.path(), &stem, 6, Geometry::Box),
        Some(part)
    );
    assert!(super::persist::read_part(dir.path(), &stem, 7, Geometry::Box).is_none());
    assert!(super::persist::read_part(dir.path(), &stem, 6, Geometry::Centroid).is_none());

    let column = &shards[0].column;
    let denied = sample(&mut rng, 0..BASE, 0.1);
    let labels = DeniedLabels::read([3; 32], column, 7, &denied, &shards[0].positions.places());
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
    let sum_stem = super::persist::counts_stem(&[1; 32], "s0", LayerSpace::ZERO, 0, Geometry::Box);
    let path = dir.path().join(format!("{sum_stem}-0.sum"));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[60] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(super::persist::read_sum(dir.path(), &sum_stem, Geometry::Box).is_none());
}

/// Counts and position sums over several shards pass what a `u32` and a `u64` hold, and are
/// held whole; taking a shard away brings them back into the arrays.
#[test]
fn sums_over_shards_are_widened_before_they_are_added() {
    let near_the_top = Part {
        len: 2,
        positions: true,
        reserved: false,
        ordinals: vec![0],
        counts: vec![u32::MAX],
        placed: vec![u32::MAX],
        sums: vec![[u64::MAX - 1, u64::MAX - 3]],
        boxes: vec![[0, 0, u32::MAX, u32::MAX]],
        reserves: Vec::new(),
    };
    let mut dense = Dense::empty(2, true);
    dense.add(&near_the_top);
    dense.add(&near_the_top);
    let figures = LevelFigures::of_dense(dense.clone());
    let placed = 2.0 * f64::from(u32::MAX);
    let sums = [2 * u128::from(u64::MAX - 1), 2 * u128::from(u64::MAX - 3)];
    assert_eq!(figures.get(0), 2 * u64::from(u32::MAX));
    assert_eq!(
        figures.centroid(0),
        Some([sums[0] as f64 / placed, sums[1] as f64 / placed])
    );
    dense.sub(&near_the_top).expect("the sum holds the part");
    assert!(dense.wide.is_empty());
    assert_eq!(dense.counts[0], u32::MAX);
    assert_eq!(dense.sums[0], [u64::MAX - 1, u64::MAX - 3]);
}
