//! **What the served hull costs and what shape it is**, over a built bundle's real memberships.
//!
//! Two questions on one pass, because both want the same gathered positions and the second is what
//! makes the first worth quoting:
//!
//! - **where the derivation's time goes**, stage by stage — reading a position per visible member
//!   (`gather`), the mean and the extremes a declared `centroid` or `box` already pays for,
//!   reducing the members to one representative per occupied cell (`reduce`), and everything the
//!   shape itself costs over the reduced input (`shape` — sort, wrap, group, dig);
//! - **the served shape against an independent convex wrap** — vertices, rings, wire bytes, area
//!   ratio, and how many members sit outside their own shape.
//!
//! The convex hull and the containment test here are a **second implementation**, written from the
//! definition rather than shared with the engine's: they are both the baseline the served shape is
//! timed against and the oracle its containment is checked against, and a baseline that called the
//! code under test would measure nothing.
//!
//! **It also asserts, and exits non-zero.** While real memberships are in hand it holds the
//! engine's run-folding reduction to a plain hash-map binning that shares no code with it: the
//! representative sets must be equal, and the shapes follow, because the dig sorts and
//! deduplicates the set it is given. That is a correctness differential rather than a measurement,
//! and it is here because this is the only place a million-member cell arrives.
//!
//! Single-threaded throughout — `tessera_engine::derived` is, deliberately, and a timed section
//! that was not would be measuring a different program.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin hull_cost -- <bundle root>
//! cargo run --release -p tessera-bench --bin hull_cost -- <bundle root> \
//!     --layer clusters/hdbscan --rows
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use croaring::Bitmap;

use tessera_engine::derived::{compute, dig_rings, quantised, ComputedProperty, RowLocator};
use tessera_store::read::{open_bundle, Bundle, SegmentData};

/// The resolution the engine reduces at, **written out here rather than read from it**. This
/// binary's oracle is a second implementation of the reduction, and an oracle that imported the
/// constant it is binning by would share the one thing the two sides must agree on independently.
/// A resolution change shows up as a failed differential, which is the right way for it to show up.
const DIVISIONS: u32 = 1_024;

/// The membership size the engine reduces above, on the same terms as [`DIVISIONS`].
const REDUCTION_FLOOR: usize = 75_000;

/// How far inside every edge of the extreme octagon a member must be before the oracle discards it
/// as a hull candidate, on the same terms as [`DIVISIONS`].
const OCTAGON_MARGIN: f64 = 65_536.0;

#[derive(Parser)]
#[command(about = "the served hull's cost and shape over a built bundle's memberships")]
struct Args {
    /// Bundle root — the directory holding `CURRENT`. Opened read-only.
    bundle: PathBuf,
    /// Measure only this layer, rather than every layer declaring a derived hull.
    #[arg(long)]
    layer: Option<String>,
    /// Print one line per artifact as well as the per-layer summary.
    #[arg(long)]
    rows: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let bundle = match open_bundle(&args.bundle) {
        Ok(b) => Box::leak(Box::new(b)) as &'static Bundle,
        Err(e) => {
            eprintln!("{}: {e}", args.bundle.display());
            return ExitCode::FAILURE;
        }
    };
    // The pack paths in a side-manifest are relative to the published prefix, `CURRENT` names it.
    let current: serde_json::Value =
        match std::fs::read(args.bundle.join("CURRENT")).map(|b| serde_json::from_slice(&b)) {
            Ok(Ok(v)) => v,
            _ => {
                eprintln!("{}: no readable CURRENT", args.bundle.display());
                return ExitCode::FAILURE;
            }
        };
    let prefix = args
        .bundle
        .join(current["prefix"].as_str().unwrap_or_default());

    let mut measured = 0usize;
    let mut failed = false;
    for (partition_name, partition) in &bundle.partitions {
        for layer in &partition.manifest.layers {
            let name = &layer.declaration.name;
            if !layer.declaration.content.computed.iter().any(|c| c == "hull") {
                continue;
            }
            if args.layer.as_ref().is_some_and(|want| want != name) {
                continue;
            }
            let Some(view_name) = layer.declaration.views.first() else {
                continue;
            };
            let Some(view) = partition.views.get(view_name) else {
                eprintln!("{name}: view '{view_name}' is not in partition '{partition_name}'");
                continue;
            };
            let locator = locator_over(view);

            let mut clouds: Vec<Cloud> = Vec::new();
            for extent in &partition.manifest.membership_extents {
                if &extent.layer != name {
                    continue;
                }
                let path = prefix.join(Path::new(&extent.path));
                let pack = match tessera_store::membership::MembershipPack::open(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("{}: {e}", path.display());
                        failed = true;
                        continue;
                    }
                };
                for (ordinal, blob) in pack.iter() {
                    let Some((record, _)) = tessera_lifecycle::membership::decode_record(
                        tessera_types::EntityId::new(1),
                        blob,
                    ) else {
                        continue;
                    };
                    let visible = view.row_space.project_base(&record.members);
                    if visible.is_empty() {
                        continue;
                    }
                    let t = Instant::now();
                    let positions = locator.positions(&visible);
                    let gather_ms = t.elapsed().as_secs_f64() * 1e3;
                    if positions.len() < 3 {
                        continue;
                    }
                    clouds.push(Cloud {
                        ordinal,
                        visible,
                        positions,
                        gather_ms,
                    });
                }
            }
            if clouds.is_empty() {
                println!("{name}: no artifact has three visible members");
                continue;
            }
            measured += 1;
            failed |= report(name, view_name, &clouds, &locator, args.rows);
        }
    }

    if measured == 0 {
        eprintln!(
            "{}: no layer declares a derived hull{}",
            args.bundle.display(),
            args.layer
                .as_ref()
                .map_or(String::new(), |l| format!(" under the name '{l}'")),
        );
        return ExitCode::FAILURE;
    }
    if failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// The locator over a view's segments, in row-base order — the way `compute` resolves a member row
/// to a position, so what is timed below is the one pass every declared property pays for.
fn locator_over(view: &'static tessera_store::read::ViewData) -> RowLocator<'static> {
    let row_bases: HashMap<&str, u32> = view
        .row_space
        .extents()
        .iter()
        .map(|e| (e.seg_id.as_str(), e.row_base))
        .collect();
    let mut segments: Vec<(&'static SegmentData, u32)> = view
        .segments
        .iter()
        .map(|s| {
            let base = row_bases.get(s.seg_id.as_str()).copied().unwrap_or(0);
            (s.as_ref(), base)
        })
        .collect();
    segments.sort_unstable_by_key(|&(_, base)| base);
    RowLocator::new(segments)
}

/// One artifact's gathered membership: the mask `compute` is given and the positions it reads.
struct Cloud {
    ordinal: u32,
    visible: Bitmap,
    positions: Vec<[u32; 2]>,
    gather_ms: f64,
}

/// One artifact's row: the four stages a change can move independently, and what the shape is.
struct Row {
    ordinal: u32,
    members: usize,
    representatives: usize,
    wrap: usize,
    vertices: usize,
    rings: usize,
    outside: usize,
    gather_ms: f64,
    box_ms: f64,
    reduce_ms: f64,
    shape_ms: f64,
    wrap_ms: f64,
    derive_ms: f64,
    area_ratio: f64,
}

/// Measure one layer and print its table. Returns `true` if the reduction differential failed on
/// any artifact.
fn report(
    layer: &str,
    view: &str,
    clouds: &[Cloud],
    locator: &RowLocator<'_>,
    per_row: bool,
) -> bool {
    let mut rows: Vec<Row> = Vec::with_capacity(clouds.len());
    let mut failed = false;

    for cloud in clouds {
        let positions = &cloud.positions;
        let t = Instant::now();
        let bc = box_and_centroid(positions);
        let box_ms = t.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(bc);

        let t = Instant::now();
        let reduced = quantised(positions);
        let reduce_ms = t.elapsed().as_secs_f64() * 1e3;
        let representatives = reduced.as_ref().map_or(positions.len(), |r| r.len());

        // The dig with the reduction inside it, timed whole and the reduction taken back off, so
        // the stage rows below separate the two passes without either being measured twice.
        let t = Instant::now();
        let dug = dig_rings(positions, usize::MAX);
        let shape_ms = (t.elapsed().as_secs_f64() * 1e3 - reduce_ms).max(0.0);
        std::hint::black_box(dug);

        let t = Instant::now();
        let wrap = convex_hull(positions);
        let wrap_ms = t.elapsed().as_secs_f64() * 1e3;

        // The whole derivation as a request pays for it — gather, mean, extremes and shape in the
        // order `compute` runs them, which is the number the stage rows have to add up to.
        let t = Instant::now();
        let derived = compute(
            &[
                ComputedProperty::Centroid,
                ComputedProperty::Box,
                ComputedProperty::Hull,
            ],
            &cloud.visible,
            locator,
        );
        let derive_ms = t.elapsed().as_secs_f64() * 1e3;
        // Every α-group is its own part on the wire; the shape is measured over the rings.
        let shape: Vec<Vec<[u32; 2]>> = derived
            .shape
            .expect("a declared hull")
            .into_iter()
            .flatten()
            .collect();

        let outside = positions
            .iter()
            .filter(|m| !shape.iter().any(|r| contains(r, **m)))
            .count();
        let area_ratio = shape.iter().map(|r| double_area(r)).sum::<i128>() as f64
            / (double_area(&wrap) as f64).max(1.0);

        failed |= !reduction_is_the_definition(cloud.ordinal, positions, reduced.as_deref());

        rows.push(Row {
            ordinal: cloud.ordinal,
            members: positions.len(),
            representatives,
            wrap: wrap.len(),
            vertices: shape.iter().map(|r| r.len()).sum(),
            rings: shape.len(),
            outside,
            gather_ms: cloud.gather_ms,
            box_ms,
            reduce_ms,
            shape_ms,
            wrap_ms,
            derive_ms,
            area_ratio,
        });
    }

    rows.sort_unstable_by_key(|r| r.members);
    println!("\n{layer} ({view}): {} artifacts", rows.len());
    if per_row {
        println!(
            "row,ordinal,members,representatives,wrap,vertices,rings,outside,\
gather_ms,box_ms,reduce_ms,shape_ms,wrap_ms,derive_ms,area_ratio"
        );
        for r in &rows {
            println!(
                "row,{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.4}",
                r.ordinal,
                r.members,
                r.representatives,
                r.wrap,
                r.vertices,
                r.rings,
                r.outside,
                r.gather_ms,
                r.box_ms,
                r.reduce_ms,
                r.shape_ms,
                r.wrap_ms,
                r.derive_ms,
                r.area_ratio
            );
        }
    }

    let sum = |f: fn(&Row) -> f64| -> f64 { rows.iter().map(f).sum() };
    let (g, b, q, s, w) = (
        sum(|r| r.gather_ms),
        sum(|r| r.box_ms),
        sum(|r| r.reduce_ms),
        sum(|r| r.shape_ms),
        sum(|r| r.wrap_ms),
    );
    let total = g + b + q + s;
    let members: usize = rows.iter().map(|r| r.members).sum();
    let reps: usize = rows.iter().map(|r| r.representatives).sum();
    let wrap_vertices: usize = rows.iter().map(|r| r.wrap).sum();
    let vertices: usize = rows.iter().map(|r| r.vertices).sum();
    let rings: usize = rows.iter().map(|r| r.rings).sum();
    let mut ratios: Vec<f64> = rows.iter().map(|r| r.area_ratio).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    println!(
        "  members {members} ({} … {}), representatives {reps}",
        rows[0].members,
        rows[rows.len() - 1].members
    );
    println!(
        "  gather {g:.0} ms ({:.0}%)  box+centroid {b:.0} ms ({:.0}%)  reduce {q:.0} ms \
({:.0}%)  shape {s:.0} ms ({:.0}%)  total {total:.0} ms",
        100.0 * g / total,
        100.0 * b / total,
        100.0 * q / total,
        100.0 * s / total,
    );
    println!(
        "  the hull's own cost (reduce+shape) {:.0} ms, against an independent convex wrap at \
{w:.0} ms; compute(centroid, box, hull) end to end {:.0} ms",
        q + s,
        sum(|r| r.derive_ms),
    );
    println!(
        "  vertices {wrap_vertices} → {vertices} in {rings} rings ({} → {} bytes at 8 a vertex \
plus 4 a ring, {:+.1}%)",
        wrap_vertices * 8,
        vertices * 8 + rings * 4,
        100.0 * ((vertices * 8 + rings * 4) as f64 / (wrap_vertices * 8) as f64 - 1.0),
    );
    println!(
        "  area against the wrap: median {:.3}, tightest {:.3}, widest {:.3}",
        ratios[ratios.len() / 2],
        ratios[0],
        ratios[ratios.len() - 1]
    );
    println!(
        "  members outside every ring of their own shape: {} on {} artifacts, worst {:.4}% of \
one artifact's",
        rows.iter().map(|r| r.outside).sum::<usize>(),
        rows.iter().filter(|r| r.outside > 0).count(),
        100.0
            * rows
                .iter()
                .map(|r| r.outside as f64 / r.members as f64)
                .fold(0.0, f64::max),
    );
    failed
}

/// **The reduction the engine computes is the reduction the definition asks for, member for
/// member** — the assertion that makes the route to the occupied cells a speed change and not a
/// shape change.
///
/// The engine finds the occupied cells by folding runs of consecutive members and answers both
/// hull-candidate questions per *cell*, over a band. [`dense_reduction`] does neither: it bins
/// every member into a hash map keyed on the cell and tests every member against the octagon on
/// its own.
///
/// What is checked is that the representative **sets** are equal, and the rings follow from that:
/// the dig sorts and deduplicates what it is given, so two equal sets dig to one shape.
fn reduction_is_the_definition(
    ordinal: u32,
    positions: &[[u32; 2]],
    engine: Option<&[[u32; 2]]>,
) -> bool {
    let oracle = dense_reduction(positions);
    if engine.is_some() != oracle.is_some() {
        eprintln!(
            "artifact {ordinal}: the two routes disagree about whether the reduction engages"
        );
        return false;
    }
    let (Some(engine), Some(oracle)) = (engine, oracle) else {
        return true;
    };

    let normalise = |v: &[[u32; 2]]| {
        let mut v = v.to_vec();
        v.sort_unstable();
        v.dedup();
        v
    };
    let (engine_set, oracle_set) = (normalise(engine), normalise(&oracle));
    if engine_set != oracle_set {
        eprintln!(
            "artifact {ordinal}: the representatives and hull candidates are not the same set \
             ({} against {})",
            engine_set.len(),
            oracle_set.len()
        );
        return false;
    }
    true
}

/// One member per occupied cell and every hull candidate beside it, **the slow obvious way**: a
/// hash map over every member for the cells, an exact octagon over every member for the candidates,
/// and no run, band or merge anywhere.
fn dense_reduction(points: &[[u32; 2]]) -> Option<Vec<[u32; 2]>> {
    if points.len() < REDUCTION_FLOOR {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for q in points {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
    let (wx, wy) = ((x1 - x0) as u64 + 1, (y1 - y0) as u64 + 1);
    let shift = wx
        .max(wy)
        .div_ceil(DIVISIONS as u64)
        .next_power_of_two()
        .trailing_zeros();
    let side = 1u64 << shift;
    if side <= 1 {
        return None;
    }
    let half = side / 2;

    // Every member into its cell, keeping the one nearest that cell's centre and breaking ties on
    // the position.
    let mut cells: HashMap<(u64, u64), ([u32; 2], u64)> = HashMap::new();
    for q in points {
        let (cx, cy) = ((q[0] as u64) >> shift, (q[1] as u64) >> shift);
        let (mx, my) = ((cx << shift) + half, (cy << shift) + half);
        let (dx, dy) = ((q[0] as u64).abs_diff(mx), (q[1] as u64).abs_diff(my));
        let d = dx * dx + dy * dy;
        cells
            .entry((cx, cy))
            .and_modify(|best| {
                if d < best.1 || (d == best.1 && *q < best.0) {
                    *best = (*q, d);
                }
            })
            .or_insert((*q, d));
    }
    if cells.len() * 4 > points.len() * 3 {
        return None;
    }

    // Akl–Toussaint over every member, one at a time: a member strictly inside the polygon the
    // eight extremes span cannot be a convex-hull vertex, and every one that could be survives.
    const DIRECTIONS: [(i64, i64); 8] = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    let mut best: [Option<(i64, [u32; 2])>; 8] = [None; 8];
    for q in points {
        let (x, y) = (q[0] as i64, q[1] as i64);
        for (slot, (wx, wy)) in best.iter_mut().zip(DIRECTIONS) {
            let score = wx * x + wy * y;
            if slot.is_none_or(|(s, b)| score > s || (score == s && *q < b)) {
                *slot = Some((score, *q));
            }
        }
    }
    let extremes: Vec<[u32; 2]> = best.into_iter().flatten().map(|(_, q)| q).collect();
    let ring = convex_hull(&extremes);
    let edges: Vec<[f64; 3]> = (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            let (ax, ay) = (a[0] as f64, a[1] as f64);
            let (bx, by) = (b[0] as f64, b[1] as f64);
            [-(by - ay), bx - ax, (by - ay) * ax - (bx - ax) * ay]
        })
        .collect();
    let inside = |q: [u32; 2]| -> bool {
        if ring.len() < 3 {
            return false;
        }
        let (x, y) = (q[0] as f64, q[1] as f64);
        edges
            .iter()
            .all(|[a, b, c]| a * x + b * y + c > OCTAGON_MARGIN)
    };

    let mut out: Vec<[u32; 2]> = cells.values().map(|(rep, _)| *rep).collect();
    for q in points {
        if !inside(*q) {
            out.push(*q);
        }
    }
    Some(out)
}

/// The `box` and `centroid` properties over the gathered positions, written from the definition —
/// the profile's floor, since a layer declaring either already pays a pass over every member and
/// the hull's own cost is what it adds on top.
fn box_and_centroid(points: &[[u32; 2]]) -> ([u32; 4], [f64; 2]) {
    let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
    let (mut sx, mut sy) = (0u64, 0u64);
    for p in points {
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
        sx += u64::from(p[0]);
        sy += u64::from(p[1]);
    }
    let n = points.len() as f64;
    (b, [sx as f64 / n, sy as f64 / n])
}

/// Andrew's monotone chain, counter-clockwise — exact in `i128`, and written from the definition
/// rather than shared with the engine's, which is what makes it a baseline.
fn convex_hull(points: &[[u32; 2]]) -> Vec<[u32; 2]> {
    let mut p = points.to_vec();
    p.sort_unstable();
    p.dedup();
    if p.len() <= 2 {
        return p;
    }
    let turn = |o: [u32; 2], a: [u32; 2], b: [u32; 2]| -> i128 {
        let (ox, oy) = (o[0] as i128, o[1] as i128);
        (a[0] as i128 - ox) * (b[1] as i128 - oy) - (a[1] as i128 - oy) * (b[0] as i128 - ox)
    };
    let mut hull: Vec<[u32; 2]> = Vec::with_capacity(p.len() + 1);
    for &point in &p {
        while hull.len() >= 2 && turn(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    let lower = hull.len() + 1;
    for &point in p.iter().rev().skip(1) {
        while hull.len() >= lower && turn(hull[hull.len() - 2], hull[hull.len() - 1], point) <= 0 {
            hull.pop();
        }
        hull.push(point);
    }
    hull.pop();
    hull
}

/// `p` lies on the closed segment `a b`.
fn on_segment(a: [u32; 2], b: [u32; 2], p: [u32; 2]) -> bool {
    let (ax, ay, bx, by) = (a[0] as i128, a[1] as i128, b[0] as i128, b[1] as i128);
    let (px, py) = (p[0] as i128, p[1] as i128);
    (bx - ax) * (py - ay) - (by - ay) * (px - ax) == 0
        && p[0] >= a[0].min(b[0])
        && p[0] <= a[0].max(b[0])
        && p[1] >= a[1].min(b[1])
        && p[1] <= a[1].max(b[1])
}

/// `p` is inside the ring or on its boundary — crossing number, with the boundary tested first so a
/// member sitting on an edge counts as contained.
fn contains(poly: &[[u32; 2]], p: [u32; 2]) -> bool {
    let n = poly.len();
    if n == 1 {
        return poly[0] == p;
    }
    for i in 0..n {
        if on_segment(poly[i], poly[(i + 1) % n], p) {
            return true;
        }
    }
    if n == 2 {
        return false;
    }
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let d = b[1] as i128 - a[1] as i128;
            let lhs = (p[0] as i128 - a[0] as i128) * d;
            let rhs = (p[1] as i128 - a[1] as i128) * (b[0] as i128 - a[0] as i128);
            if (d > 0 && lhs < rhs) || (d < 0 && lhs > rhs) {
                inside = !inside;
            }
        }
    }
    inside
}

/// Twice the signed area of a ring — positive for counter-clockwise, exact in `i128`.
fn double_area(poly: &[[u32; 2]]) -> i128 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a[0] as i128 * b[1] as i128 - b[0] as i128 * a[1] as i128
        })
        .sum()
}
