//! **The measurement campaign behind the concave hull** — `docs/evidence/memos/2026-08-26-concave-hulls.md`.
//!
//! Ignored by default, because the corpus it reads is not in the repository. Run it as
//!
//! ```text
//! TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
//!   cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture
//! ```
//!
//! The convex hull here is a **second implementation**, written from the definition rather than
//! shared with the engine's: it is both the baseline the concave shape is timed against and the
//! oracle its containment is checked against on real memberships, and a baseline that called the
//! code under test would measure nothing. The properties themselves are tested in
//! `tessera_engine::derived`'s own tests and, through the serving path, in `artifact_content.rs`.

use croaring::Bitmap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tessera_engine::derived::{compute, ComputedProperty, RowLocator};
use tessera_store::read::{open_bundle, SegmentData};

#[path = "common/ring.rs"]
mod ring;
use ring::{contains, convex_hull, double_area};

#[test]
#[ignore]
fn measure_against_the_corpus() {
    let Ok(root) = std::env::var("TESSERA_HULL_BUNDLE") else {
        panic!("set TESSERA_HULL_BUNDLE to a bundle root (the directory holding CURRENT)");
    };
    let root = PathBuf::from(root);
    let layer_pack = std::env::var("TESSERA_HULL_PACK")
        .unwrap_or_else(|_| "partitions/default/members/members-000000-000.tsmb".into());
    let partition = std::env::var("TESSERA_HULL_PARTITION").unwrap_or_else(|_| "default".into());
    let view_name = std::env::var("TESSERA_HULL_VIEW").unwrap_or_else(|_| "s0".into());

    let bundle = open_bundle(&root).expect("bundle opens");
    // The pack path in the side-manifest is relative to the published prefix, which `CURRENT` names.
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = root.join(current["prefix"].as_str().expect("prefix"));
    let part = &bundle.partitions[&partition];
    let view = &part.views[&view_name];

    let row_bases: std::collections::HashMap<&str, u32> = view
        .row_space
        .extents()
        .iter()
        .map(|e| (e.seg_id.as_str(), e.row_base))
        .collect();
    let mut segments: Vec<(&SegmentData, u32)> = view
        .segments
        .iter()
        .map(|s| {
            let base = row_bases.get(s.seg_id.as_str()).copied().unwrap_or(0);
            (s.as_ref(), base)
        })
        .collect();
    segments.sort_unstable_by_key(|&(_, base)| base);
    let locator = RowLocator::new(segments);

    let pack = tessera_store::membership::MembershipPack::open(&prefix.join(Path::new(&layer_pack)))
        .expect("membership pack opens");

    let mut rows: Vec<Row> = Vec::new();
    for (ordinal, blob) in pack.iter() {
        if blob.is_empty() {
            continue;
        }
        let Some((record, _)) =
            tessera_lifecycle::membership::decode_record(tessera_types::EntityId::new(1), blob)
        else {
            continue;
        };
        let visible = view.row_space.project_base(&record.members);
        if visible.is_empty() {
            continue;
        }

        // The three costs, separated: reading a position per member is what a `box` already pays, so
        // it is the floor both hulls sit on and not part of either's own cost.
        let t0 = Instant::now();
        let positions = gather(&visible, &locator);
        let gather_ms = t0.elapsed().as_secs_f64() * 1e3;
        if positions.len() < 3 {
            continue;
        }

        let t1 = Instant::now();
        let wrap = convex_hull(&positions);
        let wrap_ms = t1.elapsed().as_secs_f64() * 1e3;

        let t2 = Instant::now();
        let shape = compute(&[ComputedProperty::Hull], &visible, &locator)
            .hull
            .expect("declared");
        let shape_ms = t2.elapsed().as_secs_f64() * 1e3 - gather_ms;

        // Containment against the whole membership, on real positions — the property the
        // construction rests on, checked where the memberships are not of our own making.
        assert!(
            positions.iter().all(|m| contains(&shape, *m)),
            "ordinal {ordinal}: a member fell outside its shape"
        );

        // A visual check is the point of the change, so the shapes themselves are dumpable: set
        // `TESSERA_HULL_DUMP` to a directory and each artifact's members, wrap and shape are written
        // there as CSV for whatever will draw them.
        if let Ok(dir) = std::env::var("TESSERA_HULL_DUMP") {
            let dump = |name: &str, pts: &[[u32; 2]]| {
                let body: String = pts.iter().map(|p| format!("{},{}\n", p[0], p[1])).collect();
                std::fs::write(format!("{dir}/{ordinal}-{name}.csv"), body).unwrap();
            };
            dump("wrap", &wrap);
            dump("shape", &shape);
            let stride = (positions.len() / 20_000).max(1);
            let thinned: Vec<[u32; 2]> = positions.iter().copied().step_by(stride).collect();
            dump("members", &thinned);
        }

        rows.push(Row {
            members: positions.len(),
            wrap: wrap.len(),
            shape: shape.len(),
            gather_ms,
            wrap_ms,
            shape_ms,
            ratio: double_area(&shape) as f64 / (double_area(&wrap) as f64).max(1.0),
        });
    }

    rows.sort_unstable_by_key(|r| r.members);
    println!("members,wrap_vertices,shape_vertices,gather_ms,wrap_ms,shape_ms,area_ratio");
    for r in &rows {
        println!(
            "{},{},{},{:.3},{:.3},{:.3},{:.4}",
            r.members, r.wrap, r.shape, r.gather_ms, r.wrap_ms, r.shape_ms, r.ratio
        );
    }

    let total_wrap: usize = rows.iter().map(|r| r.wrap).sum();
    let total_shape: usize = rows.iter().map(|r| r.shape).sum();
    let at_budget = rows.iter().filter(|r| r.shape >= r.wrap + 64).count();
    let undug = rows.iter().filter(|r| r.shape == r.wrap).count();
    println!(
        "\n{} artifacts, {} … {} members",
        rows.len(),
        rows[0].members,
        rows[rows.len() - 1].members
    );
    println!(
        "wire vertices {} → {} ({} → {} bytes at 8 per vertex, +{:.1}%); {at_budget} at the budget, {undug} not dug at all",
        total_wrap,
        total_shape,
        total_wrap * 8,
        total_shape * 8,
        100.0 * (total_shape as f64 / total_wrap as f64 - 1.0)
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
    members: usize,
    wrap: usize,
    shape: usize,
    gather_ms: f64,
    wrap_ms: f64,
    shape_ms: f64,
    ratio: f64,
}

/// The positions `compute` reads, gathered through the same locator.
fn gather(visible: &Bitmap, locator: &RowLocator<'_>) -> Vec<[u32; 2]> {
    visible
        .iter()
        .filter_map(|row| locator.position(row).map(|(x, y)| [x, y]))
        .collect()
}
