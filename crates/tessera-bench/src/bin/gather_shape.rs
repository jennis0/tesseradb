//! **Spike: does gathering the scalar tail column-major beat row-major-then-transpose?**
//!
//! Not a shipped arm — a decision aid, run once to answer one question before a rewrite is
//! costed. `arms/gather.rs` measures the *read* and holds the output shape constant; this holds
//! the read constant and varies the output shape, which is the axis nothing has measured.
//!
//! # The two shapes
//!
//! **A — row-major, then transpose (today).** `Engine::viewport` builds one `PointOut` per row,
//! each owning a `Vec<ScalarOut>`, and `build_scalar_columns` then walks that once per column
//! demuxing into typed buffers. Two passes over the values, one heap allocation per *point*, and
//! for `utf8` two `String` allocations per value.
//!
//! **B — column-major (proposed).** One pass per column over the selected rows, straight into the
//! typed buffer the wire wants. No `PointOut`, no transpose.
//!
//! # What the measurement has to be honest about
//!
//! The suspicion this exists to test is that B trades allocation for **locality**: A touches every
//! column of one row while that row's cache lines are hot, where B re-walks the whole selected-row
//! list once per column. At 19 columns that is 19 sweeps over a scattered index list, and the
//! answer is not obvious from reading the code — which is why this is measured rather than
//! asserted.
//!
//! So the row set is **scattered and sorted**, matching what a real selection produces
//! (`Selection` sorts ascending by `tessera_id`, and the priority key is uncorrelated with row
//! order — see `arms/gather.rs`'s `Pattern::Scattered`). A contiguous row set would flatter B.
//!
//! **The outputs are compared, not just timed.** A shape that is faster because it dropped or
//! misaligned a column is the failure mode this whole change risks, so the run refuses rather
//! than reports if the two disagree.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin gather_shape -- <bundle> [rows] [reps]
//! ```

use std::time::Instant;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use tessera_store::read::{open_bundle, ScalarSlice, SegmentData};

/// One gathered value. Mirrors `tessera_engine::ScalarOut`, which is private to that crate.
#[derive(Debug, Clone, PartialEq)]
enum ScalarOut {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    TimestampUs(i64),
    Utf8(String),
}

/// One gathered column. Mirrors `tessera_server::viewer::ColumnBuf`.
#[derive(Debug, PartialEq)]
enum ColumnBuf {
    Bool(Vec<bool>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    TimestampUs(Vec<i64>),
    Utf8(Vec<String>),
}

/// Every flat member, so the three `match`es below cannot drift out of step.
macro_rules! flat_members {
    ($mac:ident) => {
        $mac! { U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, TimestampUs }
    };
}

struct Point {
    tessera_id: u64,
    code: u64,
    scalars: Vec<ScalarOut>,
}

/// Shape A, first half: exactly `viewport::row_to_point`.
fn row_to_point(segment: &SegmentData, row: u32, declared: &[String]) -> Point {
    let idx = row as usize;
    let cols = &segment.columns;
    let tessera_id = cols.tessera_id()[idx];
    let code = ((segment.morton.u32()[idx] as u64) << 32) | cols.residual()[idx] as u64;
    let mut scalars = Vec::with_capacity(declared.len());
    for name in declared {
        if let Some(value) = cols.scalar(name) {
            macro_rules! out {
                ($($v:ident),* $(,)?) => {
                    match value {
                        $(ScalarSlice::$v(s) => ScalarOut::$v(s[idx]),)*
                        ScalarSlice::Bool(a) => ScalarOut::Bool(a.value(idx)),
                        ScalarSlice::Utf8(a) => ScalarOut::Utf8(a.value(idx).to_string()),
                    }
                };
            }
            scalars.push(flat_members!(out));
        }
    }
    Point {
        tessera_id,
        code,
        scalars,
    }
}

/// Shape A, second half: exactly `viewer::build_scalar_columns`.
fn transpose(points: &[Point], n: usize) -> Vec<ColumnBuf> {
    let mut columns = Vec::with_capacity(n);
    for i in 0..n {
        macro_rules! column_of {
            ($variant:ident, $default:expr) => {
                ColumnBuf::$variant(
                    points
                        .iter()
                        .map(|p| match &p.scalars[i] {
                            ScalarOut::$variant(v) => v.clone(),
                            _ => $default,
                        })
                        .collect(),
                )
            };
        }
        let first = points.first().map(|p| &p.scalars[i]);
        columns.push(match first {
            Some(ScalarOut::Bool(_)) => column_of!(Bool, false),
            Some(ScalarOut::U8(_)) => column_of!(U8, 0),
            Some(ScalarOut::U16(_)) => column_of!(U16, 0),
            Some(ScalarOut::U32(_)) => column_of!(U32, 0),
            Some(ScalarOut::U64(_)) => column_of!(U64, 0),
            Some(ScalarOut::I8(_)) => column_of!(I8, 0),
            Some(ScalarOut::I16(_)) => column_of!(I16, 0),
            Some(ScalarOut::I32(_)) => column_of!(I32, 0),
            Some(ScalarOut::I64(_)) => column_of!(I64, 0),
            Some(ScalarOut::F32(_)) => column_of!(F32, 0.0),
            Some(ScalarOut::F64(_)) => column_of!(F64, 0.0),
            Some(ScalarOut::TimestampUs(_)) => column_of!(TimestampUs, 0),
            Some(ScalarOut::Utf8(_)) => column_of!(Utf8, String::new()),
            None => ColumnBuf::U8(Vec::new()),
        });
    }
    columns
}

/// Shape C: row-major still, but with the column lookup **hoisted out of the row loop**.
///
/// **This variant exists to stop the headline being a lie.** `row_to_point` calls
/// `cols.scalar(name)` once per column *per row* — a hash lookup and an Arrow downcast, 19 of them
/// for every point. Shape B avoids that as a side effect of its structure, so a bare A-vs-B
/// comparison credits the whole saving to column-major when much of it is just the repeated
/// lookup. C isolates the two: C-vs-A is what hoisting alone buys, and B-vs-C is what the
/// transpose and the per-point allocation actually cost.
fn gather_row_major_hoisted(
    segment: &SegmentData,
    rows: &[u32],
    declared: &[String],
) -> Vec<ColumnBuf> {
    let cols = &segment.columns;
    let slices: Vec<ScalarSlice> = declared.iter().filter_map(|n| cols.scalar(n)).collect();
    let points: Vec<Point> = rows
        .iter()
        .map(|&row| {
            let idx = row as usize;
            let mut scalars = Vec::with_capacity(slices.len());
            for value in &slices {
                macro_rules! out {
                    ($($v:ident),* $(,)?) => {
                        match value {
                            $(ScalarSlice::$v(s) => ScalarOut::$v(s[idx]),)*
                            ScalarSlice::Bool(a) => ScalarOut::Bool(a.value(idx)),
                            ScalarSlice::Utf8(a) => ScalarOut::Utf8(a.value(idx).to_string()),
                        }
                    };
                }
                scalars.push(flat_members!(out));
            }
            Point { tessera_id: cols.tessera_id()[idx], code: 0, scalars }
        })
        .collect();
    transpose(&points, slices.len())
}

/// Shape B: one pass per column, straight into the typed buffer.
///
/// **Keyed by declared name, not by position** — the audit's one binding condition. A column the
/// segment lacks yields an empty buffer rather than shifting every later column left.
fn gather_column_major(
    segment: &SegmentData,
    rows: &[u32],
    declared: &[String],
) -> (Vec<u64>, Vec<u64>, Vec<ColumnBuf>) {
    let cols = &segment.columns;
    let ids = cols.tessera_id();
    let residual = cols.residual();
    let morton = segment.morton.u32();

    let mut tessera_ids = Vec::with_capacity(rows.len());
    let mut codes = Vec::with_capacity(rows.len());
    for &row in rows {
        let idx = row as usize;
        tessera_ids.push(ids[idx]);
        codes.push(((morton[idx] as u64) << 32) | residual[idx] as u64);
    }

    let mut columns = Vec::with_capacity(declared.len());
    for name in declared {
        let Some(value) = cols.scalar(name) else {
            continue;
        };
        // The type match is hoisted OUT of the row loop — one branch per column instead of one
        // per value. That, not the allocation saving, is the structural advantage of this shape.
        macro_rules! fill {
            ($($v:ident),* $(,)?) => {
                match value {
                    $(ScalarSlice::$v(s) => {
                        let mut out = Vec::with_capacity(rows.len());
                        for &row in rows { out.push(s[row as usize]); }
                        ColumnBuf::$v(out)
                    })*
                    ScalarSlice::Bool(a) => {
                        let mut out = Vec::with_capacity(rows.len());
                        for &row in rows { out.push(a.value(row as usize)); }
                        ColumnBuf::Bool(out)
                    }
                    ScalarSlice::Utf8(a) => {
                        let mut out = Vec::with_capacity(rows.len());
                        for &row in rows { out.push(a.value(row as usize).to_string()); }
                        ColumnBuf::Utf8(out)
                    }
                }
            };
        }
        columns.push(flat_members!(fill));
    }
    (tessera_ids, codes, columns)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = args.next().ok_or("usage: gather_shape <bundle> [rows] [reps]")?;
    let want_rows: usize = args.next().unwrap_or_else(|| "200000".into()).parse()?;
    let reps: usize = args.next().unwrap_or_else(|| "5".into()).parse()?;
    // **The axis that decides whether this spike is valid.** The real gather runs once per tile,
    // not once per request: at a 1.02e6-mark viewport over 16,542 non-empty tiles that is ~62 rows
    // per call. Shape B resolves each column once *per call*, so its advantage amortises over the
    // batch — measuring one 10^6-row batch would flatter it by four orders of magnitude on that
    // fixed cost. `0` means one batch (the unrealistic upper bound, kept for comparison).
    let tile_rows: usize = args.next().unwrap_or_else(|| "0".into()).parse()?;

    let bundle = open_bundle(std::path::Path::new(&root))?;
    let declared: Vec<String> = bundle
        .manifest
        .declared_scalars
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let partition = bundle.partitions.values().next().ok_or("no partition")?;
    let slice = partition.slices.values().next().ok_or("no slice")?;
    let segment = slice.segments.first().ok_or("no segment")?;
    let total = segment.columns.row_count() as usize;
    let n = want_rows.min(total);

    // Scattered THEN SORTED: what `Selection` actually hands the gather. A contiguous run would
    // flatter the column-major shape, which is the shape under test — so it must not be used.
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    let mut rows: Vec<u32> = (0..total as u32).collect();
    rows.shuffle(&mut rng);
    rows.truncate(n);
    rows.sort_unstable();

    println!(
        "bundle {root}\n  segment rows {total}, gathering {n} scattered+sorted, {} declared columns, {reps} reps",
        declared.len()
    );
    let chunk = if tile_rows == 0 { n.max(1) } else { tile_rows };
    println!(
        "  batching: {} rows per call ({} calls) {}",
        chunk,
        n.div_ceil(chunk),
        if tile_rows == 0 { "— ONE batch, upper bound only" } else { "— models the per-tile gather" }
    );

    // Correctness first: a shape that is fast because it lost a column must not be reported.
    let points_once: Vec<Point> = rows.iter().map(|&r| row_to_point(segment, r, &declared)).collect();
    let a_cols = transpose(&points_once, declared.len());
    let (b_ids, b_codes, b_cols) = gather_column_major(segment, &rows, &declared);
    if a_cols != b_cols {
        return Err("the two shapes disagree — column-major is WRONG, not faster".into());
    }
    let a_ids: Vec<u64> = points_once.iter().map(|p| p.tessera_id).collect();
    let a_codes: Vec<u64> = points_once.iter().map(|p| p.code).collect();
    if a_ids != b_ids || a_codes != b_codes {
        return Err("identity or position columns disagree".into());
    }
    println!("  outputs identical ({} columns)\n", a_cols.len());
    drop(points_once);
    drop(a_cols);

    let mut c_total = Vec::new();
    let mut a_gather = Vec::new();
    let mut a_transpose = Vec::new();
    let mut b_total = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let points: Vec<Point> = rows
            .chunks(chunk)
            .flat_map(|c| c.iter().map(|&r| row_to_point(segment, r, &declared)))
            .collect();
        let g = t.elapsed();
        let t = Instant::now();
        let cols: Vec<Vec<ColumnBuf>> = rows
            .chunks(chunk)
            .scan(0usize, |off, c| {
                let s = &points[*off..*off + c.len()];
                *off += c.len();
                Some(transpose(s, declared.len()))
            })
            .collect();
        let x = t.elapsed();
        // Dropping is part of shape A's cost — one `Vec<ScalarOut>` per point has to be freed —
        // and is charged to neither clock in the live server, so it is timed separately here
        // rather than hidden.
        let t = Instant::now();
        drop(points);
        drop(cols);
        let d = t.elapsed();

        let t = Instant::now();
        let c: Vec<_> = rows.chunks(chunk).map(|c| gather_row_major_hoisted(segment, c, &declared)).collect();
        drop(c);
        let c_el = t.elapsed();

        let t = Instant::now();
        let out: Vec<_> = rows.chunks(chunk).map(|c| gather_column_major(segment, c, &declared)).collect();
        let b = t.elapsed();
        drop(out);
        c_total.push(c_el.as_secs_f64() * 1e3);

        a_gather.push(g.as_secs_f64() * 1e3);
        a_transpose.push((x + d).as_secs_f64() * 1e3);
        b_total.push(b.as_secs_f64() * 1e3);
    }

    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let ag = med(&mut a_gather);
    let at = med(&mut a_transpose);
    let bt = med(&mut b_total);
    let ct = med(&mut c_total);
    println!("  A row-major gather      {ag:8.1} ms");
    println!("  A transpose + drop      {at:8.1} ms");
    println!("  A total                 {:8.1} ms", ag + at);
    println!("  C row-major, hoisted    {ct:8.1} ms   (lookup out of the row loop; still transposes)");
    println!("  B column-major total    {bt:8.1} ms");
    println!("     of A's saving: hoisting the lookup {:.0}%, the shape change {:.0}%",
        100.0 * ((ag + at) - ct) / ((ag + at) - bt),
        100.0 * (ct - bt) / ((ag + at) - bt));
    println!(
        "  => B is {:.2}x {} ({:+.1} ms)",
        if bt < ag + at { (ag + at) / bt } else { bt / (ag + at) },
        if bt < ag + at { "faster" } else { "SLOWER" },
        bt - (ag + at)
    );
    Ok(())
}
