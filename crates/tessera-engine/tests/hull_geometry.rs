//! **The measurement campaign behind the served hull** — the figures `docs/design/artifact-shapes.md`
//! §6 cites, and before it `docs/evidence/memos/2026-08-26-concave-hulls.md`.
//!
//! Ignored by default, because the corpus it reads is not in the repository. Run it as
//!
//! ```text
//! TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
//!   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture
//! ```
//!
//! The convex hull here is a **second implementation**, written from the definition rather than
//! shared with the engine's: it is both the baseline the served shape is timed against and the
//! oracle its containment is checked against on real memberships, and a baseline that called the
//! code under test would measure nothing. The properties themselves are tested in
//! `tessera_engine::derived`'s own tests and, through the serving path, in `artifact_content.rs`.

use std::time::Instant;
use tessera_engine::derived::{compute, ComputedProperty};

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/ring.rs"]
mod ring;
use ring::{contains, convex_hull, double_area};

#[test]
#[ignore]
fn measure_against_the_corpus() {
    let corpus = corpus::open();
    let mut rows: Vec<Row> = Vec::new();

    for (ordinal, visible) in &corpus.memberships {
        // The three costs, separated: reading a position per member is what a `box` already pays, so
        // it is the floor both hulls sit on and not part of either's own cost.
        let t0 = Instant::now();
        let positions = corpus::gather(visible, &corpus.locator);
        let gather_ms = t0.elapsed().as_secs_f64() * 1e3;
        if positions.len() < 3 {
            continue;
        }

        let t1 = Instant::now();
        let wrap = convex_hull(&positions);
        let wrap_ms = t1.elapsed().as_secs_f64() * 1e3;

        let t2 = Instant::now();
        let shape = compute(&[ComputedProperty::Hull], visible, &corpus.locator)
            .hull
            .expect("declared");
        let shape_ms = t2.elapsed().as_secs_f64() * 1e3 - gather_ms;

        // **Containment is counted, not asserted** (`artifact-shapes.md` §4's head, ruled
        // 2026-08-28: a shape is a summary of where a cluster is, not a per-point assertion). The
        // dig itself still holds every member it is given; what can put one outside is that the dig
        // is given one representative member per grid cell rather than all of them, so a member can
        // sit up to a cell beyond its own shape. The figure is what `the_quantisation_sweep`
        // chooses the resolution against, and it is reported here on the served construction.
        let escaped = positions
            .iter()
            .filter(|m| !shape.iter().any(|r| contains(r, **m)))
            .count();
        let mut foreign = 0usize;
        if shape.len() > 1 {
            for m in &positions {
                if shape.iter().filter(|r| contains(r, *m)).count() > 1 {
                    foreign += 1;
                }
            }
        }

        // A visual check is the point of the change, so the shapes themselves are dumpable: set
        // `TESSERA_HULL_DUMP` to a directory and each artifact's members, wrap and rings are written
        // there as CSV for whatever will draw them.
        if let Ok(dir) = std::env::var("TESSERA_HULL_DUMP") {
            let dump = |name: String, pts: &[[u32; 2]]| {
                let body: String = pts.iter().map(|p| format!("{},{}\n", p[0], p[1])).collect();
                std::fs::write(format!("{dir}/{ordinal}-{name}.csv"), body).unwrap();
            };
            dump("wrap".into(), &wrap);
            for (k, r) in shape.iter().enumerate() {
                dump(format!("shape-{k}"), r);
            }
            let stride = (positions.len() / 20_000).max(1);
            let thinned: Vec<[u32; 2]> = positions.iter().copied().step_by(stride).collect();
            dump("members".into(), &thinned);
        }

        // What the served budget cost this artifact: the same dig with no cap, so "at the budget"
        // below is a fact about this shape rather than an arithmetic guess from a constant.
        let uncapped = tessera_engine::derived::dig_rings(&positions, usize::MAX).0;

        rows.push(Row {
            escaped,
            capped: shape.iter().map(|r| r.len()).sum::<usize>()
                < uncapped.iter().map(|r| r.len()).sum::<usize>(),
            members: positions.len(),
            wrap: wrap.len(),
            shape: shape.iter().map(|r| r.len()).sum(),
            rings: shape.len(),
            foreign,
            gather_ms,
            wrap_ms,
            shape_ms,
            ratio: shape.iter().map(|r| double_area(r)).sum::<i128>() as f64
                / (double_area(&wrap) as f64).max(1.0),
        });
    }

    rows.sort_unstable_by_key(|r| r.members);
    println!("members,wrap_vertices,shape_vertices,rings,gather_ms,wrap_ms,shape_ms,area_ratio");
    for r in &rows {
        println!(
            "{},{},{},{},{:.3},{:.3},{:.3},{:.4}",
            r.members, r.wrap, r.shape, r.rings, r.gather_ms, r.wrap_ms, r.shape_ms, r.ratio
        );
    }

    let total_wrap: usize = rows.iter().map(|r| r.wrap).sum();
    let total_shape: usize = rows.iter().map(|r| r.shape).sum();
    let total_rings: usize = rows.iter().map(|r| r.rings).sum();
    // **Exhaustion against the unbounded dig, not against a copy of the constant.** A test that
    // hard-codes the budget it is measuring reports the wrong number the day the budget moves,
    // which is exactly what happened to this line when it said 64.
    let at_budget = rows.iter().filter(|r| r.capped).count();
    let undug = rows.iter().filter(|r| r.shape == r.wrap).count();
    println!(
        "\n{} artifacts, {} … {} members",
        rows.len(),
        rows[0].members,
        rows[rows.len() - 1].members
    );
    println!(
        "wire vertices {} → {} ({} → {} bytes at 8 per vertex plus 4 per ring, +{:.1}%); {at_budget} at the budget, {undug} not dug at all",
        total_wrap,
        total_shape,
        total_wrap * 8,
        total_shape * 8 + total_rings * 4,
        100.0 * ((total_shape * 8 + total_rings * 4) as f64 / (total_wrap * 8) as f64 - 1.0)
    );
    println!(
        "members outside every ring of their own shape: {} over the layer, on {} artifacts; worst artifact {:.4}% of its members",
        rows.iter().map(|r| r.escaped).sum::<usize>(),
        rows.iter().filter(|r| r.escaped > 0).count(),
        100.0 * rows.iter().map(|r| r.escaped as f64 / r.members as f64).fold(0.0, f64::max),
    );
    println!(
        "rings: {total_rings} over the layer; {} artifacts carry more than one; {} members inside more than one ring of their own artifact",
        rows.iter().filter(|r| r.rings > 1).count(),
        rows.iter().map(|r| r.foreign).sum::<usize>(),
    );
    let sum = |f: fn(&Row) -> f64| rows.iter().map(f).sum::<f64>();
    println!(
        "whole layer: gather {:.1} ms, wrap {:.1} ms, shape {:.1} ms (shape is {:.1}× the wrap, {:.0}% of gather+shape)",
        sum(|r| r.gather_ms),
        sum(|r| r.wrap_ms),
        sum(|r| r.shape_ms),
        sum(|r| r.shape_ms) / sum(|r| r.wrap_ms),
        100.0 * sum(|r| r.shape_ms) / (sum(|r| r.gather_ms) + sum(|r| r.shape_ms))
    );
    let biggest = &rows[rows.len() - 1];
    println!(
        "largest artifact: {} members, gather {:.1} ms, wrap {:.1} ms, shape {:.1} ms",
        biggest.members, biggest.gather_ms, biggest.wrap_ms, biggest.shape_ms
    );
    let mut ratios: Vec<f64> = rows.iter().map(|r| r.ratio).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "area ratio: mean {:.3}, median {:.3}, min {:.3}, max {:.3}",
        ratios.iter().sum::<f64>() / ratios.len() as f64,
        ratios[ratios.len() / 2],
        ratios[0],
        ratios[ratios.len() - 1]
    );
}

struct Row {
    escaped: usize,
    capped: bool,
    members: usize,
    wrap: usize,
    shape: usize,
    rings: usize,
    foreign: usize,
    gather_ms: f64,
    wrap_ms: f64,
    shape_ms: f64,
    ratio: f64,
}

/// **Ruling B's sweep: what each vertex budget buys, on the shape as it is now built.**
///
/// The memo's sweep (`docs/evidence/memos/2026-08-26-concave-hulls.md`) predates the multi-ring
/// grouping, and a budget shared across an artifact's rings does not buy what a single ring's did,
/// so the numbers `artifact-shapes.md` §8 B rests on are these rather than those.
///
/// Three things separate it from `measure_against_the_corpus` above. The area denominator is the
/// **groups' own wraps** — the dig at budget 0, which is the same construction with the digging
/// switched off — and not the whole membership's convex wrap, because the question is what the
/// digging buys and grouping has already been paid for. Exhaustion is reported by the construction
/// itself (`dig_rings`'s second return) rather than inferred from a vertex count, so a dig that
/// happened to stop at the cap with nothing left to dig is not counted as capped. And containment
/// is checked at the **largest** budget in the sweep, which is where the ring is most folded and a
/// broken induction would show first.
///
/// ```text
/// TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
///   TESSERA_HULL_PACK=partitions/default/members/members-000000-001.tsmb \
///   TESSERA_HULL_BUDGETS=64,128,256,512,1024,0 \
///   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture the_budget_sweep
/// ```
///
/// `0` in the budget list means **unbounded** — the dig runs until no bridging edge is left.
#[test]
#[ignore]
fn the_budget_sweep() {
    let corpus = corpus::open();
    let budgets: Vec<usize> = std::env::var("TESSERA_HULL_BUDGETS")
        .unwrap_or_else(|_| "64,128,256,512,1024".into())
        .split(',')
        .map(|s| match s.trim().parse::<usize>().expect("a budget") {
            0 => usize::MAX,
            n => n,
        })
        .collect();
    let largest = *budgets.iter().max().expect("a budget");

    let mut clouds: Vec<Vec<[u32; 2]>> = Vec::new();
    for (_, visible) in &corpus.memberships {
        let positions = corpus::gather(visible, &corpus.locator);
        if positions.len() >= 3 {
            clouds.push(positions);
        }
    }

    // The denominator, once: the same construction with the digging switched off, so every ratio
    // below is what the digging bought over the rings the grouping alone drew.
    let wraps: Vec<(usize, usize, f64)> = clouds
        .iter()
        .map(|c| {
            let (rings, _) = tessera_engine::derived::dig_rings(c, 0);
            (
                rings.iter().map(|r| r.len()).sum::<usize>(),
                rings.len(),
                rings.iter().map(|r| double_area(r)).sum::<i128>() as f64,
            )
        })
        .collect();
    let wrap_vertices: usize = wraps.iter().map(|w| w.0).sum();
    let wrap_rings: usize = wraps.iter().map(|w| w.1).sum();
    println!(
        "{} artifacts, {} … {} members; group wraps: {wrap_vertices} vertices in {wrap_rings} rings, {} bytes",
        clouds.len(),
        clouds.iter().map(|c| c.len()).min().unwrap(),
        clouds.iter().map(|c| c.len()).max().unwrap(),
        wrap_vertices * 8 + wrap_rings * 4,
    );
    println!(
        "\nbudget,vertices,hull_bytes,vs_wrap_bytes,exhausted,digs_p50,digs_p99,digs_max,area_median,area_worst,dig_ms"
    );

    for &budget in &budgets {
        let mut vertices = 0usize;
        let mut rings_total = 0usize;
        let mut exhausted = 0usize;
        let mut ratios: Vec<f64> = Vec::with_capacity(clouds.len());
        // Digs spent per artifact: at the unbounded run this is what a cap has to clear to stop
        // being a fidelity control, which is the number ruling B is actually about.
        let mut digs: Vec<usize> = Vec::with_capacity(clouds.len());
        let mut dig_ms = 0.0f64;
        for (cloud, wrap) in clouds.iter().zip(&wraps) {
            let t = Instant::now();
            let (rings, capped) = tessera_engine::derived::dig_rings(cloud, budget);
            dig_ms += t.elapsed().as_secs_f64() * 1e3;
            vertices += rings.iter().map(|r| r.len()).sum::<usize>();
            rings_total += rings.len();
            exhausted += usize::from(capped);
            ratios
                .push(rings.iter().map(|r| double_area(r)).sum::<i128>() as f64 / wrap.2.max(1.0));
            digs.push(rings.iter().map(|r| r.len()).sum::<usize>() - wrap.0);
            if budget == largest {
                // Containment is the induction the construction rests on, and it must survive the
                // deepest digging the sweep asks for, not only the served budget.
                assert!(
                    cloud.iter().all(|m| rings.iter().any(|r| contains(r, *m))),
                    "a member fell outside every ring at budget {budget}"
                );
            }
        }
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
        digs.sort_unstable();
        let bytes = vertices * 8 + rings_total * 4;
        println!(
            "{},{vertices},{bytes},{:.2}×,{exhausted},{},{},{},{:.4},{:.4},{:.0}",
            if budget == usize::MAX {
                "unbounded".to_string()
            } else {
                budget.to_string()
            },
            bytes as f64 / (wrap_vertices * 8 + wrap_rings * 4) as f64,
            digs[digs.len() / 2],
            digs[digs.len() * 99 / 100],
            digs[digs.len() - 1],
            ratios[ratios.len() / 2],
            ratios[0],
            dig_ms,
        );
    }
}

/// **What binning the members before the shape costs, and what it buys** — the sweep behind
/// `tessera_engine::derived`'s `QUANTISE_DIVISIONS` and `artifact-shapes.md` §7.1.
///
/// Every construction in this family consumes one position per visible member to produce something
/// whose resolution is bounded by the drawing: the largest shape on this layer is 757 vertices over
/// 2.42M members, drawn about a thousand pixels wide. The engine therefore bins the members to a
/// square grid over their own bounding box and digs over one real member per occupied cell. This
/// measures what that does, against the same construction with the binning switched off.
///
/// Five columns decide it, and the fifth is the one the owner's containment ruling
/// (`artifact-shapes.md` §4's head) made admissible at all:
///
/// - **the input**, since that is what the whole change is;
/// - **α**, which is derived from the members and must not move — three times the median edge of
///   their own convex wrap, so a wrap whose vertices have been displaced by a cell would move it;
/// - **the area**, against the unquantised shape rather than against the wrap;
/// - **the excursion**, the two-way worst departure of either ring from the other
///   (`ring::excursion`), reported as a fraction of the artifact's own longer axis — which is what
///   makes it comparable to a pixel, a shape being drawn at most a viewport wide;
/// - **members outside their own shape**, which quantisation can now produce and which the
///   unquantised dig could not.
///
/// The dig is **unbounded** here rather than at the served budget of 2,048, because no artifact on
/// this layer reaches that budget (`measure_against_the_corpus` reports 0 at the budget) and an
/// unbounded dig removes the cap from the comparison entirely.
///
/// ```text
/// TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
///   TESSERA_HULL_DIVISIONS=256,512,1024,2048,4096 \
///   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture the_quantisation_sweep
/// ```
#[test]
#[ignore]
fn the_quantisation_sweep() {
    let corpus = corpus::open();
    let divisions: Vec<u32> = std::env::var("TESSERA_HULL_DIVISIONS")
        .unwrap_or_else(|_| "256,512,1024,2048,4096".into())
        .split(',')
        .map(|s| s.trim().parse::<u32>().expect("a division count"))
        .collect();

    let mut clouds: Vec<Vec<[u32; 2]>> = Vec::new();
    let mut gather_ms: Vec<f64> = Vec::new();
    for (_, visible) in &corpus.memberships {
        let t = Instant::now();
        let positions = corpus::gather(visible, &corpus.locator);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if positions.len() >= 3 {
            clouds.push(positions);
            gather_ms.push(ms);
        }
    }

    // The baseline, once: the same construction with the binning switched off. Every ratio below is
    // against this shape and not against the convex wrap, because the question is what the binning
    // changed and not what the digging bought.
    let mut base: Vec<Base> = Vec::new();
    for cloud in &clouds {
        let t = Instant::now();
        let rings = tessera_engine::derived::dig_rings_at(cloud, usize::MAX, 0).0;
        base.push(Base {
            ms: t.elapsed().as_secs_f64() * 1e3,
            area: rings.iter().map(|r| double_area(r)).sum::<i128>() as f64,
            vertices: rings.iter().map(|r| r.len()).sum(),
            alpha_sq: ring::alpha_sq(cloud) as f64,
            extent: extent_of(cloud),
            rings,
        });
    }
    let total_members: usize = clouds.iter().map(|c| c.len()).sum();
    println!(
        "{} artifacts, {} … {} members ({total_members} in total); unquantised: {} vertices, {:.0} ms of digging, {:.0} ms of gathering",
        clouds.len(),
        clouds.iter().map(|c| c.len()).min().unwrap(),
        clouds.iter().map(|c| c.len()).max().unwrap(),
        base.iter().map(|b| b.vertices).sum::<usize>(),
        base.iter().map(|b| b.ms).sum::<f64>(),
        gather_ms.iter().sum::<f64>(),
    );
    println!(
        "\ndivisions,input,vertices,dig_ms,p50_ms,p90_ms,worst_ms,alpha_worst,area_median,area_worst,excursion_median,excursion_worst,excursion_alpha_worst,outside,outside_worst"
    );

    // Per-artifact rows, for the diagnosis a summary cannot carry: `TESSERA_HULL_ROWS=1` prints one
    // line per artifact per resolution, which is how the worst-case columns below are traced back to
    // the artifact that produced them.
    let rows = std::env::var("TESSERA_HULL_ROWS").is_ok();
    if rows {
        println!("row,divisions,members,input,vertices,dig_ms,alpha,area,excursion,outside");
    }
    for &d in &divisions {
        let mut reduced = 0usize;
        let mut vertices = 0usize;
        let mut dig_ms = 0.0f64;
        let mut per_artifact: Vec<f64> = Vec::new();
        let mut alpha_drift: Vec<f64> = Vec::new();
        let mut areas: Vec<f64> = Vec::new();
        let mut excursions: Vec<f64> = Vec::new();
        let mut excursion_alpha: Vec<f64> = Vec::new();
        let mut outside_total = 0usize;
        let mut outside_worst = 0.0f64;
        for ((cloud, b), gather) in clouds.iter().zip(&base).zip(&gather_ms) {
            let t = Instant::now();
            let rings = tessera_engine::derived::dig_rings_at(cloud, usize::MAX, d).0;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            dig_ms += ms;
            per_artifact.push(ms + gather);
            vertices += rings.iter().map(|r| r.len()).sum::<usize>();

            // The input the shape was actually computed over, taken from the same helper the
            // engine digs over rather than recomputed here from the cell arithmetic.
            let members = representatives(cloud, d);
            reduced += members.len();

            let alpha = ring::alpha_sq(&members) as f64;
            alpha_drift.push((alpha / b.alpha_sq.max(1.0)).sqrt());
            areas.push(rings.iter().map(|r| double_area(r)).sum::<i128>() as f64 / b.area.max(1.0));
            let e = ring::excursion(&rings, &b.rings);
            excursions.push(e / b.extent);
            excursion_alpha.push(e / b.alpha_sq.max(1.0).sqrt());

            let outside = cloud
                .iter()
                .filter(|m| !rings.iter().any(|r| contains(r, **m)))
                .count();
            outside_total += outside;
            outside_worst = outside_worst.max(outside as f64 / cloud.len() as f64);
            if rows {
                println!(
                    "row,{d},{},{},{},{ms:.2},{:.4},{:.4},{:.5},{outside}",
                    cloud.len(),
                    members.len(),
                    rings.iter().map(|r| r.len()).sum::<usize>(),
                    alpha_drift[alpha_drift.len() - 1],
                    areas[areas.len() - 1],
                    excursions[excursions.len() - 1],
                );
            }
        }
        println!(
            "{d},{reduced},{vertices},{dig_ms:.0},{:.1},{:.1},{:.0},{:.4},{:.4},{:.4},{:.5},{:.5},{:.3},{outside_total},{:.5}",
            pct(&mut per_artifact.clone(), 0.5),
            pct(&mut per_artifact.clone(), 0.9),
            pct(&mut per_artifact.clone(), 1.0),
            worst_from_one(&mut alpha_drift.clone()),
            pct(&mut areas.clone(), 0.5),
            pct(&mut areas.clone(), 0.0),
            pct(&mut excursions.clone(), 0.5),
            pct(&mut excursions.clone(), 1.0),
            pct(&mut excursion_alpha.clone(), 1.0),
            outside_worst,
        );
    }
}

struct Base {
    ms: f64,
    area: f64,
    vertices: usize,
    alpha_sq: f64,
    extent: f64,
    rings: Vec<Vec<[u32; 2]>>,
}

/// The longer axis of a cloud's bounding box — the denominator that makes an excursion comparable
/// to a pixel, since a shape is drawn at most a viewport wide.
fn extent_of(cloud: &[[u32; 2]]) -> f64 {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for q in cloud {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
    (((x1 - x0) as f64).max((y1 - y0) as f64)).max(1.0)
}

/// The representatives the engine would dig over, for the α comparison.
fn representatives(cloud: &[[u32; 2]], divisions: u32) -> Vec<[u32; 2]> {
    tessera_engine::derived::quantised(cloud, divisions).unwrap_or_else(|| cloud.to_vec())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

/// The value furthest from 1 — how far a ratio that should not move has moved, in either direction.
fn worst_from_one(v: &mut [f64]) -> f64 {
    v.iter()
        .copied()
        .max_by(|a, b| {
            (a - 1.0)
                .abs()
                .partial_cmp(&(b - 1.0).abs())
                .expect("no NaN")
        })
        .unwrap_or(1.0)
}

/// **Where the derivation's time actually goes**, stage by stage over the whole layer — the
/// profile `docs/design/artifact-shapes.md` §7.1 quotes, and the thing that decides whether a
/// cheaper route to the occupied cells is worth building at all.
///
/// The stages are the ones a change can move independently: reading a position per visible member
/// (`gather`), the Akl–Toussaint pass that keeps α exact (`octagon`), binning the members to one
/// representative per cell (`bin`), and everything the shape itself costs over the reduced input
/// (`shape` — sort, wrap, group, dig). `octagon` is timed here on a second copy of the same eight
/// running maxima rather than through a seam, because it is one pass with no state to expose and a
/// seam for it would be a hole in the module for a measurement's convenience.
///
/// ```text
/// TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
///   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture the_derivation_profile
/// ```
#[test]
#[ignore]
fn the_derivation_profile() {
    let corpus = corpus::open();
    let d = tessera_engine::derived::SERVED_QUANTISE_DIVISIONS;
    let mut rows: Vec<Stage> = Vec::new();

    for (_, visible) in &corpus.memberships {
        let t0 = Instant::now();
        let positions = corpus::gather(visible, &corpus.locator);
        let gather_ms = t0.elapsed().as_secs_f64() * 1e3;
        if positions.len() < 3 {
            continue;
        }

        let t1 = Instant::now();
        let box_and_centroid = one_pass_box_and_centroid(&positions);
        let bc_ms = t1.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(box_and_centroid);

        let t2 = Instant::now();
        let reduced = tessera_engine::derived::quantised(&positions, d);
        let reduce_ms = t2.elapsed().as_secs_f64() * 1e3;
        let reduced_len = reduced.as_ref().map_or(positions.len(), |r| r.len());

        let t3 = Instant::now();
        let rings = tessera_engine::derived::dig_rings(&positions, usize::MAX).0;
        let shape_ms = t3.elapsed().as_secs_f64() * 1e3 - reduce_ms;
        std::hint::black_box(rings);

        // The four stages above are timed apart, so they double-count nothing and miss nothing a
        // stage boundary falls between. This is the whole derivation as a request pays for it —
        // gather, mean, extremes and shape in the order `compute` runs them, and the number the
        // stage rows have to add up to.
        let t4 = Instant::now();
        let derived = compute(
            &[
                ComputedProperty::Centroid,
                ComputedProperty::Box,
                ComputedProperty::Hull,
            ],
            visible,
            &corpus.locator,
        );
        let derive_ms = t4.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(derived);

        rows.push(Stage {
            members: positions.len(),
            representatives: reduced_len,
            gather_ms,
            bc_ms,
            reduce_ms,
            shape_ms: shape_ms.max(0.0),
            derive_ms,
        });
    }

    rows.sort_unstable_by_key(|r| r.members);
    println!("members,representatives,gather_ms,box_centroid_ms,reduce_ms,shape_ms,derive_ms");
    for r in &rows {
        println!(
            "{},{},{:.3},{:.3},{:.3},{:.3},{:.3}",
            r.members,
            r.representatives,
            r.gather_ms,
            r.bc_ms,
            r.reduce_ms,
            r.shape_ms,
            r.derive_ms
        );
    }
    let sum = |f: fn(&Stage) -> f64| -> f64 { rows.iter().map(f).sum() };
    let (g, o, b, s) = (
        sum(|r| r.gather_ms),
        sum(|r| r.bc_ms),
        sum(|r| r.reduce_ms),
        sum(|r| r.shape_ms),
    );
    let total = g + o + b + s;
    println!(
        "\nlayer: {} artifacts, {} members, {} representatives",
        rows.len(),
        rows.iter().map(|r| r.members).sum::<usize>(),
        rows.iter().map(|r| r.representatives).sum::<usize>(),
    );
    println!(
        "gather {g:.0} ms ({:.0}%)  box+centroid {o:.0} ms ({:.0}%)  reduce {b:.0} ms ({:.0}%)  shape {s:.0} ms ({:.0}%)  total {total:.0} ms",
        100.0 * g / total,
        100.0 * o / total,
        100.0 * b / total,
        100.0 * s / total,
    );
    println!(
        "the hull's own cost (reduce+shape) {:.0} ms; what a declared box or centroid already pays (gather) {g:.0} ms",
        b + s,
    );
    println!(
        "compute(centroid, box, hull) end to end: {:.0} ms over the layer",
        sum(|r| r.derive_ms)
    );
}

/// One artifact's row of the profile — the four stages a change can move independently.
struct Stage {
    members: usize,
    representatives: usize,
    gather_ms: f64,
    bc_ms: f64,
    reduce_ms: f64,
    shape_ms: f64,
    /// The whole of `compute` over the same membership, with all three properties declared.
    derive_ms: f64,
}

/// The `box` and `centroid` properties over the gathered positions, written from the definition —
/// the profile's floor, since a layer declaring either already pays a pass over every member and
/// the hull's own cost is what it adds on top.
fn one_pass_box_and_centroid(points: &[[u32; 2]]) -> ([u32; 4], [f64; 2]) {
    let mut b = [u32::MAX, u32::MAX, 0u32, 0u32];
    let (mut sx, mut sy) = (0.0f64, 0.0f64);
    for p in points {
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
        sx += p[0] as f64;
        sy += p[1] as f64;
    }
    let n = points.len() as f64;
    (b, [sx / n, sy / n])
}

/// **How many members share a cell** — the measurement `artifact-shapes.md` §7.3's crossover rests
/// on, and the reason the occupied cells are folded out of runs rather than jumped to.
///
/// A cell that is a contiguous row range could be reached by bitmap arithmetic — gallop over the
/// segment's Morton column to the cell's end, reset the mask's iterator past it, read one position
/// per cell instead of one per member — and that wins only where a cell holds more members than the
/// jump costs. So the question is occupancy, and it is answered at two resolutions: the corpus
/// grid's own cell, which is the finest a row range can address, and the binning resolution the
/// shape is actually computed at.
///
/// ```text
/// TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
///   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture the_cell_occupancy
/// ```
#[test]
#[ignore]
fn the_cell_occupancy() {
    use std::collections::HashSet;
    let corpus = corpus::open();
    let d = tessera_engine::derived::SERVED_QUANTISE_DIVISIONS;
    println!("members,extent,occupied_morton_cells,members_per_morton_cell,representatives,members_per_binning_cell");
    let (mut members, mut morton_cells, mut reps) = (0usize, 0usize, 0usize);
    let mut worst_binning = 0.0f64;
    for (_, visible) in &corpus.memberships {
        let positions = corpus::gather(visible, &corpus.locator);
        if positions.len() < 3 {
            continue;
        }
        // The corpus grid is 2^16 × 2^16 (`contracts.md` §2.5): a position's high half is its
        // Morton cell, and rows sharing one are the finest run a row range can be.
        let cells: HashSet<(u32, u32)> = positions.iter().map(|q| (q[0] >> 16, q[1] >> 16)).collect();
        let r = tessera_engine::derived::quantised(&positions, d).map_or(positions.len(), |v| v.len());
        let per_binning = positions.len() as f64 / r as f64;
        worst_binning = worst_binning.max(per_binning);
        members += positions.len();
        morton_cells += cells.len();
        reps += r;
        println!(
            "{},{:.0},{},{:.2},{},{:.2}",
            positions.len(),
            extent_of(&positions),
            cells.len(),
            positions.len() as f64 / cells.len() as f64,
            r,
            per_binning
        );
    }
    println!(
        "\nlayer: {members} members, {morton_cells} distinct Morton cells ({:.2} members a cell), {reps} representatives at {d} divisions ({:.2} members a cell, densest artifact {worst_binning:.1})",
        members as f64 / morton_cells as f64,
        members as f64 / reps as f64,
    );
    assert!(
        members as f64 / morton_cells as f64 > 1.0,
        "a member cannot share a cell with fewer than one member"
    );
}

/// **The reduction the engine computes is the reduction the definition asks for, member for
/// member, over every artifact of the layer** — the assertion that makes the route to the occupied
/// cells a speed change and not a shape change.
///
/// The engine finds the occupied cells by folding runs of consecutive members and answers both
/// hull-candidate questions per *cell*, over a band. This oracle does neither: it bins every member
/// into a hash map keyed on the cell, and tests every member against the octagon on its own. It
/// shares no code with the engine's reduction — only the resolution constant and the convex hull
/// in `common/ring.rs`, which is itself a second implementation.
///
/// Two things are asserted, and the second is the one that matters: the representative **sets** are
/// equal, and the **rings** the dig produces from each are byte-identical. A set difference that
/// happened not to move a vertex would still be caught by the first; a ring difference could not
/// hide behind either.
///
/// ```text
/// TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
///   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture the_reduction_is_the_definition
/// ```
#[test]
#[ignore]
fn the_reduction_is_the_definition() {
    let corpus = corpus::open();
    let d = tessera_engine::derived::SERVED_QUANTISE_DIVISIONS;
    let (mut reduced, mut exact, mut members, mut reps) = (0usize, 0usize, 0usize, 0usize);
    for (ordinal, visible) in &corpus.memberships {
        let positions = corpus::gather(visible, &corpus.locator);
        if positions.len() < 3 {
            continue;
        }
        members += positions.len();

        let engine = tessera_engine::derived::quantised(&positions, d);
        let oracle = dense_reduction(&positions, d);
        assert_eq!(
            engine.is_some(),
            oracle.is_some(),
            "artifact {ordinal}: the two routes disagree about whether the reduction engages"
        );

        let Some(oracle) = oracle else {
            exact += 1;
            continue;
        };
        let mut engine = engine.expect("both routes reduce");
        reduced += 1;

        let mut oracle = oracle;
        engine.sort_unstable();
        engine.dedup();
        oracle.sort_unstable();
        oracle.dedup();
        assert_eq!(
            engine, oracle,
            "artifact {ordinal}: the representatives and hull candidates are not the same set"
        );
        reps += engine.len();

        // The rings, from the engine's own route and from the oracle's set fed to a dig that
        // reduces nothing further. Unbounded, so the budget cannot mask a difference by stopping
        // both digs at the same vertex count.
        let by_engine = tessera_engine::derived::dig_rings_at(&positions, usize::MAX, d).0;
        let by_oracle = tessera_engine::derived::dig_rings_at(&oracle, usize::MAX, 0).0;
        assert_eq!(
            by_engine, by_oracle,
            "artifact {ordinal}: the rings are not byte-identical"
        );
    }
    println!(
        "{} artifacts reduced and {} exact, {members} members, {reps} representatives — every ring identical",
        reduced, exact
    );
    assert!(reduced > 0, "the layer must exercise the reduced path");
}

/// One member per occupied cell and every hull candidate beside them, **written from
/// `artifact-shapes.md` §7.1 rather than from the engine**: a hash map over every member for the
/// cells, an exact octagon over every member for the candidates, and no run, band or merge
/// anywhere. It is the oracle `the_reduction_is_the_definition` compares against, and it is
/// deliberately the slow, obvious construction.
fn dense_reduction(points: &[[u32; 2]], divisions: u32) -> Option<Vec<[u32; 2]>> {
    use std::collections::HashMap;
    const FLOOR: usize = 75_000;
    const MARGIN: f64 = 65_536.0;
    if divisions < 16 || points.len() < FLOOR {
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
        .div_ceil(divisions as u64)
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

    // Akl–Toussaint over every member, one at a time.
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
        edges.iter().all(|[a, b, c]| a * x + b * y + c > MARGIN)
    };

    let mut out: Vec<[u32; 2]> = cells.values().map(|(rep, _)| *rep).collect();
    for q in points {
        if !inside(*q) {
            out.push(*q);
        }
    }
    Some(out)
}
