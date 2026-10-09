//! Artifact membership at scale: row-space bitmaps against an assignment column.
//!
//! Tier B of `probes/2026-08-15-artifact-representation/`. Tier A measured real
//! HDBSCAN membership over the real 2.42M UMAP corpus and found row-space
//! bitmaps 52x smaller than a dense column; that ratio is a function of members
//! *per artifact*, so it cannot be extrapolated. This generates clustered
//! geometry at 10^7..10^9 with the artifact counts the design actually assumes
//! and measures where the two curves cross.
//!
//! Geometry is synthetic but its shape is taken from Tier A: ~22% noise, a
//! skewed cluster-size distribution, and clusters compact in Morton order —
//! which is the only property either representation is sensitive to.
//!
//! Run: cargo run --release --example artifact_layers -p tessera-store -- \
//!        [--rows N] [--artifacts K] [--noise F] [--reps R]

use croaring::Bitmap;
use std::time::Instant;

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Assign each row to an artifact so that membership is contiguous in row space
/// apart from a noise fraction — the Tier A shape. Sizes are Zipf-ish: a few
/// large clusters and a long tail, matching the measured distribution.
fn assign(rows: u32, k: u32, noise: f64, seed: u64) -> Vec<i64> {
    let mut out = vec![-1i64; rows as usize];
    // Zipf-ish widths normalised to cover (1-noise) of the row space.
    let mut w: Vec<f64> = (1..=k).map(|i| 1.0 / f64::from(i).powf(0.7)).collect();
    let s: f64 = w.iter().sum();
    let target = f64::from(rows) * (1.0 - noise);
    for x in w.iter_mut() {
        *x = *x / s * target;
    }
    let mut st = seed | 1;
    let mut next = |m: u64| {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st % m.max(1)
    };
    // Assign spans in a SHUFFLED order, not Zipf-rank order along the row axis.
    // Laying the giants down first put every narrow viewport inside one of them,
    // so `artifacts in range` collapsed to a handful at 10^7 artifacts and the
    // per-artifact route looked structurally free. Measured on the campaign's own
    // real Tier A assignment, a 1% window holds 1.1-1.7x (frac x artifacts), so
    // sizes must interleave spatially. (Review round two, 2026-08-15.)
    let mut ord: Vec<usize> = (0..w.len()).collect();
    for i in (1..ord.len()).rev() {
        let j = (next(i as u64 + 1)) as usize;
        ord.swap(i, j);
    }
    let mut cursor = 0u64;
    for &c in ord.iter() {
        let width = &w[c];
        // each artifact owns a contiguous stretch, with the noise scattered through it
        let span = (*width / (1.0 - noise)).round() as u64;
        let end = (cursor + span).min(rows as u64);
        for r in cursor..end {
            if (next(1000) as f64) < noise * 1000.0 {
                continue; // noise: stays -1
            }
            out[r as usize] = c as i64;
        }
        cursor = end;
        if cursor >= rows as u64 {
            break;
        }
    }
    out
}

fn main() {
    let rows: u32 = arg("--rows", 100_000_000u32);
    let k: u32 = arg("--artifacts", 100_000u32);
    let noise: f64 = arg("--noise", 0.22f64);
    let reps: usize = arg("--reps", 3usize);

    eprintln!("rows={rows} artifacts={k} noise={noise}");
    let t0 = Instant::now();
    let lab = assign(rows, k, noise, 0x5eed_1234);
    let covered = lab.iter().filter(|x| **x >= 0).count();
    eprintln!(
        "assigned in {:?}; coverage {:.3}",
        t0.elapsed(),
        covered as f64 / rows as f64
    );

    // ---- representation 1: one row-space bitmap per artifact -----------------
    let t = Instant::now();
    let mut bms: Vec<Bitmap> = vec![Bitmap::new(); k as usize];
    for (r, c) in lab.iter().enumerate() {
        if *c >= 0 {
            bms[*c as usize].add(r as u32);
        }
    }
    let mut bm_bytes = 0usize;
    for b in bms.iter_mut() {
        b.run_optimize();
        bm_bytes += b.get_serialized_size_in_bytes::<croaring::Portable>();
    }
    let build_bm = t.elapsed();

    // ---- representation 2: dense assignment column ---------------------------
    let width = if k < 255 { 1 } else if k < 65535 { 2 } else { 4 };
    let col_bytes = rows as usize * width;
    let col: Vec<u32> = lab.iter().map(|c| (c + 1) as u32).collect();

    println!("\n--- size ---");
    println!("row-space bitmaps : {:>12} B  ({:.3} MB)", bm_bytes, bm_bytes as f64 / 1e6);
    println!("dense column (u{}) : {:>12} B  ({:.3} MB)", width * 8, col_bytes, col_bytes as f64 / 1e6);
    println!("column / bitmaps  : {:.2}x", col_bytes as f64 / bm_bytes as f64);
    println!("bytes per artifact: {:.1}", bm_bytes as f64 / k as f64);

    // ---- count cost: a viewport covering a fraction of row space -------------
    // mask = a 25% grant, scattered (the corpus's standard shape)
    let mut mask = Bitmap::new();
    let mut st = 0x1234_5678u64;
    for r in 0..rows {
        st ^= st << 13; st ^= st >> 7; st ^= st << 17;
        if st.is_multiple_of(4) { mask.add(r); }
    }
    mask.run_optimize();

    for &frac in &[1.0f64, 0.1, 0.01] {
        let hi = (rows as f64 * frac) as u32;
        let mut view = Bitmap::new();
        view.add_range(0u32..hi);
        let vis = mask.and(&view);
        let in_range: Vec<usize> = (0..k as usize)
            .filter(|c| !bms[*c].is_empty() && bms[*c].minimum().unwrap_or(u32::MAX) < hi)
            .collect();

        // route A: one and_cardinality per artifact in range
        let mut ta = std::time::Duration::ZERO;
        let mut sum_a = 0u64;
        for _ in 0..reps {
            let t = Instant::now();
            let mut s = 0u64;
            for c in &in_range {
                s += bms[*c].and_cardinality(&vis);
            }
            ta += t.elapsed();
            sum_a = s;
        }
        // route B: one pass over visible rows, accumulating into counters
        let mut tb = std::time::Duration::ZERO;
        let mut sum_b: u64 = 0;
        for _ in 0..reps {
            let t = Instant::now();
            let mut acc = vec![0u32; k as usize + 1];
            for r in vis.iter() {
                acc[col[r as usize] as usize] += 1;
            }
            tb += t.elapsed();
            sum_b = acc[1..].iter().map(|x| u64::from(*x)).sum::<u64>();
        }
        println!(
            "\nviewport {:>5.0}% : visible={:>10}  artifacts in range={:>8}",
            frac * 100.0, vis.cardinality(), in_range.len()
        );
        println!("  per-artifact bitmaps : {:>9.2} ms   (sum {})", ta.as_secs_f64() * 1e3 / reps as f64, sum_a);
        println!("  column scan          : {:>9.2} ms   (sum {})", tb.as_secs_f64() * 1e3 / reps as f64, sum_b);
    }
    eprintln!("bitmap build {:?}", build_bm);
}
