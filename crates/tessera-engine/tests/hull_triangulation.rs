//! **Ruling C's measurement: what a Rust Delaunay costs against the Rust dig**, on the same 197
//! memberships, on one machine, in one process — `docs/design/artifact-shapes.md` §8.
//!
//! The probe that raised the question could not answer it: there, Qhull is C and the dig is a numpy
//! loop, so its timing column compares implementations rather than algorithms. Here both sides are
//! release-mode Rust over the same gathered positions.
//!
//! It also measures the two shapes the ruling chooses between — the χ-peel over the triangulation,
//! and the α-components the multi-ring ruling asks the wire to carry — so that the vertex counts,
//! the ring counts and the wire bytes come from the same run as the timings.
//!
//! Ignored by default, because the corpus it reads is not in the repository. Run it as
//!
//! ```text
//! TESSERA_HULL_BUNDLE=<…>/bundle-notebook-2m4 \
//!   cargo test --release -p tessera-engine --test hull_triangulation -- --ignored --nocapture
//! ```

use delaunator::{next_halfedge, prev_halfedge, triangulate, Point, Triangulation, EMPTY};
use std::time::Instant;
use tessera_engine::derived::{compute, ComputedProperty};

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/ring.rs"]
mod ring;
use ring::{contains, convex_hull, double_area};

#[test]
#[ignore]
fn what_a_triangulation_costs_against_the_dig() {
    let corpus = corpus::open();
    let mut rows: Vec<Row> = Vec::new();

    for (ordinal, visible) in &corpus.memberships {
        let t0 = Instant::now();
        let positions = corpus::gather(visible, &corpus.locator);
        let gather_ms = ms(t0);
        if positions.len() < 3 {
            continue;
        }

        // The dig, through the serving path, with the gather it shares subtracted out.
        let t1 = Instant::now();
        // Every α-group is its own part on the wire; the comparison is over the rings.
        let dig: Vec<Vec<[u32; 2]>> = compute(&[ComputedProperty::Hull], visible, &corpus.locator)
            .shape
            .expect("declared")
            .into_iter()
            .flatten()
            .collect();
        let dig_ms = ms(t1) - gather_ms;

        // **Both alternatives start from the reduced input the dig now receives** — one real
        // member per occupied cell of the artifact's own grid, plus every member that could be a
        // convex-hull vertex (`tessera_engine::derived`'s `QUANTISE_DIVISIONS`). Handing the
        // triangulation the whole membership while the dig is given a reduction of it would
        // compare two constructions over two different clouds, and the question ruling A is asked
        // now is what each costs *on the input the service actually has*.
        //
        // The reduction and the sort are charged here, so the triangulated route pays them
        // explicitly and the dig pays them inside its own timing.
        let t2 = Instant::now();
        let mut p = tessera_engine::derived::quantised(
            &positions,
            tessera_engine::derived::SERVED_QUANTISE_DIVISIONS,
        )
        .unwrap_or_else(|| positions.clone());
        p.sort_unstable();
        p.dedup();
        let sort_ms = ms(t2);

        let alpha_sq = bridge_threshold(&convex_hull(&p));

        let t3 = Instant::now();
        let tri = delaunay(&p);
        let tri_ms = ms(t3);

        let t4 = Instant::now();
        let labels = components(&p, &tri, alpha_sq);
        let comp_ms = ms(t4);
        let component_count = labels.iter().copied().max().map(|m| m + 1).unwrap_or(0);

        let t5 = Instant::now();
        let rings = split_then_peel(&p, &tri, &labels, component_count, alpha_sq);
        let peel_ms = ms(t5);

        // The dependency-free alternative: grid connectivity at α, which never separates members
        // single-linkage would join and may join members further apart.
        let t6 = Instant::now();
        let grid = grid_components(&p, alpha_sq, 1);
        let grid_ms = ms(t6);
        let grid_count = grid.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let grid_refines = refines(&grid, &labels);
        let same_partition = grid_count == component_count && refines(&labels, &grid);
        let mut sweep = [0usize; 4];
        let mut sweep_same = [0usize; 4];
        for (k, r) in [1u64, 2, 3, 4].into_iter().enumerate() {
            let g = grid_components(&p, alpha_sq, r);
            let n = g.iter().copied().max().map(|m| m + 1).unwrap_or(0);
            sweep[k] = n;
            sweep_same[k] =
                usize::from(n == component_count && refines(&labels, &g) && refines(&g, &labels));
        }

        // Is a member ever inside a ring that is not its own? Measured, not assumed: nothing in the
        // construction forbids one component wrapping around another.
        let mut foreign = 0usize;
        if rings.len() > 1 {
            for (i, m) in p.iter().enumerate() {
                for (r, ring) in rings.iter().enumerate() {
                    if r != labels[i] && contains(ring, *m) {
                        foreign += 1;
                    }
                }
            }
        }

        // Every member inside the ring built from its own component, and inside no other. The
        // property the multi-ring wire is for, checked on memberships that are not of our making.
        for (i, m) in p.iter().enumerate() {
            let own = labels[i];
            assert!(
                contains(&rings[own], *m),
                "ordinal {ordinal}: member {m:?} fell outside its own component's ring"
            );
        }

        let vertices: usize = rings.iter().map(|r| r.len()).sum();
        let area: i128 = rings.iter().map(|r| double_area(r)).sum();
        rows.push(Row {
            members: p.len(),
            dig_vertices: dig.iter().map(|r| r.len()).sum(),
            dig_rings: dig.len(),
            rings: rings.len(),
            components: component_count,
            singletons: rings.iter().filter(|r| r.len() < 3).count(),
            vertices,
            gather_ms,
            dig_ms,
            sort_ms,
            tri_ms,
            comp_ms,
            peel_ms,
            grid_ms,
            grid_count,
            sweep,
            sweep_same,
            grid_refines,
            same_partition,
            foreign,
            wrap_area: double_area(&convex_hull(&p)) as f64,
            dig_area: dig.iter().map(|r| double_area(r)).sum::<i128>() as f64,
            peel_area: area as f64,
        });
    }

    rows.sort_unstable_by_key(|r| r.members);
    println!("members,dig_vertices,peel_vertices,rings,components,singleton_rings,gather_ms,dig_ms,sort_ms,delaunay_ms,components_ms,peel_ms,dig_area,peel_area");
    for r in &rows {
        println!(
            "{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.4},{:.4}",
            r.members,
            r.dig_vertices,
            r.vertices,
            r.rings,
            r.components,
            r.singletons,
            r.gather_ms,
            r.dig_ms,
            r.sort_ms,
            r.tri_ms,
            r.comp_ms,
            r.peel_ms,
            r.dig_area / r.wrap_area.max(1.0),
            r.peel_area / r.wrap_area.max(1.0),
        );
    }

    let sum = |f: fn(&Row) -> f64| rows.iter().map(f).sum::<f64>();
    let count = |f: fn(&Row) -> usize| rows.iter().map(f).sum::<usize>();
    println!(
        "\n{} artifacts, {} … {} distinct member positions",
        rows.len(),
        rows[0].members,
        rows[rows.len() - 1].members
    );
    println!(
        "whole layer: gather {:.0} ms, dig {:.0} ms | sort {:.0} ms, delaunay {:.0} ms, components {:.0} ms, peel {:.0} ms (triangulated route is {:.2}× the dig)",
        sum(|r| r.gather_ms),
        sum(|r| r.dig_ms),
        sum(|r| r.sort_ms),
        sum(|r| r.tri_ms),
        sum(|r| r.comp_ms),
        sum(|r| r.peel_ms),
        (sum(|r| r.tri_ms) + sum(|r| r.comp_ms) + sum(|r| r.peel_ms)) / sum(|r| r.dig_ms),
    );
    let big = &rows[rows.len() - 1];
    println!(
        "largest artifact: {} members — gather {:.0} ms, dig {:.0} ms | delaunay {:.0} ms, components {:.0} ms, peel {:.0} ms",
        big.members, big.gather_ms, big.dig_ms, big.tri_ms, big.comp_ms, big.peel_ms
    );

    // **The one-shape-per-request view, which is the question ruling A is now asked at.** With
    // `computed` on the wire the server derives a hull for the artifact the client draws and for
    // no other, so the layer sums above are the wrong denominator: what a viewer waits for is one
    // artifact's shape, and what rides the wire is one artifact's vertices. Reported as
    // percentiles over the 197 artifacts rather than as a total.
    let pct = |mut v: Vec<f64>, p: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
    };
    let dig_one: Vec<f64> = rows.iter().map(|r| r.gather_ms + r.dig_ms).collect();
    let tri_one: Vec<f64> = rows
        .iter()
        .map(|r| r.gather_ms + r.sort_ms + r.tri_ms + r.comp_ms + r.peel_ms)
        .collect();
    println!(
        "\none shape, gather included — the dig: p50 {:.1} ms, p90 {:.1} ms, worst {:.0} ms",
        pct(dig_one.clone(), 0.5),
        pct(dig_one.clone(), 0.9),
        pct(dig_one.clone(), 1.0),
    );
    println!(
        "one shape, gather included — triangulated: p50 {:.1} ms, p90 {:.1} ms, worst {:.0} ms",
        pct(tri_one.clone(), 0.5),
        pct(tri_one.clone(), 0.9),
        pct(tri_one.clone(), 1.0),
    );
    let ratio: Vec<f64> = rows
        .iter()
        .map(|r| (r.sort_ms + r.tri_ms + r.comp_ms + r.peel_ms) / r.dig_ms.max(1e-9))
        .collect();
    println!(
        "triangulated / dig, per artifact: p50 {:.1}×, p90 {:.1}×, worst {:.1}×",
        pct(ratio.clone(), 0.5),
        pct(ratio.clone(), 0.9),
        pct(ratio.clone(), 1.0),
    );
    let dig_bytes: Vec<f64> = rows
        .iter()
        .map(|r| (r.dig_vertices * 8 + r.dig_rings * 4) as f64)
        .collect();
    let peel_bytes: Vec<f64> = rows
        .iter()
        .map(|r| (r.vertices * 8 + r.rings * 4) as f64)
        .collect();
    println!(
        "one shape on the wire — the dig: p50 {:.0} B, worst {:.0} B; split-then-peel: p50 {:.0} B, worst {:.0} B",
        pct(dig_bytes.clone(), 0.5),
        pct(dig_bytes, 1.0),
        pct(peel_bytes.clone(), 0.5),
        pct(peel_bytes, 1.0),
    );
    println!(
        "the delaunay is {:.0}% of the triangulated route's own time over the layer",
        100.0 * sum(|r| r.tri_ms)
            / (sum(|r| r.sort_ms) + sum(|r| r.tri_ms) + sum(|r| r.comp_ms) + sum(|r| r.peel_ms)),
    );
    println!(
        "wire: dig {} vertices ({} B), split-then-peel {} vertices in {} rings ({} B at 8 per vertex + 4 per ring)",
        count(|r| r.dig_vertices),
        count(|r| r.dig_vertices) * 8,
        count(|r| r.vertices),
        count(|r| r.rings),
        count(|r| r.vertices) * 8 + count(|r| r.rings) * 4,
    );
    println!(
        "the dig, grouped: {} vertices in {} rings ({} B), {} artifacts with more than one ring",
        count(|r| r.dig_vertices),
        count(|r| r.dig_rings),
        count(|r| r.dig_vertices) * 8 + count(|r| r.dig_rings) * 4,
        rows.iter().filter(|r| r.dig_rings > 1).count(),
    );
    let multi = rows.iter().filter(|r| r.rings > 1).count();
    println!(
        "rings: {multi} of {} artifacts have more than one; {} singleton rings in total; largest ring count {}",
        rows.len(),
        count(|r| r.singletons),
        rows.iter().map(|r| r.rings).max().unwrap(),
    );
    println!(
        "grid connectivity at α: {} ms over the layer against {:.0} ms for the triangulated route; \
same partition on {} of {} artifacts, a sound coarsening on {} more, unsound on {}",
        sum(|r| r.grid_ms).round(),
        sum(|r| r.tri_ms) + sum(|r| r.comp_ms),
        rows.iter().filter(|r| r.same_partition).count(),
        rows.len(),
        rows.iter().filter(|r| !r.same_partition && r.grid_refines).count(),
        rows.iter().filter(|r| !r.grid_refines).count(),
    );
    println!(
        "grid rings: {} in total against {} exact; {} artifacts with more than one against {}",
        count(|r| r.grid_count),
        count(|r| r.components),
        rows.iter().filter(|r| r.grid_count > 1).count(),
        rows.iter().filter(|r| r.components > 1).count(),
    );
    for (k, r) in [1usize, 2, 3, 4].into_iter().enumerate() {
        println!(
            "  grid radius {r} (cell α/{r}, over-merge ≤ {:.2}α): {} rings, {} artifacts with more than one, exact on {} of {}",
            2f64.sqrt() * (r as f64 + 1.0) / r as f64,
            rows.iter().map(|x| x.sweep[k]).sum::<usize>(),
            rows.iter().filter(|x| x.sweep[k] > 1).count(),
            rows.iter().map(|x| x.sweep_same[k]).sum::<usize>(),
            rows.len(),
        );
    }
    println!(
        "members inside a ring that is not their own: {}",
        count(|r| r.foreign)
    );
    let mut ratios: Vec<f64> = rows
        .iter()
        .map(|r| r.peel_area / r.wrap_area.max(1.0))
        .collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "peel area / wrap: mean {:.3}, median {:.3}, min {:.3}",
        ratios.iter().sum::<f64>() / ratios.len() as f64,
        ratios[ratios.len() / 2],
        ratios[0],
    );
}

struct Row {
    members: usize,
    dig_vertices: usize,
    dig_rings: usize,
    rings: usize,
    components: usize,
    singletons: usize,
    vertices: usize,
    gather_ms: f64,
    dig_ms: f64,
    sort_ms: f64,
    tri_ms: f64,
    comp_ms: f64,
    peel_ms: f64,
    grid_ms: f64,
    grid_count: usize,
    sweep: [usize; 4],
    sweep_same: [usize; 4],
    grid_refines: bool,
    same_partition: bool,
    foreign: usize,
    wrap_area: f64,
    dig_area: f64,
    peel_area: f64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn delaunay(p: &[[u32; 2]]) -> Triangulation {
    let pts: Vec<Point> = p
        .iter()
        .map(|q| Point {
            x: q[0] as f64,
            y: q[1] as f64,
        })
        .collect();
    triangulate(&pts)
}

fn sq_len(a: [u32; 2], b: [u32; 2]) -> i128 {
    let (dx, dy) = (b[0] as i128 - a[0] as i128, b[1] as i128 - a[1] as i128);
    dx * dx + dy * dy
}

/// α², the engine's own: `(3 × the median edge of the convex wrap)²`.
fn bridge_threshold(convex: &[[u32; 2]]) -> i128 {
    if convex.len() < 2 {
        return 0;
    }
    let mut lengths: Vec<i128> = (0..convex.len())
        .map(|i| sq_len(convex[i], convex[(i + 1) % convex.len()]))
        .collect();
    lengths.sort_unstable();
    9 * lengths[lengths.len() / 2]
}

/// Single-linkage components at α, one label per point.
///
/// The Euclidean minimum spanning tree is a subgraph of the Delaunay triangulation, so cutting the
/// triangulation's edges longer than α gives exactly the single-linkage components at α — no
/// neighbour search, no grid, and no approximation.
fn components(p: &[[u32; 2]], tri: &Triangulation, alpha_sq: i128) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..p.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    };
    if tri.triangles.is_empty() {
        // Collinear: delaunator returns no triangles and the hull is every point along the line.
        for w in tri.hull.windows(2) {
            if sq_len(p[w[0]], p[w[1]]) <= alpha_sq {
                union(&mut parent, w[0], w[1]);
            }
        }
    } else {
        for e in 0..tri.triangles.len() {
            let (a, b) = (tri.triangles[e], tri.triangles[next_halfedge(e)]);
            if sq_len(p[a], p[b]) <= alpha_sq {
                union(&mut parent, a, b);
            }
        }
    }
    let mut label = vec![usize::MAX; p.len()];
    let mut next = 0usize;
    for i in 0..p.len() {
        let r = find(&mut parent, i);
        if label[r] == usize::MAX {
            label[r] = next;
            next += 1;
        }
        label[i] = label[r];
    }
    label
}

/// Whether every group of `coarse` is a union of groups of `fine` — that is, `coarse` never
/// separates two members `fine` puts together.
fn refines(coarse: &[usize], fine: &[usize]) -> bool {
    let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for i in 0..fine.len() {
        match map.entry(fine[i]) {
            std::collections::hash_map::Entry::Occupied(e) => {
                if *e.get() != coarse[i] {
                    return false;
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(coarse[i]);
            }
        }
    }
    true
}

/// Grid connectivity at α, without a triangulation: bucket the members into a square grid of side
/// α anchored at their own bounding box, and join members whose cells are equal or 8-adjacent.
///
/// A displacement of at most α moves a cell index by at most one along each axis, so **every pair
/// within α lands in the same group** — the grouping never separates members single-linkage at α
/// would join. It may join members up to 2√2·α apart, which is the safe direction: an over-merged
/// group draws the single ring the wire drew before, while an over-split one would claim a gap the
/// members do not have.
fn grid_components(p: &[[u32; 2]], alpha_sq: i128, r: u64) -> Vec<usize> {
    let side = ((alpha_sq as f64).sqrt() / r as f64).ceil().max(1.0) as u64;
    let (x0, y0) = p
        .iter()
        .fold((u32::MAX, u32::MAX), |(x, y), q| (x.min(q[0]), y.min(q[1])));
    let cell =
        |q: &[u32; 2]| -> (u64, u64) { ((q[0] - x0) as u64 / side, (q[1] - y0) as u64 / side) };
    let mut first: std::collections::HashMap<(u64, u64), usize> = std::collections::HashMap::new();
    for (i, q) in p.iter().enumerate() {
        first.entry(cell(q)).or_insert(i);
    }
    let mut parent: Vec<usize> = (0..p.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    };
    for (i, q) in p.iter().enumerate() {
        let c = cell(q);
        union(&mut parent, i, first[&c]);
        let r = r as i64;
        for dx in 0..=r {
            for dy in -r..=r {
                if dx == 0 && dy <= 0 {
                    continue;
                }
                let (nx, ny) = (c.0 as i64 + dx, c.1 as i64 + dy);
                if nx < 0 || ny < 0 {
                    continue;
                }
                if let Some(&j) = first.get(&(nx as u64, ny as u64)) {
                    union(&mut parent, i, j);
                }
            }
        }
    }
    let mut label = vec![usize::MAX; p.len()];
    let mut next = 0usize;
    for i in 0..p.len() {
        let r = find(&mut parent, i);
        if label[r] == usize::MAX {
            label[r] = next;
            next += 1;
        }
        label[i] = label[r];
    }
    label
}

/// One ring per α-component: the χ-peel over each component's own triangulation.
///
/// The single-component case — which is most of them — reuses the triangulation already computed,
/// so the common path triangulates once.
fn split_then_peel(
    p: &[[u32; 2]],
    tri: &Triangulation,
    labels: &[usize],
    components: usize,
    alpha_sq: i128,
) -> Vec<Vec<[u32; 2]>> {
    if components <= 1 {
        return vec![peel(p, tri, alpha_sq)];
    }
    let mut buckets: Vec<Vec<[u32; 2]>> = vec![Vec::new(); components];
    for (i, q) in p.iter().enumerate() {
        buckets[labels[i]].push(*q);
    }
    buckets
        .iter()
        .map(|members| {
            if members.len() < 3 {
                return members.clone();
            }
            let sub = delaunay(members);
            peel(members, &sub, alpha_sq)
        })
        .collect()
}

/// The χ-shape (Duckham et al.): peel the triangle behind the longest boundary edge above α, unless
/// its third vertex is already on the boundary.
///
/// That last clause is the whole construction. It keeps the boundary one simple cycle — a peel that
/// exposed an already-boundary vertex would pinch the ring — and it is why the χ-shape degrades to a
/// filament rather than to a hole. Every vertex is a member, because the triangulation's vertices
/// are the members and peeling only ever moves one from the interior to the boundary; every member
/// stays inside, because a removed triangle is empty of members by the triangulation's own property.
fn peel(p: &[[u32; 2]], tri: &Triangulation, alpha_sq: i128) -> Vec<[u32; 2]> {
    if p.len() < 3 || tri.triangles.is_empty() {
        return convex_hull(p);
    }
    let mut alive = vec![true; tri.triangles.len() / 3];
    let mut on_boundary = vec![false; p.len()];
    // A max-heap keyed on squared edge length, validated lazily: an entry whose halfedge has since
    // stopped bounding a live triangle is skipped rather than removed.
    let mut heap: std::collections::BinaryHeap<(i128, usize)> = std::collections::BinaryHeap::new();
    for e in 0..tri.triangles.len() {
        if tri.halfedges[e] == EMPTY {
            let (a, b) = (tri.triangles[e], tri.triangles[next_halfedge(e)]);
            on_boundary[a] = true;
            on_boundary[b] = true;
            let len = sq_len(p[a], p[b]);
            if len > alpha_sq {
                heap.push((len, e));
            }
        }
    }

    while let Some((_, e)) = heap.pop() {
        let t = e / 3;
        if !alive[t] {
            continue;
        }
        let twin = tri.halfedges[e];
        if twin != EMPTY && alive[twin / 3] {
            continue; // no longer a boundary edge
        }
        let third = tri.triangles[prev_halfedge(e)];
        if on_boundary[third] {
            continue;
        }
        alive[t] = false;
        on_boundary[third] = true;
        for h in [next_halfedge(e), prev_halfedge(e)] {
            let twin = tri.halfedges[h];
            if twin != EMPTY && alive[twin / 3] {
                let (a, b) = (tri.triangles[twin], tri.triangles[next_halfedge(twin)]);
                let l = sq_len(p[a], p[b]);
                if l > alpha_sq {
                    heap.push((l, twin));
                }
            }
        }
    }

    // The surviving boundary, as a successor map, walked into one cycle.
    let mut succ = vec![usize::MAX; p.len()];
    let mut start = usize::MAX;
    for e in 0..tri.triangles.len() {
        if !alive[e / 3] {
            continue;
        }
        let twin = tri.halfedges[e];
        if twin == EMPTY || !alive[twin / 3] {
            let (a, b) = (tri.triangles[e], tri.triangles[next_halfedge(e)]);
            succ[a] = b;
            start = a;
        }
    }
    let mut ring = Vec::new();
    let mut v = start;
    loop {
        ring.push(p[v]);
        v = succ[v];
        assert_ne!(v, usize::MAX, "the peeled boundary is not a cycle");
        if v == start {
            break;
        }
        assert!(
            ring.len() <= p.len(),
            "the peeled boundary revisits a vertex"
        );
    }
    // The engine's convention: counter-clockwise, starting at the lowest vertex, so a shape is a
    // vertex list and not a vertex list up to rotation.
    let at = ring
        .iter()
        .enumerate()
        .min_by_key(|(_, q)| **q)
        .map(|(i, _)| i)
        .unwrap();
    ring.rotate_left(at);
    ring
}
